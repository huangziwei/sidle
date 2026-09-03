use std::collections::BTreeSet;

use anyhow::{Context, Result};
use bokai::formats::epub::{self as epub, EpubPackage, MemberRole};
use bokai::validate::Severity;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::library::db::BookRow;
use crate::library::source::{self, Source};

#[derive(Debug, Clone, Serialize)]
pub struct MemberInfo {
    pub path: String,
    pub id: Option<String>,
    pub media_type: Option<String>,
    pub role: &'static str,
    pub spine_index: Option<usize>,
    pub label: Option<String>,
    pub size: usize,
    pub text: bool,
}

impl From<epub::Member> for MemberInfo {
    fn from(m: epub::Member) -> Self {
        Self {
            path: m.path,
            id: m.id,
            media_type: m.media_type,
            role: m.role.as_str(),
            spine_index: m.spine_index,
            label: m.label,
            size: m.size,
            text: m.text,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FindingInfo {
    pub check: &'static str,
    pub rule: String,
    pub severity: &'static str,
    pub location: String,
    pub message: String,
    pub member: Option<String>,
    pub line: Option<u32>,
    pub fix_action: Option<String>,
    pub fix_detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Operation {
    RenameClass {
        from: String,
        to: String,
    },
    RemoveUnusedCss,
    Beautify {
        member: Option<String>,
    },
    SplitDocument {
        member: String,
        line: usize,
        col: usize,
    },
    MergeWithNext {
        member: String,
    },
    UpgradeEpub3,
}

impl Operation {
    pub fn describe(&self) -> String {
        match self {
            Operation::RenameClass { from, to } => format!("rename class {from} to {to}"),
            Operation::RemoveUnusedCss => "remove unused CSS".to_string(),
            Operation::Beautify { member: Some(m) } => format!("beautify {m}"),
            Operation::Beautify { member: None } => "beautify every text member".to_string(),
            Operation::SplitDocument { member, line, .. } => {
                format!("split {member} at line {line}")
            }
            Operation::MergeWithNext { member } => format!("merge the next document into {member}"),
            Operation::UpgradeEpub3 => "upgrade to EPUB 3".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberText {
    pub path: String,
    pub media_type: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub operation: String,
    pub changed: Vec<MemberText>,
    pub added: Vec<MemberText>,
    pub removed: Vec<String>,
    pub notes: Vec<String>,
}

pub struct EpubSession {
    book_id: i64,
    path: String,
    package: EpubPackage,
    dirty: BTreeSet<String>,
}

impl EpubSession {
    pub fn open(book: &BookRow) -> Result<Self> {
        let (source, path) = source::of(book)?;
        if source != Source::Epub {
            anyhow::bail!(
                "text editing writes to an EPUB source; this is a {}-source book",
                source.as_str()
            );
        }
        let bytes = std::fs::read(&path).with_context(|| format!("read {path}"))?;
        Self::from_bytes(book.id, path, &bytes)
    }

    pub fn from_bytes(book_id: i64, path: String, bytes: &[u8]) -> Result<Self> {
        let package = EpubPackage::parse(bytes).with_context(|| format!("open {path}"))?;
        Ok(Self {
            book_id,
            path,
            package,
            dirty: BTreeSet::new(),
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn members(&self) -> Result<Vec<MemberInfo>> {
        Ok(epub::members(&self.package)
            .context("read the package manifest")?
            .into_iter()
            .map(MemberInfo::from)
            .collect())
    }

    pub fn opf_path(&self) -> Result<String> {
        self.package
            .opf_path()
            .context("locate the package document")
    }

    pub fn read(&self, member: &str) -> Result<&[u8]> {
        self.package
            .get(member)
            .with_context(|| format!("no member {member} in {}", self.path))
    }

    pub fn read_text(&self, member: &str) -> Result<String> {
        Ok(decode(self.read(member)?))
    }

    pub fn write_text(&mut self, member: &str, text: &str) -> Result<()> {
        self.write_bytes(member, text.as_bytes().to_vec())
    }

    pub fn write_bytes(&mut self, member: &str, bytes: Vec<u8>) -> Result<()> {
        if !self.package.replace(member, bytes) {
            anyhow::bail!("no member {member} in {}; add it instead", self.path);
        }
        self.dirty.insert(member.to_string());
        Ok(())
    }

    pub fn add(&mut self, member: &str, media_type: &str, bytes: Vec<u8>) -> Result<String> {
        self.package.set(member, bytes);
        let opf = self.opf_path()?;
        let id = epub::add_manifest_item(&mut self.package, member, media_type)
            .with_context(|| format!("register {member} in the manifest"))?;
        self.dirty.insert(member.to_string());
        self.dirty.insert(opf);
        Ok(id)
    }

    pub fn remove(&mut self, member: &str) -> Result<()> {
        if !self.package.remove(member) {
            anyhow::bail!("no member {member} in {}", self.path);
        }
        self.dirty.remove(member);
        self.dirty.insert(self.opf_path()?);
        Ok(())
    }

    pub fn apply(&mut self, op: &Operation) -> Result<Outcome> {
        let pkg = &mut self.package;
        let changes = match op {
            Operation::RenameClass { from, to } => epub::rename_class(pkg, from, to),
            Operation::RemoveUnusedCss => epub::remove_unused_css(pkg),
            Operation::Beautify { member } => epub::beautify(pkg, member.as_deref()),
            Operation::SplitDocument { member, line, col } => {
                epub::split_document(pkg, member, *line, *col)
            }
            Operation::MergeWithNext { member } => epub::merge_with_next(pkg, member),
            Operation::UpgradeEpub3 => epub::upgrade_to_epub3(pkg),
        }
        .with_context(|| op.describe())?;
        let mut outcome = Outcome {
            operation: op.describe(),
            changed: Vec::new(),
            added: Vec::new(),
            removed: changes.removed.clone(),
            notes: changes.notes.clone(),
        };
        for path in &changes.changed {
            self.dirty.insert(path.clone());
            outcome.changed.push(MemberText {
                path: path.clone(),
                media_type: None,
                text: self.read_text(path)?,
            });
        }
        for (path, media_type) in &changes.added {
            self.dirty.insert(path.clone());
            outcome.added.push(MemberText {
                path: path.clone(),
                media_type: Some(media_type.clone()),
                text: self.read_text(path)?,
            });
        }
        for path in &changes.removed {
            self.dirty.remove(path);
        }
        Ok(outcome)
    }

    pub fn dirty(&self) -> Vec<String> {
        self.dirty.iter().cloned().collect()
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.package.to_bytes().context("repackage the EPUB")
    }

    pub fn validate(&self) -> Result<Vec<FindingInfo>> {
        let bytes = self.to_bytes()?;
        Ok(findings_of(&bytes, &self.member_names()))
    }

    pub fn save(&mut self, conn: &Connection) -> Result<Vec<String>> {
        if self.dirty.is_empty() {
            return Ok(Vec::new());
        }
        let bytes = self.to_bytes()?;
        source::commit(conn, self.book_id, Source::Epub, &self.path, &bytes)?;
        Ok(std::mem::take(&mut self.dirty).into_iter().collect())
    }

    fn member_names(&self) -> Vec<String> {
        self.package.names().map(str::to_string).collect()
    }
}

pub fn member_bytes(source_path: &str, member: &str) -> Result<Vec<u8>> {
    let bytes = std::fs::read(source_path).with_context(|| format!("read {source_path}"))?;
    epub::edit::read_member(&bytes, member)
        .with_context(|| format!("open {source_path}"))?
        .with_context(|| format!("no member {member} in {source_path}"))
}

pub fn member_text(source_path: &str, member: &str) -> Result<String> {
    Ok(decode(&member_bytes(source_path, member)?))
}

pub fn validate_file(source_path: &str) -> Result<Vec<FindingInfo>> {
    let bytes = std::fs::read(source_path).with_context(|| format!("read {source_path}"))?;
    let names: Vec<String> = EpubPackage::parse(&bytes)
        .map(|p| p.names().map(str::to_string).collect())
        .unwrap_or_default();
    Ok(findings_of(&bytes, &names))
}

pub fn media_type_for(member: &str) -> Option<&'static str> {
    let ext = member.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "xhtml" | "html" | "htm" => "application/xhtml+xml",
        "css" => "text/css",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ncx" => "application/x-dtbncx+xml",
        "js" => "application/javascript",
        "xml" => "application/xml",
        "txt" => "text/plain",
        _ => return None,
    })
}

pub fn role_is_text(role: MemberRole) -> bool {
    !matches!(
        role,
        MemberRole::Image | MemberRole::Font | MemberRole::Audio | MemberRole::Video
    )
}

pub fn decode(bytes: &[u8]) -> String {
    let bytes = epub::strip_bom(bytes);
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units = bytes.chunks_exact(2).map(|c| unit([c[0], c[1]]));
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

fn findings_of(bytes: &[u8], members: &[String]) -> Vec<FindingInfo> {
    bokai::validate::source::validate(bytes)
        .findings
        .into_iter()
        .map(|f| {
            let (member, line) = locate(&f.location, members);
            FindingInfo {
                check: f.check,
                rule: f.rule,
                severity: match f.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                    Severity::Info => "info",
                },
                location: f.location,
                message: f.message,
                member,
                line,
                fix_action: f.fix.as_ref().map(|h| h.action.clone()),
                fix_detail: f.fix.map(|h| h.detail),
            }
        })
        .collect()
}

fn locate(location: &str, members: &[String]) -> (Option<String>, Option<u32>) {
    let is_member = |s: &str| members.iter().any(|m| m == s);
    if is_member(location) {
        return (Some(location.to_string()), None);
    }
    if let Some((path, line)) = location.rsplit_once(':')
        && is_member(path)
    {
        return (Some(path.to_string()), line.trim().parse().ok());
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_handles_boms_and_utf16() {
        assert_eq!(decode(b"\xEF\xBB\xBFabc"), "abc");
        assert_eq!(decode(b"abc"), "abc");
        assert_eq!(decode(b"\xFF\xFEa\x00b\x00"), "ab");
        assert_eq!(decode(b"\xFE\xFF\x00a\x00b"), "ab");
    }

    #[test]
    fn locations_split_into_member_and_line() {
        let members = vec!["OEBPS/ch1.xhtml".to_string(), "OEBPS/a:b.css".to_string()];
        assert_eq!(
            locate("OEBPS/ch1.xhtml:12", &members),
            (Some("OEBPS/ch1.xhtml".into()), Some(12))
        );
        assert_eq!(
            locate("OEBPS/ch1.xhtml", &members),
            (Some("OEBPS/ch1.xhtml".into()), None)
        );
        assert_eq!(
            locate("OEBPS/a:b.css", &members),
            (Some("OEBPS/a:b.css".into()), None)
        );
        assert_eq!(locate("<toc>", &members), (None, None));
    }

    #[test]
    fn media_types_follow_the_extension() {
        assert_eq!(media_type_for("OEBPS/styles/a.css"), Some("text/css"));
        assert_eq!(media_type_for("a.XHTML"), Some("application/xhtml+xml"));
        assert_eq!(media_type_for("a.unknown"), None);
    }

    fn tiny_epub() -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default();
        zip.start_file("mimetype", stored).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();
        zip.start_file("META-INF/container.xml", deflated).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#).unwrap();
        zip.start_file("OEBPS/content.opf", deflated).unwrap();
        zip.write_all(br#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="id">x</dc:identifier><dc:title>T</dc:title><dc:language>en</dc:language></metadata>
  <manifest>
    <item href="ch1.xhtml" id="ch1" media-type="application/xhtml+xml"/>
    <item href="style.css" id="css" media-type="text/css"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#).unwrap();
        zip.start_file("OEBPS/ch1.xhtml", deflated).unwrap();
        zip.write_all(b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>c</title></head><body><p>old</p></body></html>").unwrap();
        zip.start_file("OEBPS/style.css", deflated).unwrap();
        zip.write_all(b"p { color: red }").unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn a_save_rewrites_only_the_edited_member() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("book.epub");
        std::fs::write(&path, tiny_epub()).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        let p = path.to_string_lossy().to_string();

        let mut s = EpubSession::from_bytes(1, p.clone(), &std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(s.members().unwrap().len(), 5);
        assert_eq!(s.read_text("OEBPS/style.css").unwrap(), "p { color: red }");
        assert!(s.write_text("OEBPS/missing.css", "x").is_err());
        s.write_text("OEBPS/ch1.xhtml", "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>c</title></head><body><p>new</p></body></html>").unwrap();
        assert_eq!(s.dirty(), vec!["OEBPS/ch1.xhtml".to_string()]);
        assert_eq!(s.save(&conn).unwrap(), vec!["OEBPS/ch1.xhtml".to_string()]);
        assert!(s.dirty().is_empty());
        assert!(s.save(&conn).unwrap().is_empty());

        let after = std::fs::read(&path).unwrap();
        assert!(
            member_text(&p, "OEBPS/ch1.xhtml")
                .unwrap()
                .contains("<p>new</p>")
        );
        assert_eq!(
            member_text(&p, "OEBPS/style.css").unwrap(),
            "p { color: red }"
        );

        let mut s = EpubSession::from_bytes(1, p.clone(), &after).unwrap();
        let id = s
            .add("OEBPS/extra.css", "text/css", b"b { x: 1 }".to_vec())
            .unwrap();
        assert_eq!(
            s.dirty(),
            vec![
                "OEBPS/content.opf".to_string(),
                "OEBPS/extra.css".to_string()
            ]
        );
        s.save(&conn).unwrap();
        let s = EpubSession::from_bytes(1, p.clone(), &std::fs::read(&path).unwrap()).unwrap();
        let m = s.members().unwrap();
        let extra = m.iter().find(|m| m.path == "OEBPS/extra.css").unwrap();
        assert_eq!(extra.id.as_deref(), Some(id.as_str()));
        assert_eq!(extra.role, "style");
        assert!(
            !s.validate()
                .unwrap()
                .iter()
                .any(|f| f.severity == "error" && f.member.as_deref() == Some("OEBPS/extra.css"))
        );
    }
}
