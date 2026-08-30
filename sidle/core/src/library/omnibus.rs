//! Turning a collection in the library into the series it collects.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use bokai::formats::epub::split::{self, Cut, Numbering};

use crate::library::db::{self, BookRow, BulkMetadataPatch};
use crate::library::import::{self, ImportOutcome};
use crate::library::paths::LibraryPaths;

/// One volume of a split, as the user reviews it: a title and a number they may
/// change, over a span of the collection they may not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeCut {
    /// Spine index of the volume's first document.
    pub spine_index: usize,
    /// Spine documents the volume spans.
    pub documents: usize,
    /// The volume's title — what the collection's own navigation calls it,
    /// until the user says otherwise.
    pub title: String,
    /// Position in the series. Fractional because publishers number that way: a
    /// 5.5 shipped between 5 and 6 is a real volume with a real place.
    pub number: f64,
    /// The volume's cover page inside the collection, as a path within the
    /// archive. Carried through the round trip untouched — which document that
    /// is belongs to the splitter, not to the user.
    #[serde(default)]
    pub cover: Option<String>,
    /// Whether [`VolumeCut::number`] was counted on from the volume before
    /// rather than read from the volume's own label. Shown so the user knows
    /// which numbers deserve a second look.
    pub counted: bool,
}

impl VolumeCut {
    fn from_cut(cut: &Cut) -> Self {
        Self {
            spine_index: cut.spine_index,
            documents: cut.documents,
            title: cut.label.clone(),
            number: cut.number,
            cover: cut.cover.clone(),
            counted: cut.numbering == Numbering::Sequence,
        }
    }

    fn to_cut(&self) -> Cut {
        Cut {
            spine_index: self.spine_index,
            documents: self.documents,
            label: self.title.clone(),
            cover: self.cover.clone(),
            number: self.number,
            // Round-tripping the provenance would be a lie once the user has
            // edited the number, and the splitter only reads the value.
            numbering: Numbering::Label,
        }
    }
}

/// What a collection is about to become: one series, with these volumes in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitPlan {
    pub series_name: String,
    pub volumes: Vec<VolumeCut>,
}

/// How one volume of a committed split ended up.
#[derive(Debug, Clone, Serialize)]
pub struct VolumeOutcome {
    pub title: String,
    pub number: f64,
    /// The library row the volume landed in, when it landed in one.
    pub book_id: Option<i64>,
    /// This place in the series was already taken, so nothing was written.
    pub duplicate: bool,
    /// The volume still needs its `epub_to_kfx` conversion queued.
    pub needs_enqueue: bool,
    pub error: Option<String>,
}

/// Propose how to split a collection: where the volumes divide, what to call
/// each one, and what to call the series they form.
pub fn propose(epub_bytes: &[u8], omnibus: &BookRow) -> Result<SplitPlan> {
    let cuts = split::propose_cuts(epub_bytes).context("read the collection's volumes")?;
    let declared = omnibus
        .series_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    Ok(SplitPlan {
        series_name: declared
            .map(str::to_string)
            .unwrap_or_else(|| series_name_from_title(&omnibus.title)),
        volumes: cuts.iter().map(VolumeCut::from_cut).collect(),
    })
}

/// Carve the collection into the volumes the plan describes, as EPUB bytes.
pub fn carve_volumes(omnibus: &BookRow, plan: &SplitPlan) -> Result<Vec<Vec<u8>>> {
    let series = plan.series_name.trim();
    if series.is_empty() {
        bail!("the series needs a name — every volume is grouped by it");
    }
    if plan.volumes.is_empty() {
        bail!("no volumes to split into");
    }
    if let Some(blank) = plan.volumes.iter().find(|v| v.title.trim().is_empty()) {
        bail!("volume {} needs a title", trim_number(blank.number));
    }

    let epub_path = omnibus
        .epub_path
        .as_deref()
        .context("this book has no EPUB yet — the conversion queue is still working on it")?;
    let epub_bytes = std::fs::read(epub_path).with_context(|| format!("read {epub_path}"))?;

    let cuts: Vec<Cut> = plan.volumes.iter().map(VolumeCut::to_cut).collect();
    split::split(&epub_bytes, &cuts, Some(series))
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("split the collection into volumes")
}

/// Import each carved volume, then put the omnibus in the series alongside them.
pub fn add_volumes(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    omnibus: &BookRow,
    plan: &SplitPlan,
    volumes: Vec<Vec<u8>>,
    mut progress: impl FnMut(usize, &str),
) -> Result<Vec<VolumeOutcome>> {
    let series = plan.series_name.trim();
    let mut out = Vec::with_capacity(volumes.len());
    for (n, (cut, bytes)) in plan.volumes.iter().zip(volumes).enumerate() {
        progress(n, &cut.title);
        out.push(import_volume(conn, paths, series, cut, bytes));
    }

    // The omnibus joins the series it produced, with no position of its own so
    // it sorts after the numbered volumes. Only once something is actually in
    // that series — a split where every volume failed leaves the book alone.
    if out.iter().any(|v| v.book_id.is_some()) {
        let patch = BulkMetadataPatch {
            series_name: Some(series.to_string()),
            ..Default::default()
        };
        db::apply_bulk_patch(conn, omnibus.id, &patch)
            .context("put the omnibus in its own series")?;
    }

    Ok(out)
}

/// Import one volume, unless its place in the series is already taken.
fn import_volume(
    conn: &rusqlite::Connection,
    paths: &LibraryPaths,
    series: &str,
    cut: &VolumeCut,
    bytes: Vec<u8>,
) -> VolumeOutcome {
    let outcome = VolumeOutcome {
        title: cut.title.clone(),
        number: cut.number,
        book_id: None,
        duplicate: false,
        needs_enqueue: false,
        error: None,
    };

    // Splitting the same collection twice must not double the series: the two
    // runs produce different bytes (a fresh modification timestamp in each
    // volume), so the byte-level dedupe inside the import would not catch it.
    match db::find_in_series(conn, series, cut.number) {
        Ok(Some(existing)) => {
            return VolumeOutcome {
                book_id: Some(existing.id),
                duplicate: true,
                ..outcome
            };
        }
        Ok(None) => {}
        Err(e) => {
            return VolumeOutcome {
                error: Some(e.to_string()),
                ..outcome
            };
        }
    }

    let name = format!(
        "{}.epub",
        crate::library::paths::sanitize_segment(&cut.title)
    );
    match import::import_bytes(conn, paths, bytes, &name) {
        Ok(ImportOutcome::Imported {
            book,
            needs_enqueue,
        }) => VolumeOutcome {
            book_id: Some(book.id),
            needs_enqueue,
            ..outcome
        },
        Ok(ImportOutcome::Duplicate(book)) => VolumeOutcome {
            book_id: Some(book.id),
            duplicate: true,
            ..outcome
        },
        Err(e) => VolumeOutcome {
            error: Some(format!("{e:#}")),
            ..outcome
        },
    }
}

/// The series name a collection's title states, with the words that describe
pub fn series_name_from_title(title: &str) -> String {
    let stripped = tidy(&strip_bundle_words(&strip_bracketed(title)));
    if stripped.is_empty() {
        title.trim().to_string()
    } else {
        stripped
    }
}

/// Bracketed groups that describe the bundle rather than the work: the 【合本版】
fn strip_bracketed(title: &str) -> String {
    const PAIRS: [(char, char); 8] = [
        ('【', '】'),
        ('〔', '〕'),
        ('〈', '〉'),
        ('《', '》'),
        ('＜', '＞'),
        ('［', '］'),
        ('[', ']'),
        ('（', '）'),
    ];

    let mut out = String::with_capacity(title.len());
    let mut rest = title;
    // `(` is handled apart from the table because it is the ASCII pair a store
    // uses for the imprint, and the table's fixed size says nothing about it.
    while let Some((open, close, at)) =
        rest.char_indices()
            .find_map(|(i, c)| match PAIRS.iter().find(|(o, _)| *o == c) {
                Some(&(o, cl)) => Some((o, cl, i)),
                None if c == '(' => Some(('(', ')', i)),
                None => None,
            })
    {
        let after_open = at + open.len_utf8();
        let Some(end) = rest[after_open..].find(close) else {
            break;
        };
        let inner = &rest[after_open..after_open + end];
        out.push_str(&rest[..at]);
        if !describes_the_bundle(inner) {
            out.push(open);
            out.push_str(inner);
            out.push(close);
        }
        rest = &rest[after_open + end + close.len_utf8()..];
    }
    out.push_str(rest);
    out
}

/// Whether a bracketed group says something about the bundle rather than the
/// work: it names the packaging, spans the volumes inside, or is the imprint.
fn describes_the_bundle(inner: &str) -> bool {
    const PACKAGING: [&str; 6] = ["合本", "セット", "特典", "収録", "分冊", "全巻"];
    const IMPRINTS: [&str; 7] = [
        "文庫",
        "新書",
        "選書",
        "ノベルス",
        "コミックス",
        "ブックス",
        "BOOKS",
    ];
    if PACKAGING.iter().any(|w| inner.contains(w)) {
        return true;
    }
    if IMPRINTS.iter().any(|w| inner.ends_with(w)) {
        return true;
    }
    spans_volumes(inner)
}

/// Whether a group is a span of the volumes inside — `上下`, `第１部～第３部`,
/// `ＢＯＯＫ１～３`. It must be made only of counting words and say what it is
/// counting, so a bare `（１）` after a work's title is left alone.
fn spans_volumes(inner: &str) -> bool {
    const COUNTED: [char; 10] = ['上', '中', '下', '巻', '卷', '冊', '册', '部', '編', '篇'];
    // `ＢＯＯＫ１～３` counts volumes the way `第１部～第３部` does; it just says
    // so in letters.
    let mut counts = inner
        .chars()
        .map(fold_fullwidth)
        .collect::<String>()
        .to_ascii_uppercase()
        .contains("BOOK");
    let mut any = false;
    for c in inner.chars() {
        any = true;
        if COUNTED.contains(&c) {
            counts = true;
            continue;
        }
        let structural = c.is_ascii_digit()
            || ('０'..='９').contains(&c)
            || matches!(
                c,
                '第' | '全' | '～' | '〜' | '~' | '-' | '‐' | '–' | '—' | '・' | '、'
            )
            || c.is_whitespace()
            || matches!(c, 'B' | 'O' | 'K' | 'b' | 'o' | 'k' | 'Ｂ' | 'Ｏ' | 'Ｋ');
        if !structural {
            return false;
        }
    }
    any && counts
}

/// A fullwidth letter or digit as its ASCII twin; everything else unchanged.
fn fold_fullwidth(c: char) -> char {
    match c {
        'Ａ'..='Ｚ' | 'ａ'..='ｚ' | '０'..='９' => {
            char::from_u32(c as u32 - 0xFEE0).unwrap_or(c)
        }
        _ => c,
    }
}

/// The same words standing on their own, outside any bracket: `合本版`,
/// `全巻セット`, and a count of the volumes inside.
fn strip_bundle_words(title: &str) -> String {
    let mut out = strip_volume_counts(title);
    for word in ["合本版", "合本", "全巻セット", "全巻", "電子特典付き"] {
        out = out.replace(word, "");
    }
    out
}

/// `全` + a number + a counter (+ an optional `収録`): how many volumes the
/// bundle holds. The digits are required, so a work whose own name ends in
/// `全集` keeps it.
fn strip_volume_counts(title: &str) -> String {
    const COUNTERS: [char; 4] = ['巻', '卷', '冊', '册'];
    let chars: Vec<char> = title.chars().collect();
    let mut out = String::with_capacity(title.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '全' {
            let mut j = i + 1;
            while j < chars.len()
                && (chars[j].is_ascii_digit() || ('０'..='９').contains(&chars[j]))
            {
                j += 1;
            }
            if j > i + 1 && j < chars.len() && COUNTERS.contains(&chars[j]) {
                j += 1;
                if chars[j..].starts_with(&['収', '録']) {
                    j += 2;
                }
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Collapse the whitespace the removals left behind and trim the punctuation
/// they exposed at either end.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    out.trim_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '-' | '–' | '—' | '～' | '〜' | '~' | '・' | '、' | ',' | ':' | '：'
            )
    })
    .to_string()
}

/// A volume number as a person writes it: `3` rather than `3.0`, and `5.5` when
/// that is what it is.
fn trim_number(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        format!("{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every title below is invented. The point is the shape a store writes —
    // banner, count, span, imprint — not any particular book.

    #[test]
    fn a_bundles_title_gives_up_the_work_inside_it() {
        for (title, want) in [
            (
                "【合本版】星降る庭の物語　全20冊収録 (架空文庫)",
                "星降る庭の物語",
            ),
            (
                "【合本版】灯台守の日々　全11巻（電子特典付き） (架空文庫)",
                "灯台守の日々",
            ),
            ("霧の街（上下）合本版（架空文庫）", "霧の街"),
            ("古都日記（第１部～第３部）合本版", "古都日記"),
            (
                "水底の図書館（ＢＯＯＫ１～３）合本版（架空文庫）",
                "水底の図書館",
            ),
            ("森の便り　合本版", "森の便り"),
            ("砂の記録 全巻セット", "砂の記録"),
            ("＜3冊合本＞短編の組み立て方", "短編の組み立て方"),
            ("旅の詩集 全１９冊合本版", "旅の詩集"),
        ] {
            assert_eq!(series_name_from_title(title), want, "from {title}");
        }
    }

    #[test]
    fn a_work_whose_own_name_counts_keeps_it() {
        // `全集` is the work, not a count of what is bundled — the digits a
        // bundle's count always carries are what tell them apart.
        assert_eq!(series_name_from_title("架空太郎全集"), "架空太郎全集");
        // A number in brackets after a title is a volume of a series, not a
        // span of volumes inside one file.
        assert_eq!(
            series_name_from_title("戯曲全集（１） (架空文庫)"),
            "戯曲全集（１）"
        );
        // An edition marker describes the text, not the packaging.
        assert_eq!(
            series_name_from_title("湖畔の事件＜完全改訂版＞"),
            "湖畔の事件＜完全改訂版＞"
        );
    }

    #[test]
    fn a_title_that_is_nothing_but_packaging_survives_as_itself() {
        // Reducing it to an empty field would leave the user nothing to edit.
        assert_eq!(series_name_from_title("【合本版】"), "【合本版】");
        assert_eq!(series_name_from_title("   "), "");
    }

    #[test]
    fn a_cut_survives_the_round_trip_the_user_edits_it_on() {
        let cut = Cut {
            spine_index: 12,
            documents: 21,
            label: "物語2".to_string(),
            cover: Some("OEBPS/cover2.xhtml".to_string()),
            number: 2.0,
            numbering: Numbering::Label,
        };
        let mut edited = VolumeCut::from_cut(&cut);
        assert!(!edited.counted);
        edited.title = "第二部".to_string();
        edited.number = 2.5;

        let back = edited.to_cut();
        assert_eq!(back.spine_index, 12);
        assert_eq!(back.documents, 21);
        assert_eq!(back.label, "第二部");
        assert_eq!(back.number, 2.5);
        // The cover the splitter found is not the user's to lose.
        assert_eq!(back.cover.as_deref(), Some("OEBPS/cover2.xhtml"));
    }

    /// A collection of three volumes, each opening on its own Contents page —
    /// the shape a split reads. Invented end to end; the point is the structure.
    fn collection_epub() -> Vec<u8> {
        fn page(title: &str, body: &str) -> String {
            format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>{title}</title></head>\
                 <body>{body}</body></html>"
            )
        }

        let mut docs: Vec<(String, String)> = vec![(
            "front.xhtml".to_string(),
            page("扉", "<p>この本は三巻を収めます。</p>"),
        )];
        for v in 1..=3 {
            docs.push((
                format!("v{v}-toc.xhtml"),
                page(
                    &format!("第{v}巻"),
                    &format!(
                        "<h1>目次</h1><ul>\
                         <li><a href=\"v{v}-c1.xhtml\">第一章</a></li>\
                         <li><a href=\"v{v}-c2.xhtml\">第二章</a></li></ul>"
                    ),
                ),
            ));
            docs.push((
                format!("v{v}-c1.xhtml"),
                page("第一章", "<h1>第一章</h1><p>ある朝のことでした。</p>"),
            ));
            docs.push((
                format!("v{v}-c2.xhtml"),
                page("第二章", "<h1>第二章</h1><p>それから幾年か。</p>"),
            ));
        }

        let manifest: String = docs
            .iter()
            .enumerate()
            .map(|(i, (name, _))| {
                format!(
                    "<item id=\"d{i}\" href=\"{name}\" media-type=\"application/xhtml+xml\"/>\n"
                )
            })
            .collect();
        let spine: String = (0..docs.len())
            .map(|i| format!("<itemref idref=\"d{i}\"/>\n"))
            .collect();
        let nav_items: String = (1..=3)
            .map(|v| format!("<li><a href=\"v{v}-toc.xhtml\">第{v}巻</a></li>\n"))
            .collect();
        let nav = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <html xmlns=\"http://www.w3.org/1999/xhtml\" \
             xmlns:epub=\"http://www.idpf.org/2007/ops\"><head><title>目次</title></head>\
             <body><nav epub:type=\"toc\"><ol>{nav_items}</ol></nav></body></html>"
        );
        let opf = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" \
             unique-identifier=\"uid\">\
             <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\
             <dc:identifier id=\"uid\">urn:uuid:11111111-2222-3333-4444-555555555555</dc:identifier>\
             <dc:title>【合本版】架空の記録　全3巻</dc:title>\
             <dc:language>ja</dc:language>\
             <dc:creator>架空 太郎</dc:creator>\
             <meta property=\"dcterms:modified\">2020-01-01T00:00:00Z</meta>\
             </metadata>\
             <manifest><item id=\"nav\" href=\"nav.xhtml\" \
             media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n{manifest}</manifest>\
             <spine>{spine}</spine></package>"
        );

        let mut out = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut out);
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let deflated = zip::write::SimpleFileOptions::default();
            zip.start_file("mimetype", stored).unwrap();
            std::io::Write::write_all(&mut zip, b"application/epub+zip").unwrap();
            zip.start_file("META-INF/container.xml", deflated).unwrap();
            std::io::Write::write_all(
                &mut zip,
                b"<?xml version=\"1.0\"?>\n<container version=\"1.0\" \
                  xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\"><rootfiles>\
                  <rootfile full-path=\"OEBPS/content.opf\" \
                  media-type=\"application/oebps-package+xml\"/></rootfiles></container>",
            )
            .unwrap();
            zip.start_file("OEBPS/content.opf", deflated).unwrap();
            std::io::Write::write_all(&mut zip, opf.as_bytes()).unwrap();
            zip.start_file("OEBPS/nav.xhtml", deflated).unwrap();
            std::io::Write::write_all(&mut zip, nav.as_bytes()).unwrap();
            for (name, body) in &docs {
                zip.start_file(format!("OEBPS/{name}"), deflated).unwrap();
                std::io::Write::write_all(&mut zip, body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        out.into_inner()
    }

    /// A library with nothing in it, and the connection to reach it by.
    fn empty_library() -> (tempfile::TempDir, LibraryPaths, rusqlite::Connection) {
        let dir = tempfile::tempdir().unwrap();
        let paths = LibraryPaths {
            root: dir.path().to_path_buf(),
        };
        paths.ensure().unwrap();
        let conn = db::open(&paths.db()).unwrap();
        (dir, paths, conn)
    }

    #[test]
    fn a_series_the_source_declared_beats_one_read_out_of_the_title() {
        let (_dir, paths, conn) = empty_library();
        let bytes = collection_epub();
        let mut omnibus =
            match import::import_bytes(&conn, &paths, bytes.clone(), "collection.epub").unwrap() {
                ImportOutcome::Imported { book, .. } => book,
                ImportOutcome::Duplicate(_) => panic!("nothing was in the library"),
            };
        // What a source states — an EPUB's `belongs-to-collection`, or the
        // annotation a store title carries — is a declaration, not a guess.
        omnibus.series_name = Some("別名の叢書".to_string());
        assert_eq!(propose(&bytes, &omnibus).unwrap().series_name, "別名の叢書");

        omnibus.series_name = Some("   ".to_string());
        assert_eq!(propose(&bytes, &omnibus).unwrap().series_name, "架空の記録");
    }

    #[test]
    fn a_collection_becomes_a_series_of_books() {
        let (_dir, paths, conn) = empty_library();
        let bytes = collection_epub();

        let omnibus = match import::import_bytes(&conn, &paths, bytes.clone(), "collection.epub")
            .expect("import the collection")
        {
            ImportOutcome::Imported { book, .. } => book,
            ImportOutcome::Duplicate(_) => panic!("nothing was in the library"),
        };

        let plan = propose(&bytes, &omnibus).expect("propose");
        assert_eq!(plan.series_name, "架空の記録");
        assert_eq!(plan.volumes.len(), 3, "one cut per volume");
        assert_eq!(
            plan.volumes.iter().map(|v| v.number).collect::<Vec<_>>(),
            vec![1.0, 2.0, 3.0]
        );

        let carved = carve_volumes(&omnibus, &plan).expect("carve");
        let outcomes = add_volumes(&conn, &paths, &omnibus, &plan, carved, |_, _| {}).expect("add");
        assert_eq!(outcomes.len(), 3);
        for (n, outcome) in outcomes.iter().enumerate() {
            assert!(outcome.error.is_none(), "volume {n}: {:?}", outcome.error);
            assert!(
                !outcome.duplicate,
                "volume {n} was written for the first time"
            );
            let id = outcome.book_id.expect("a row per volume");
            let row = db::get_book(&conn, id).unwrap().expect("the volume's row");
            assert_eq!(row.series_name.as_deref(), Some("架空の記録"));
            assert_eq!(row.series_index, Some(n as f64 + 1.0));
            // Each volume is its own book, not a copy of the collection.
            assert!(row.file_size < omnibus.file_size);
        }

        // The collection joins the series it produced, with no position of its
        // own so it sorts after the volumes.
        let after = db::get_book(&conn, omnibus.id).unwrap().unwrap();
        assert_eq!(after.series_name.as_deref(), Some("架空の記録"));
        assert_eq!(after.series_index, None);
        assert_eq!(db::list_books(&conn).unwrap().len(), 4);
    }

    #[test]
    fn splitting_the_same_collection_twice_does_not_double_the_series() {
        let (_dir, paths, conn) = empty_library();
        let bytes = collection_epub();
        let omnibus = match import::import_bytes(&conn, &paths, bytes.clone(), "collection.epub")
            .expect("import the collection")
        {
            ImportOutcome::Imported { book, .. } => book,
            ImportOutcome::Duplicate(_) => panic!("nothing was in the library"),
        };
        let plan = propose(&bytes, &omnibus).unwrap();

        let carve = || carve_volumes(&omnibus, &plan).unwrap();
        let first = add_volumes(&conn, &paths, &omnibus, &plan, carve(), |_, _| {}).unwrap();
        let second = add_volumes(&conn, &paths, &omnibus, &plan, carve(), |_, _| {}).unwrap();

        // Each volume's own bytes differ run to run — every one carries a fresh
        // modification timestamp — so the byte-level dedupe cannot catch this.
        // The place in the series is what is already taken.
        assert!(second.iter().all(|v| v.duplicate));
        assert_eq!(
            first.iter().map(|v| v.book_id).collect::<Vec<_>>(),
            second.iter().map(|v| v.book_id).collect::<Vec<_>>(),
        );
        assert_eq!(db::list_books(&conn).unwrap().len(), 4);
    }

    #[test]
    fn a_counted_number_is_flagged_for_review() {
        let cut = Cut {
            spine_index: 0,
            documents: 3,
            label: "序".to_string(),
            cover: None,
            number: 1.0,
            numbering: Numbering::Sequence,
        };
        assert!(VolumeCut::from_cut(&cut).counted);
    }
}
