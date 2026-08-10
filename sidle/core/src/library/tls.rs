//! TLS material for the LAN server: a private CA generated once, and a server
//! leaf re-issued for whatever addresses the machine currently answers on.
//!
//! # Why a private CA rather than a public one
//!
//! The system is closed: one server, a handful of Kindles, and the desktop app.
//! A publicly-trusted certificate would need a real domain, DNS-01 renewals
//! every 90 days, and a device clock accurate enough to accept it after an
//! arbitrarily long sleep. None of that buys anything here, because there is no
//! third party who needs to verify us.
//!
//! Pinning our own CA is the stronger position, not the weaker one: the picker
//! trusts exactly one root — this one — so no public CA can mint a certificate
//! it will accept. The device client is built without the Mozilla root set
//! compiled in at all, which makes that structural rather than a policy.
//!
//! # Why the leaf is re-issued rather than long-lived
//!
//! The picker reaches the server by address, and that address can move (DHCP).
//! A leaf therefore carries the current address as a SAN and is re-issued when
//! it changes. This adds no fragility that isn't already there: the same move
//! already invalidates `HOST=` in the device's `server.conf`, and both are
//! rewritten by the same deploy.

use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType, date_time_ymd,
};

use super::LibraryPaths;

/// How long the CA is good for. Long, because rotating it means re-deploying to
/// every device — and unlike a public CA there is no revocation ecosystem whose
/// expectations we have to meet. A regenerate action exists for the case where
/// the key is believed compromised.
const CA_VALID_YEARS: i32 = 10;

/// How long a server leaf is good for. Shorter than the CA, but still long
/// enough that expiry is never the reason a sync fails — the leaf is re-issued
/// on address changes anyway, which is the event that actually matters.
const LEAF_VALID_YEARS: i32 = 5;

/// Backdate `not_before` by this much. A Kindle that has been asleep can carry a
/// clock that drifted behind real time, and a certificate that isn't valid yet
/// fails exactly as hard as one that expired. Costs nothing to avoid.
const BACKDATE_DAYS: i64 = 30;

/// The CA certificate PEM — what the device pins. Read from disk so callers
/// don't have to know the layout.
pub fn ca_cert_pem(paths: &LibraryPaths) -> Result<String> {
    std::fs::read_to_string(paths.ca_cert())
        .with_context(|| format!("read CA cert {}", paths.ca_cert().display()))
}

/// Generate the CA if it isn't there yet. Idempotent: an existing CA is left
/// exactly as-is, because regenerating it would silently invalidate every
/// device already carrying the old one.
///
/// Returns `true` if a CA was created by this call.
pub fn ensure_ca(paths: &LibraryPaths) -> Result<bool> {
    if paths.ca_cert().exists() && paths.ca_key().exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(paths.tls_dir()).context("create tls dir")?;

    let key = KeyPair::generate().context("generate CA key")?;
    let params = ca_params()?;
    let cert = params.self_signed(&key).context("self-sign CA")?;

    write_public(&paths.ca_cert(), &cert.pem())?;
    write_private(&paths.ca_key(), &key.serialize_pem())?;
    Ok(true)
}

/// Issue (or re-issue) the server leaf covering `addrs` — the addresses clients
/// will actually connect to, as they appear in a URL. An entry that parses as an
/// IP becomes an IP SAN; anything else becomes a DNS SAN.
///
/// Always writes, rather than checking whether the existing leaf already covers
/// `addrs`: re-issuing is cheap, and "the cert on disk matches what we just
/// asked for" is a much easier property to reason about than a staleness test.
pub fn issue_server_cert(paths: &LibraryPaths, addrs: &[String]) -> Result<()> {
    anyhow::ensure!(
        !addrs.is_empty(),
        "a server certificate needs at least one address to cover"
    );
    ensure_ca(paths)?;

    let ca_key_pem = std::fs::read_to_string(paths.ca_key())
        .with_context(|| format!("read CA key {}", paths.ca_key().display()))?;
    let ca_key = KeyPair::from_pem(&ca_key_pem).context("parse CA key")?;
    let issuer =
        Issuer::from_ca_cert_pem(&ca_cert_pem(paths)?, ca_key).context("load CA as issuer")?;

    let leaf_key = KeyPair::generate().context("generate leaf key")?;
    let params = leaf_params(addrs)?;
    let cert = params
        .signed_by(&leaf_key, &issuer)
        .context("sign server leaf")?;

    write_public(&paths.server_cert(), &cert.pem())?;
    write_private(&paths.server_key(), &leaf_key.serialize_pem())?;
    Ok(())
}

fn ca_params() -> Result<CertificateParams> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "sidle local CA");
    params.distinguished_name = dn;
    set_validity(&mut params, CA_VALID_YEARS);
    Ok(params)
}

fn leaf_params(addrs: &[String]) -> Result<CertificateParams> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.subject_alt_names = addrs.iter().map(|a| san_for(a)).collect::<Result<_>>()?;

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "sidle-server");
    params.distinguished_name = dn;
    set_validity(&mut params, LEAF_VALID_YEARS);
    Ok(params)
}

/// An address as it appears in a URL becomes the matching SAN type. Done
/// explicitly rather than via `CertificateParams::new`, which infers the same
/// split but swallows the reason a name was rejected.
fn san_for(addr: &str) -> Result<SanType> {
    match addr.parse::<IpAddr>() {
        Ok(ip) => Ok(SanType::IpAddress(ip)),
        Err(_) => Ok(SanType::DnsName(addr.to_string().try_into().with_context(
            || format!("{addr:?} is neither an IP nor a usable DNS name"),
        )?)),
    }
}

/// Set the validity window: backdated per [`BACKDATE_DAYS`], expiring `years`
/// from today.
///
/// Takes `&mut params` rather than returning the pair so this module never has
/// to name rcgen's date type, which would mean a direct `time` dependency for
/// nothing.
fn set_validity(params: &mut CertificateParams, years: i32) {
    use chrono::Datelike as _;

    let today = chrono::Utc::now().date_naive();
    let from = today - chrono::Duration::days(BACKDATE_DAYS);
    // `with_year` is None only for Feb 29 landing on a non-leap year; stepping
    // back a day keeps the same meaning and always resolves.
    let to = today
        .with_year(today.year() + years)
        .or_else(|| (today - chrono::Duration::days(1)).with_year(today.year() + years))
        .unwrap_or(today);

    params.not_before = date_time_ymd(from.year(), from.month() as u8, from.day() as u8);
    params.not_after = date_time_ymd(to.year(), to.month() as u8, to.day() as u8);
}

/// Certificates are public material — readable is fine, and the server may run
/// as a different session than the app that wrote them.
fn write_public(path: &Path, pem: &str) -> Result<()> {
    std::fs::write(path, pem).with_context(|| format!("write {}", path.display()))
}

/// Private keys are 0600, same as `.server-token`.
fn write_private(path: &Path, pem: &str) -> Result<()> {
    std::fs::write(path, pem).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 600 {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, LibraryPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: tmp.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        (tmp, paths)
    }

    #[test]
    fn ensure_ca_is_idempotent() {
        let (_tmp, paths) = paths();
        assert!(ensure_ca(&paths).unwrap(), "first call should create");
        let pem = ca_cert_pem(&paths).unwrap();
        assert!(!ensure_ca(&paths).unwrap(), "second call must not recreate");
        assert_eq!(
            pem,
            ca_cert_pem(&paths).unwrap(),
            "an existing CA must be left byte-identical — regenerating it would \
             silently orphan every device already carrying the old one"
        );
    }

    /// Both SAN kinds must land, so an IP today and a hostname (or a tailnet
    /// name) later work through one code path.
    ///
    /// That the leaf actually *chains* to the CA is proved where it matters, by
    /// a real handshake in `sidle-server` — parsing the bytes here would only
    /// restate what rcgen was asked to do.
    #[test]
    fn leaf_carries_both_ip_and_dns_sans() {
        let (_tmp, paths) = paths();
        issue_server_cert(
            &paths,
            &["192.168.1.42".to_string(), "mac.local".to_string()],
        )
        .unwrap();

        let pem = std::fs::read_to_string(paths.server_cert()).unwrap();
        let (_, der) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
        let (_, leaf) = x509_parser::parse_x509_certificate(&der.contents).unwrap();

        let san = leaf
            .subject_alternative_name()
            .unwrap()
            .expect("leaf has a SAN extension");
        let mut ips = vec![];
        let mut dns = vec![];
        for name in &san.value.general_names {
            match name {
                x509_parser::extensions::GeneralName::IPAddress(b) => ips.push(b.to_vec()),
                x509_parser::extensions::GeneralName::DNSName(s) => dns.push(s.to_string()),
                _ => {}
            }
        }
        assert_eq!(ips, vec![vec![192u8, 168, 1, 42]], "IP SAN missing");
        assert_eq!(dns, vec!["mac.local".to_string()], "DNS SAN missing");

        // The device rejects a leaf whose issuer isn't the pinned CA, so the
        // linkage has to be right before any handshake is attempted.
        let ca_pem = ca_cert_pem(&paths).unwrap();
        let (_, ca_der) = x509_parser::pem::parse_x509_pem(ca_pem.as_bytes()).unwrap();
        let (_, ca) = x509_parser::parse_x509_certificate(&ca_der.contents).unwrap();
        assert_eq!(leaf.issuer().to_string(), ca.subject().to_string());
    }

    /// A Kindle whose clock drifted behind real time must still accept the cert
    /// — `not_before` is backdated for exactly that case.
    #[test]
    fn validity_window_is_backdated_and_long() {
        let (_tmp, paths) = paths();
        issue_server_cert(&paths, &["127.0.0.1".to_string()]).unwrap();
        let pem = std::fs::read_to_string(paths.server_cert()).unwrap();
        let (_, der) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
        let (_, leaf) = x509_parser::parse_x509_certificate(&der.contents).unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let not_before = leaf.validity().not_before.timestamp();
        let not_after = leaf.validity().not_after.timestamp();

        let days = 24 * 60 * 60;
        assert!(
            now - not_before >= (BACKDATE_DAYS - 1) * days,
            "not_before should be ~{BACKDATE_DAYS}d in the past"
        );
        assert!(
            not_after - now > 4 * 365 * days,
            "leaf should be good for years, not months"
        );
    }

    #[test]
    fn issuing_without_an_address_is_refused() {
        let (_tmp, paths) = paths();
        assert!(issue_server_cert(&paths, &[]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn private_keys_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, paths) = paths();
        issue_server_cert(&paths, &["127.0.0.1".to_string()]).unwrap();
        for key in [paths.ca_key(), paths.server_key()] {
            let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} is {mode:o}", key.display());
        }
    }
}
