//! An app from a GitHub release.
//!
//! Every repo in the fleet publishes the same pair: a zip whose entries are
//! paths under `/mnt/us`, and a `.sha256` beside it. That is the whole bundle
//! format — the tree a checkout holds, packed.
//!
//! [`fetch`] takes the pair, checks the bundle against its sidecar, and unpacks
//! it under [`LibraryPaths::app_release_dir`], where [`super::discover`] reads
//! it exactly as it reads a repo. A release is what makes a version a thing
//! that exists apart from a working tree, and what puts the fleet on a machine
//! that never built any of it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::tree::{AppTree, discover_registrable};
use crate::library::paths::LibraryPaths;

/// GitHub's REST root.
const API: &str = "https://api.github.com";
/// GitHub refuses a request that names no client.
const CLIENT: &str = concat!("sidle/", env!("CARGO_PKG_VERSION"));
/// Metadata: one small JSON body.
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// A bundle runs to tens of megabytes, over whatever link this machine has.
const ASSET_TIMEOUT: Duration = Duration::from_secs(600);
/// Bound a runaway response, well over the largest bundle in the fleet.
const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;
/// The digest sidecar names one hash and nothing else.
const MAX_DIGEST_BYTES: u64 = 4096;
/// One release, however many assets it lists.
const MAX_JSON_BYTES: u64 = 1024 * 1024;
/// The characters a path component of the unpack directory may hold. GitHub
/// owner and repo names are drawn from these.
const SAFE: &[char] = &['.', '_', '-'];

/// `owner/repo`, checked for anything a path could take badly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repo {
    pub owner: String,
    pub repo: String,
}

impl std::fmt::Display for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

impl Repo {
    /// Read `owner/repo`, a `github.com/owner/repo`, or the URL either appears
    /// in. A component holding anything outside GitHub's own name charset is
    /// refused.
    pub fn parse(source: &str) -> Result<Repo> {
        let s = source.trim().trim_end_matches('/');
        let s = s
            .rsplit_once("github.com/")
            .map(|(_, rest)| rest)
            .unwrap_or(s);
        let s = s.strip_suffix(".git").unwrap_or(s);
        let Some((owner, repo)) = s.split_once('/') else {
            bail!("{source} is not an owner/repo — a GitHub source reads like `amazon/sprocket`");
        };
        if repo.contains('/') {
            bail!("{source} names more than an owner and a repo");
        }
        Ok(Repo {
            owner: component(owner, "owner")?,
            repo: component(repo, "repo")?,
        })
    }
}

/// One component of the unpack path. An unusable one is refused.
fn component(part: &str, what: &str) -> Result<String> {
    if part.is_empty() {
        bail!("empty {what}");
    }
    if part == "."
        || part == ".."
        || !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || SAFE.contains(&c))
    {
        bail!("{part} is not a usable {what} — letters, digits, `.`, `_` and `-` only");
    }
    Ok(part.to_string())
}

/// A tag as a directory name: every other character becomes `-`.
fn tag_dir(tag: &str) -> Result<String> {
    let safe: String = tag
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || SAFE.contains(&c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    component(&safe, "tag")
}

/// What [`fetch`] produced.
#[derive(Debug, Clone, Serialize)]
pub struct Fetched {
    pub repo: Repo,
    /// The release's own tag, as GitHub states it.
    pub tag: String,
    /// The asset the tree came out of. Empty for a tag already unpacked.
    pub bundle: String,
    /// The mount root the bundle unpacked to.
    pub root: PathBuf,
    /// Every app the bundle holds. One repo can publish several.
    #[serde(skip)]
    pub apps: Vec<AppTree>,
    /// Whether this call went to the network. A tag already unpacked is read
    /// off disk.
    pub downloaded: bool,
}

/// Take a release of `source` and unpack it under `paths`.
///
/// `tag` names a release; absent, it is the repo's latest. A tag already
/// unpacked is returned as it stands.
///
/// Each candidate is checked against its sidecar and unpacked, and the one
/// whose entries are a mount-rooted tree is kept. One candidate costs itself,
/// and a refusal names what each did.
pub fn fetch(paths: &LibraryPaths, source: &str, tag: Option<&str>) -> Result<Fetched> {
    let repo = Repo::parse(source)?;
    let client = client()?;
    let release = resolve(&client, &repo, tag)?;
    let dest = paths.app_release_dir(&repo.owner, &repo.repo, &tag_dir(&release.tag)?);

    if let Ok(apps) = discover_registrable(&dest) {
        return Ok(Fetched {
            repo,
            tag: release.tag,
            bundle: String::new(),
            root: apps[0].root.clone(),
            apps,
            downloaded: false,
        });
    }

    let mut refused = Vec::new();
    for candidate in &release.candidates {
        match take(&client, candidate, &dest) {
            Ok(apps) => {
                return Ok(Fetched {
                    repo,
                    tag: release.tag,
                    bundle: candidate.bundle_name.clone(),
                    root: apps[0].root.clone(),
                    apps,
                    downloaded: true,
                });
            }
            Err(e) => refused.push(format!("{}: {e:#}", candidate.bundle_name)),
        }
    }
    bail!(
        "no asset of {repo} {} is an app bundle — {}",
        release.tag,
        refused.join("; ")
    )
}

/// Fetch one candidate, check it, and make it `dest` if it holds a
/// mount-rooted tree. Nothing at `dest` moves for a candidate that does not.
fn take(
    client: &reqwest::blocking::Client,
    candidate: &Candidate,
    dest: &Path,
) -> Result<Vec<AppTree>> {
    let bytes = fetch_verified(client, candidate)?;
    let staging = stage(&bytes, dest).context("unpack it")?;
    if let Err(e) = discover_registrable(&staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    promote(&staging, dest)?;
    discover_registrable(dest)
}

/// One candidate's bytes, checked against the sha256 the release declares for
/// them. The caller names the asset every message here is about.
fn fetch_verified(client: &reqwest::blocking::Client, candidate: &Candidate) -> Result<Vec<u8>> {
    let bytes = get(
        client,
        &candidate.bundle_url,
        ASSET_TIMEOUT,
        MAX_ASSET_BYTES,
    )
    .context("download it")?;
    let sidecar = get(client, &candidate.digest_url, API_TIMEOUT, MAX_DIGEST_BYTES)
        .context("download its sha256 sidecar")?;
    let declared = declared_digest(&sidecar).context("read its sha256 sidecar")?;
    let actual = sha256_hex(&bytes);
    if actual != declared {
        bail!("got sha256 {actual}, the release declares {declared}");
    }
    Ok(bytes)
}

/// One release, and every zip in it carrying a sha256 sidecar.
struct Release {
    tag: String,
    candidates: Vec<Candidate>,
}

/// A `<name>.zip` and the `<name>.zip.sha256` beside it.
struct Candidate {
    bundle_name: String,
    bundle_url: String,
    digest_url: String,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    /// The API's own handle on the bytes, which serves a private repo's assets
    /// to a token the browser URL would turn away.
    url: String,
}

/// A client carrying what GitHub asks of one, and a token when the environment
/// holds one.
fn client() -> Result<reqwest::blocking::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static(CLIENT),
    );
    if let Some(token) = token() {
        let mut value = reqwest::header::HeaderValue::try_from(format!("Bearer {token}"))
            .context("build the Authorization header")?;
        value.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    reqwest::blocking::Client::builder()
        .default_headers(headers)
        .build()
        .context("build the GitHub client")
}

/// The token `gh` itself reads, for a private repo and for a rate limit worth
/// having.
fn token() -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .filter(|t| !t.trim().is_empty())
}

/// The release `tag` names, or the repo's latest, and the candidates in it.
fn resolve(client: &reqwest::blocking::Client, repo: &Repo, tag: Option<&str>) -> Result<Release> {
    let url = match tag {
        Some(tag) => format!("{API}/repos/{repo}/releases/tags/{tag}"),
        None => format!("{API}/repos/{repo}/releases/latest"),
    };
    let res = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(API_TIMEOUT)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = res.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        match tag {
            Some(tag) => bail!("{repo} publishes no release tagged {tag}"),
            None => bail!(
                "{repo} publishes no release, or is private to a token this machine does not hold"
            ),
        }
    }
    if !status.is_success() {
        bail!("GET {url} — {status}");
    }
    let mut body = Vec::new();
    res.take(MAX_JSON_BYTES)
        .read_to_end(&mut body)
        .with_context(|| format!("read the body of {url}"))?;
    let release: ApiRelease =
        serde_json::from_slice(&body).with_context(|| format!("parse {url}"))?;
    let candidates = candidates(&release.assets);
    if candidates.is_empty() {
        bail!(
            "{} offers no <name>.zip with a <name>.zip.sha256 beside it — it holds {}",
            release.tag_name,
            asset_names(&release.assets)
        );
    }
    Ok(Release {
        tag: release.tag_name,
        candidates,
    })
}

/// Every `<name>.zip` carrying a `<name>.zip.sha256` beside it, in the order
/// the release lists them.
fn candidates(assets: &[ApiAsset]) -> Vec<Candidate> {
    assets
        .iter()
        .filter(|a| a.name.ends_with(".zip"))
        .filter_map(|zip| {
            let want = format!("{}.sha256", zip.name);
            let digest = assets.iter().find(|a| a.name == want)?;
            Some(Candidate {
                bundle_name: zip.name.clone(),
                bundle_url: zip.url.clone(),
                digest_url: digest.url.clone(),
            })
        })
        .collect()
}

fn asset_names(assets: &[ApiAsset]) -> String {
    if assets.is_empty() {
        return "no assets at all".to_string();
    }
    assets
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// One asset's bytes, capped.
fn get(
    client: &reqwest::blocking::Client,
    url: &str,
    timeout: Duration,
    max: u64,
) -> Result<Vec<u8>> {
    let res = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .timeout(timeout)
        .send()
        .with_context(|| format!("GET {url}"))?;
    let status = res.status();
    if !status.is_success() {
        bail!("GET {url} — {status}");
    }
    let mut bytes = Vec::new();
    res.take(max)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read the body of {url}"))?;
    Ok(bytes)
}

/// The hash a `sha256sum`-style sidecar declares: its first token.
fn declared_digest(sidecar: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(sidecar).context("the sidecar is not text")?;
    let token = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("the sidecar names no sha256");
    }
    Ok(token)
}

/// Hex sha256, the form the sidecars and the manifest both carry.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Write every entry of `bundle` into a sibling `.partial` of `dest`, and
/// return where it landed. [`promote`] is what makes a staged tree `dest`, and
/// an unpack that reaches neither step leaves `dest` as it was.
fn stage(bundle: &[u8], dest: &Path) -> Result<PathBuf> {
    let staging = staging_dir(dest)?;
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
    write_entries(bundle, &staging).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&staging);
    })?;
    Ok(staging)
}

/// Replace `dest` with `staging`, in one `rename`.
fn promote(staging: &Path, dest: &Path) -> Result<()> {
    let _ = std::fs::remove_dir_all(dest);
    std::fs::rename(staging, dest)
        .with_context(|| format!("rename {} -> {}", staging.display(), dest.display()))
}

/// Where a bundle unpacks before it is `dest`.
fn staging_dir(dest: &Path) -> Result<PathBuf> {
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", dest.display()))?;
    let name = dest
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} names no directory", dest.display()))?;
    Ok(parent.join(format!("{}.partial", name.to_string_lossy())))
}

fn write_entries(bundle: &[u8], into: &Path) -> Result<usize> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bundle)).context("read the bundle as a zip")?;
    let mut written = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("zip entry {i}"))?;
        if entry.is_dir() {
            continue;
        }
        // `enclosed_name` is the crate's own refusal of an absolute path and of
        // anything reaching above the directory it unpacks into.
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| anyhow::anyhow!("{} escapes the bundle", entry.name()))?;
        let path = into.join(&rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let mut out =
            std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
        std::io::copy(&mut entry, &mut out).with_context(|| format!("write {}", path.display()))?;

        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = out.set_permissions(std::fs::Permissions::from_mode(mode));
        }
        // The build time the bundle carries. `built_at_of` reads it wherever no
        // `.build-ts` sidecar stands beside the file.
        if let Some(stamp) = entry.last_modified().and_then(as_system_time) {
            let _ = out.set_modified(stamp);
        }
        written += 1;
    }
    Ok(written)
}

/// A zip's DOS timestamp as a wall-clock instant. The format carries no zone,
/// and this reads it as UTC.
fn as_system_time(dt: zip::DateTime) -> Option<SystemTime> {
    let date =
        chrono::NaiveDate::from_ymd_opt(dt.year() as i32, dt.month() as u32, dt.day() as u32)?;
    let naive = date.and_hms_opt(dt.hour() as u32, dt.minute() as u32, dt.second() as u32)?;
    let secs = naive.and_utc().timestamp();
    u64::try_from(secs)
        .ok()
        .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_source_reads_as_owner_and_repo_however_it_is_written() {
        let want = Repo {
            owner: "acme".into(),
            repo: "sprocket".into(),
        };
        assert_eq!(Repo::parse("acme/sprocket").unwrap(), want);
        assert_eq!(Repo::parse("  acme/sprocket/ ").unwrap(), want);
        assert_eq!(Repo::parse("github.com/acme/sprocket").unwrap(), want);
        assert_eq!(
            Repo::parse("https://github.com/acme/sprocket.git").unwrap(),
            want
        );
        assert_eq!(want.to_string(), "acme/sprocket");
    }

    #[test]
    fn a_source_that_could_reach_out_of_the_unpack_directory_is_refused() {
        for bad in [
            "../etc",
            "acme/..",
            "../../acme/sprocket",
            "acme/kar yll",
            "acme",
            "acme/sprocket/extra",
            "/sprocket",
        ] {
            assert!(Repo::parse(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn a_tag_becomes_one_path_component() {
        assert_eq!(tag_dir("v0.4.0").unwrap(), "v0.4.0");
        assert_eq!(tag_dir("release/2026-08").unwrap(), "release-2026-08");
        assert!(tag_dir("..").is_err());
        assert!(tag_dir("").is_err());
    }

    #[test]
    fn a_sidecar_names_one_hash_whatever_follows_it() {
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            declared_digest(format!("{hash}  sprocket.zip\n").as_bytes()).unwrap(),
            hash
        );
        assert_eq!(
            declared_digest(hash.to_ascii_uppercase().as_bytes()).unwrap(),
            hash
        );
        assert!(declared_digest(b"not a hash").is_err());
        assert!(declared_digest(b"").is_err());
        assert!(declared_digest(&[0xff, 0xfe]).is_err());
    }

    /// A zip whose entries are paths under `/mnt/us` — the bundle format.
    fn bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut out));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .unix_permissions(0o755)
                .last_modified_time(
                    zip::DateTime::from_date_and_time(2026, 8, 3, 4, 5, 6).unwrap(),
                );
            for (name, bytes) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(bytes).unwrap();
            }
            w.finish().unwrap();
        }
        out
    }

    /// [`fetch`]'s two filesystem steps, with the network between them left out.
    fn unpack(zip: &[u8], dest: &Path) -> Result<()> {
        let staging = stage(zip, dest)?;
        promote(&staging, dest)
    }

    #[test]
    fn a_bundle_unpacks_to_the_tree_a_checkout_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("acme/sprocket/v0.4.0");
        let zip = bundle(&[
            ("extensions/sprocket/bin/sprocket", b"armhf"),
            ("extensions/sprocket/hid/config.ini", b"[device]\n"),
            (
                "documents/Sprocket.sh",
                b"#!/bin/sh\n# Name: Sprocket\nexec /mnt/us/extensions/sprocket/bin/sprocket\n",
            ),
        ]);

        unpack(&zip, &dest).unwrap();

        let apps = discover_registrable(&dest).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].app.id, "sprocket");
        assert_eq!(apps[0].files.len(), 3, "the tile counts as the app's");
        assert_eq!(
            apps[0].built_at(),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 3)
                .unwrap()
                .and_hms_opt(4, 5, 6)
                .unwrap()
                .and_utc()
                .timestamp() as u64,
            "the bundle's own build time survives the unpack"
        );
        assert!(!staging_dir(&dest).unwrap().exists());
    }

    #[test]
    fn an_entry_reaching_above_the_unpack_directory_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("repo/v1");
        let zip = bundle(&[("../../escaped.txt", b"nope")]);

        assert!(unpack(&zip, &dest).is_err());
        assert!(!dest.exists(), "a refused bundle leaves no tree");
        assert!(!tmp.path().join("escaped.txt").exists());
        assert!(!staging_dir(&dest).unwrap().exists());
    }

    /// An asset that is not a mount-rooted tree is read in staging and dropped,
    /// which is what lets a release carry one beside its bundle.
    #[test]
    fn a_staged_tree_that_is_no_app_never_becomes_the_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("repo/v1");
        unpack(&bundle(&[("extensions/gadget/bin/gadget", b"v1")]), &dest).unwrap();

        let staging = stage(&bundle(&[("reader-plugin/main.lua", b"-- lua")]), &dest).unwrap();

        assert!(discover_registrable(&staging).is_err());
        assert!(
            dest.join("extensions/gadget/bin/gadget").exists(),
            "the tree already here stands until a bundle replaces it"
        );
        std::fs::remove_dir_all(&staging).unwrap();
    }

    /// A second unpack of the same tag replaces the tree; nothing of the first
    /// survives it.
    #[test]
    fn unpacking_over_a_tag_leaves_only_what_the_bundle_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("repo/v1");
        unpack(&bundle(&[("extensions/gadget/bin/gadget", b"v1")]), &dest).unwrap();
        std::fs::write(dest.join("extensions/gadget/stray"), b"left over").unwrap();

        unpack(&bundle(&[("extensions/gadget/bin/gadget", b"v2")]), &dest).unwrap();

        assert_eq!(
            std::fs::read(dest.join("extensions/gadget/bin/gadget")).unwrap(),
            b"v2"
        );
        assert!(!dest.join("extensions/gadget/stray").exists());
    }
}
