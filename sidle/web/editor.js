// Book editor — a Calibre "Edit book"-style surface built into Sidle. Full-screen
// #editor-view, parallel to #reader-view, driven entirely from the built
// boko-kai source-edit primitives via the `editor_*` Tauri commands. Edits
// KFX-, EPUB- and PDF-source books through the Metadata / Cover / Images / TOC
// panels; the rail's Text panel lights up in a later phase. Exposed as
// `window.sidleEditor`.

const $ = (sel) => document.querySelector(sel);
const toast = (msg, isError) => window.showToast?.(msg, isError);

// Live editor session, or null when closed. `open` snapshots the opening values
// so Revert can restore them and Save can diff against them.
let session = null;

const view = () => $("#editor-view");

// --- open / close ----------------------------------------------------------

async function open(bookId) {
  if (!close()) return; // tear down any prior session (aborts if user keeps edits)
  let data;
  try {
    data = await window.api.invoke("editor_open", { bookId });
  } catch (err) {
    toast(`Couldn't open editor: ${err}`, true);
    return;
  }

  session = { bookId, data, panel: "metadata", dirty: false, tocDetail: null };

  $("#editor-title").textContent = data.metadata.title || "Untitled";
  renderTocChip(data.toc);
  configureRail();
  selectPanel("metadata");

  view().hidden = false;
  installKeys();
  // Focus the first field so keyboard editing starts immediately.
  requestAnimationFrame(() => $("#editor-center input")?.focus());
}

// Returns true if the editor is now closed, false if the user chose to keep
// unsaved edits (so callers like `open` can abort).
function close() {
  if (!session) return true;
  if (session.dirty && !confirm("Discard unsaved changes?")) return false;
  removeKeys();
  view().hidden = true;
  $("#editor-center").replaceChildren();
  session = null;
  return true;
}

function isOpen() {
  return !!session && !view().hidden;
}

// --- keyboard --------------------------------------------------------------

let keyHandler = null;

function installKeys() {
  keyHandler = (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    } else if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "s") {
      e.preventDefault();
      if (!$("#editor-save").disabled) saveCurrentPanel();
    }
  };
  document.addEventListener("keydown", keyHandler, true);
}

function removeKeys() {
  if (keyHandler) {
    document.removeEventListener("keydown", keyHandler, true);
    keyHandler = null;
  }
}

// --- left rail -------------------------------------------------------------

// Metadata, Cover, Images and Table of Contents are live for every editable
// source (KFX, EPUB, PDF); a book whose source file is missing gets none of
// them. Text (in-place typo fixes) needs the surgical text-replace primitive
// that isn't built yet, so it stays gated with an explanatory tooltip.
function configureRail() {
  const editable = session.data.editable;
  // The backend reports which panels this source format can actually back, so
  // the rail follows capability rather than re-deriving it from the format name.
  const panels = new Set(session.data.panels || []);
  for (const item of document.querySelectorAll(".editor-rail-item")) {
    const p = item.dataset.panel;
    item.disabled = !(editable && panels.has(p));
    if (p === "text") {
      item.title = editable
        ? "In-place text editing is coming in a later tier."
        : "";
    } else {
      item.title = "";
    }
  }
}

function selectPanel(panel) {
  // Switching away from the metadata panel with unsaved edits would drop them.
  if (
    session.panel !== panel &&
    session.dirty &&
    !confirm("Discard unsaved changes to this panel?")
  ) {
    return;
  }
  session.panel = panel;
  session.dirty = false;
  for (const item of document.querySelectorAll(".editor-rail-item")) {
    item.classList.toggle("active", item.dataset.panel === panel);
  }
  // Every panel except Metadata commits via its own in-panel buttons, so the
  // top-bar Save/Revert (the metadata panel's) are disabled for them.
  if (panel === "metadata") {
    renderMetadataPanel();
    markDirty(false);
  } else if (panel === "cover") {
    $("#editor-save").disabled = true;
    $("#editor-revert").disabled = true;
    renderCoverPanel();
  } else if (panel === "images") {
    $("#editor-save").disabled = true;
    $("#editor-revert").disabled = true;
    renderImagesPanel();
  } else if (panel === "toc") {
    $("#editor-save").disabled = true;
    $("#editor-revert").disabled = true;
    renderTocPanel();
  }
}

// --- validate chip ---------------------------------------------------------

function renderTocChip(toc) {
  const chip = $("#editor-toc-chip");
  if (!toc) {
    chip.hidden = true;
    return;
  }
  chip.hidden = false;
  chip.dataset.verdict = toc.verdict;
  chip.textContent = chipText(toc.verdict);
  let title = `Declared TOC: ${toc.nav_chapters} chapter entr${
    toc.nav_chapters === 1 ? "y" : "ies"
  } of ${toc.nav_count}.`;
  // The in-book signal counts are only in the fuller open-time verdict, not the
  // post-edit refresh — include them when present.
  if (toc.contents_links != null) {
    title +=
      ` In-book signals — contents links: ${toc.contents_links}, ` +
      `headings: ${toc.headings}, chapter starts: ${toc.section_heads}.`;
  }
  chip.title = title;
}

// --- dirty state -----------------------------------------------------------

function markDirty(dirty) {
  session.dirty = dirty;
  $("#editor-save").disabled = !dirty || !session.data.editable;
  $("#editor-revert").disabled = !dirty;
}

// --- metadata panel --------------------------------------------------------

function renderMetadataPanel() {
  const m = session.data.metadata;
  const editable = session.data.editable;
  const center = $("#editor-center");
  center.replaceChildren();

  // All three source formats are editable; the only way here is a book whose
  // source file is missing from disk, so say that rather than blaming the format.
  const notice = editable
    ? ""
    : `<div class="editor-notice">This book's ${session.data.format.toUpperCase()} source
         file isn't on disk, so it can't be edited. These fields are read-only.</div>`;

  const form = document.createElement("form");
  form.className = "editor-panel metadata-fields";
  form.autocomplete = "off";
  form.innerHTML = `
    ${notice}
    <div class="field-group-title">Title &amp; author</div>
    <label class="field">
      <span>Title</span>
      <input name="title" required ${editable ? "" : "readonly"} />
    </label>
    <label class="field">
      <span>Authors</span>
      <input name="author" placeholder="Jane Smith & John Doe" ${editable ? "" : "readonly"} />
    </label>

    <div class="field-group-title">Publication</div>
    <div class="field-row">
      <label class="field">
        <span>Publisher</span>
        <input name="publisher" placeholder="新潮文庫" ${editable ? "" : "readonly"} />
      </label>
      <label class="field field-narrow">
        <span>Published</span>
        <input name="published_at" placeholder="2024" ${editable ? "" : "readonly"} />
      </label>
    </div>
    <label class="field">
      <span>Language</span>
      <input name="language" placeholder="en" ${editable ? "" : "readonly"} />
    </label>

    <div class="field-group-title">Identifiers</div>
    <label class="field">
      <span>ASIN</span>
      <input name="asin" placeholder="B0…" ${editable ? "" : "readonly"} />
    </label>
  `;

  form.elements.title.value = m.title || "";
  form.elements.author.value = m.author || "";
  form.elements.publisher.value = m.publisher || "";
  form.elements.published_at.value = m.published_at || "";
  form.elements.language.value = m.language || "";
  form.elements.asin.value = m.asin || "";

  form.addEventListener("submit", (e) => e.preventDefault());
  if (editable) {
    form.addEventListener("input", () => markDirty(true));
  }
  center.appendChild(form);
}

function metadataFormValues() {
  const f = $("#editor-center");
  const val = (name) => f.querySelector(`[name="${name}"]`).value.trim();
  return {
    title: val("title"),
    author: val("author"),
    language: val("language"),
    publisher: val("publisher") || null,
    published_at: val("published_at") || null,
    asin: val("asin") || null,
  };
}

async function saveMetadata() {
  const form = metadataFormValues();
  if (!form.title) {
    toast("Title can't be empty.", true);
    return;
  }
  const save = $("#editor-save");
  save.disabled = true;
  save.textContent = "Saving…";
  try {
    const row = await window.api.invoke("editor_save_metadata", {
      bookId: session.bookId,
      form,
    });
    // Reflect the canonicalized values the backend settled on (author flip,
    // language code harmonize) back into the fields and the snapshot.
    session.data.metadata = {
      title: row.title,
      author: row.author,
      language: row.language,
      publisher: row.publisher,
      published_at: row.published_at,
      asin: row.asin,
    };
    $("#editor-title").textContent = row.title || "Untitled";
    renderMetadataPanel();
    markDirty(false);
    toast("Saved — regenerating the derived EPUB…");
  } catch (err) {
    toast(`Save failed: ${err}`, true);
  } finally {
    save.textContent = "Save";
    save.disabled = !session.dirty;
  }
}

// --- cover panel -----------------------------------------------------------

// The cover flow reuses the library's proven, battle-tested commands
// (`library_set_cover` / `library_recrawl_cover`): each embeds the image into
// the KFX (preserving the frozen on-device identity) and the derived EPUB,
// refreshes the sidecar + gallery thumbnail, and emits `library:row-updated` so
// the gallery stays in step — the editor just drives them and repaints.
async function renderCoverPanel() {
  const center = $("#editor-center");
  center.replaceChildren(el("div", "editor-panel editor-muted", "Loading cover…"));
  let path;
  try {
    path = await window.api.invoke("library_cover_path", { bookId: session.bookId });
  } catch (err) {
    center.replaceChildren(wrapPanel(el("div", "editor-notice", `Couldn't load the cover: ${err}`)));
    return;
  }
  if (session.panel !== "cover") return; // navigated away while loading
  paintCoverPanel(path);
}

function paintCoverPanel(coverPath) {
  session.coverPath = coverPath;
  const pdfCover = session.data.format === "pdf";
  const panel = el("div", "editor-panel");
  // For a PDF this preview *is* the book's first page (the library tile is a
  // render of it), and seeing it is how you decide between Replace and Insert —
  // so name it for what it is rather than "Cover image".
  panel.append(
    el("div", "field-group-title", pdfCover ? "Current first page" : "Cover image"),
  );

  const preview = el("div", "cover-preview");
  if (coverPath) {
    const img = el("img", "cover-art");
    // Cache-bust: the sidecar path is stable across swaps, so force a re-fetch.
    img.src = `${window.api.fileUrl(coverPath)}?v=${Date.now()}`;
    img.alt = pdfCover ? "The book's current first page" : "Current cover";
    preview.append(img);
  } else {
    preview.append(el("div", "cover-empty", pdfCover ? "No preview" : "No cover set"));
  }
  panel.append(preview);

  // A PDF's cover isn't an embeddable resource like the EPUB/KFX one — it *is*
  // the book's first page. So the choice is which page edit to make, and the
  // wording says so rather than pretending it's the same operation.
  const pdf = session.data.format === "pdf";
  panel.append(
    el(
      "p",
      "editor-muted",
      pdf
        ? "A PDF's cover is its first page. Replace that page with an image, or insert " +
          "a new first page if the book opens straight onto its text. The library tile " +
          "and the Kindle file follow from it."
        : "The cover is embedded in the KFX — the on-device home tile and sleep-screen art — " +
          "and written to the library, keeping the same on-device identity.",
    ),
  );

  const actions = el("div", "cover-actions");
  if (pdf) {
    const replace = el("button", "btn btn-primary", "Replace first page…");
    replace.type = "button";
    replace.title = "Overwrite the book's existing first page with an image";
    replace.addEventListener("click", () => setPdfCover("replace"));
    const insert = el("button", "btn", "Insert cover page…");
    insert.type = "button";
    insert.title = "Add a new first page in front of the book (page count grows by one)";
    insert.addEventListener("click", () => setPdfCover("insert"));
    actions.append(replace, insert);
  } else {
    const change = el("button", "btn btn-primary", "Change cover…");
    change.type = "button";
    change.addEventListener("click", changeCover);
    actions.append(change);

    const asin = session.data.metadata.asin;
    if (asin) {
      const refetch = el("button", "btn", "Re-fetch from Amazon");
      refetch.type = "button";
      refetch.title = `Fetch the cover by ASIN ${asin}`;
      refetch.addEventListener("click", refetchCover);
      actions.append(refetch);
    }
  }
  panel.append(actions);
  $("#editor-center").replaceChildren(panel);
}

// Pick an image and write it into the PDF as its cover page. `mode` is
// "replace" (overwrite page 1) or "insert" (add a new page 1).
async function setPdfCover(mode) {
  let src;
  try {
    src = await window.api.invoke("library_pick_image");
  } catch (err) {
    toast(`Couldn't open the picker: ${err}`, true);
    return;
  }
  if (!src) return; // cancelled
  await runCoverWrite(() =>
    window.api.invoke("editor_set_pdf_cover", {
      bookId: session.bookId,
      srcPath: src,
      mode,
    }),
  );
}

async function changeCover() {
  let src;
  try {
    src = await window.api.invoke("library_pick_image");
  } catch (err) {
    toast(`Couldn't pick an image: ${err}`, true);
    return;
  }
  if (!src) return; // cancelled
  await runCoverWrite(() =>
    window.api.invoke("library_set_cover", { bookId: session.bookId, srcPath: src }),
  );
}

async function refetchCover() {
  await runCoverWrite(() =>
    window.api.invoke("library_recrawl_cover", { bookId: session.bookId }),
  );
}

// Shared commit wrapper for both cover write paths. Both return a tagged result:
// {kind:"updated", cover_path} | {kind:"no_asin"} | {kind:"failed", error}.
async function runCoverWrite(invoke) {
  const buttons = $("#editor-center").querySelectorAll(".cover-actions button");
  buttons.forEach((b) => (b.disabled = true));
  try {
    const res = await invoke();
    if (res.kind === "updated") {
      session.data.has_cover = true;
      paintCoverPanel(res.cover_path); // repaint re-enables the buttons
      toast("Cover updated.");
      return;
    }
    if (res.kind === "no_asin") {
      toast("This book has no Amazon ASIN to fetch from.", true);
    } else {
      toast(`Couldn't update the cover: ${res.error}`, true);
    }
  } catch (err) {
    toast(`Couldn't update the cover: ${err}`, true);
  }
  buttons.forEach((b) => (b.disabled = false));
}

// --- images panel ----------------------------------------------------------

// KFX and EPUB carry a list of embedded images to pull out. A PDF doesn't: its
// pages *are* its images, so it gets a different panel entirely — a page grid
// you select from and export (see `renderPdfPagesPanel`).

async function renderImagesPanel() {
  if (session.data.format === "pdf") return renderPdfPagesPanel();
  const center = $("#editor-center");
  center.replaceChildren(el("div", "editor-panel editor-muted", "Extracting images…"));
  let images;
  try {
    images = await window.api.invoke("editor_images", { bookId: session.bookId });
  } catch (err) {
    center.replaceChildren(wrapPanel(el("div", "editor-notice", `Couldn't read images: ${err}`)));
    return;
  }
  if (session.panel !== "images") return; // navigated away while loading
  session.images = images;
  paintImagesPanel(images);
}

function paintImagesPanel(images) {
  const panel = el("div", "editor-panel");
  panel.append(
    el("div", "field-group-title", `Embedded images${images.length ? ` (${images.length})` : ""}`),
  );
  if (!images.length) {
    panel.append(el("p", "editor-muted", "This book has no embedded images."));
    $("#editor-center").replaceChildren(panel);
    return;
  }

  const actions = el("div", "images-actions");
  const exportAll = el("button", "btn btn-primary", "Export all…");
  exportAll.type = "button";
  exportAll.addEventListener("click", exportAllImages);
  actions.append(exportAll);
  panel.append(actions);

  const grid = el("div", "images-grid");
  for (const img of images) grid.append(imageCard(img));
  panel.append(grid);
  $("#editor-center").replaceChildren(panel);
}

function imageCard(img) {
  const card = el("div", "image-card");

  const thumb = el("div", "image-thumb");
  const preview = el("img");
  preview.src = window.api.fileUrl(img.preview_path);
  preview.alt = img.resource_name;
  preview.loading = "lazy";
  thumb.append(preview);
  if (img.is_cover) thumb.append(el("span", "image-badge", "Cover"));
  card.append(thumb);

  const meta = el("div", "image-meta");
  meta.append(el("div", "image-name", img.resource_name));
  const dims = img.width && img.height ? `${img.width}×${img.height} · ` : "";
  meta.append(
    el("div", "image-dims", `${dims}${img.ext.toUpperCase()} · ${formatBytes(img.byte_len)}`),
  );
  card.append(meta);

  const save = el("button", "btn btn-small", "Save…");
  save.type = "button";
  save.addEventListener("click", () => exportOneImage(img, save));
  card.append(save);
  return card;
}

function formatBytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

async function exportOneImage(img, button) {
  button.disabled = true;
  try {
    const saved = await window.api.invoke("editor_export_image", {
      bookId: session.bookId,
      index: img.index,
    });
    if (saved) toast(`Saved ${img.resource_name}.`);
  } catch (err) {
    toast(`Couldn't save the image: ${err}`, true);
  } finally {
    button.disabled = false;
  }
}

async function exportAllImages() {
  let dir;
  try {
    dir = await window.api.invoke("library_pick_folder");
  } catch (err) {
    toast(`Couldn't pick a folder: ${err}`, true);
    return;
  }
  if (!dir) return; // cancelled
  try {
    const res = await window.api.invoke("editor_export_images", {
      bookId: session.bookId,
      dir,
    });
    toast(`Exported ${res.count} image${res.count === 1 ? "" : "s"}.`);
  } catch (err) {
    toast(`Couldn't export images: ${err}`, true);
  }
}

// --- PDF pages panel -------------------------------------------------------

// The PDF arm of Images: a grid of pages to select and export. Thumbnails load
// lazily — a scanned novel runs to hundreds of pages, and rendering them all up
// front would stall the panel for seconds to draw what's mostly off-screen.

const PDF_EXPORT_DPI = [150, 300, 600];
const PDF_THUMB_WIDTH = 200;

async function renderPdfPagesPanel() {
  const center = $("#editor-center");
  center.replaceChildren(el("div", "editor-panel editor-muted", "Reading pages…"));
  let pages;
  try {
    pages = await window.api.invoke("editor_pdf_pages", { bookId: session.bookId });
  } catch (err) {
    center.replaceChildren(wrapPanel(el("div", "editor-notice", `Couldn't read pages: ${err}`)));
    return;
  }
  if (session.panel !== "images") return; // navigated away while loading
  session.pdfPages = { pages, selected: new Set(), dpi: 300, format: "jpeg" };
  paintPdfPagesPanel();
}

// Painted once. Selection and DPI changes patch the affected nodes in place
// rather than repainting: a repaint would drop every rendered thumbnail and
// re-render it, so ticking one checkbox would cost as much as opening the panel.
function paintPdfPagesPanel() {
  const st = session.pdfPages;
  const panel = el("div", "editor-panel");
  panel.append(el("div", "field-group-title", `Pages (${st.pages.length})`));
  panel.append(
    el("p", "editor-muted", "A PDF page is an image. Pick the pages you want and export them."),
  );

  const actions = el("div", "images-actions page-export-bar");
  actions.append(
    labeledSelect(
      "Resolution",
      PDF_EXPORT_DPI.map((d) => [d, `${d} DPI`]),
      st.dpi,
      (v) => {
        st.dpi = Number(v);
        for (const p of st.pages) st.dims.get(p.page).textContent = pageDims(p, st.dpi);
      },
    ),
  );
  actions.append(
    labeledSelect(
      "Format",
      [
        ["jpeg", "JPEG (smaller)"],
        ["png", "PNG (lossless)"],
      ],
      st.format,
      (v) => {
        st.format = v;
      },
    ),
  );

  const selectAll = el("button", "btn btn-small", "Select all");
  selectAll.type = "button";
  selectAll.addEventListener("click", () => setAllPdfPagesSelected(true));
  const clear = el("button", "btn btn-small", "Clear");
  clear.type = "button";
  clear.addEventListener("click", () => setAllPdfPagesSelected(false));

  const exportBtn = el("button", "btn btn-primary", "Export…");
  exportBtn.type = "button";
  exportBtn.addEventListener("click", exportSelectedPdfPages);

  actions.append(selectAll, clear, exportBtn);
  panel.append(actions);

  const grid = el("div", "images-grid");
  st.dims = new Map(); // page -> the dims node its DPI label lives in
  st.checks = new Map(); // page -> its checkbox
  for (const p of st.pages) grid.append(pdfPageCard(p, st));
  panel.append(grid);

  st.el = { clear, exportBtn };
  syncPdfExportBar();
  $("#editor-center").replaceChildren(panel);
  observePdfThumbs(grid);
}

// The two controls that depend on how many pages are ticked.
function syncPdfExportBar() {
  const st = session.pdfPages;
  const n = st.selected.size;
  st.el.clear.disabled = n === 0;
  st.el.exportBtn.disabled = n === 0;
  st.el.exportBtn.textContent = n ? `Export ${n} page${n === 1 ? "" : "s"}…` : "Export…";
}

function setAllPdfPagesSelected(on) {
  const st = session.pdfPages;
  st.selected.clear();
  if (on) st.pages.forEach((p) => st.selected.add(p.page));
  for (const check of st.checks.values()) check.checked = on;
  syncPdfExportBar();
}

// What one page exports as at `dpi` — points are 1/72", same maths as the
// backend's `render_page`.
function pageDims(p, dpi) {
  const px = (pt) => Math.round((pt * dpi) / 72);
  return `${px(p.width_pt)}×${px(p.height_pt)}`;
}

// A <label>Text <select>…</select></label> pair for the export bar.
function labeledSelect(label, options, value, onChange) {
  const wrap = el("label", "page-export-field");
  wrap.append(el("span", null, label));
  const sel = el("select", "input-small");
  for (const [v, text] of options) {
    const opt = el("option", null, text);
    opt.value = v;
    if (String(v) === String(value)) opt.selected = true;
    sel.append(opt);
  }
  sel.addEventListener("change", () => onChange(sel.value));
  wrap.append(sel);
  return wrap;
}

function pdfPageCard(p, st) {
  const card = el("div", "image-card");

  const thumb = el("div", "image-thumb");
  const img = el("img");
  img.alt = `Page ${p.page}`;
  img.dataset.page = p.page; // the lazy loader's handle
  thumb.append(img);
  if (p.page === 1) thumb.append(el("span", "image-badge", "Cover"));
  card.append(thumb);

  const meta = el("div", "image-meta");
  const name = el("label", "image-name page-card-name");
  const check = el("input");
  check.type = "checkbox";
  check.checked = st.selected.has(p.page);
  check.addEventListener("change", () => {
    if (check.checked) st.selected.add(p.page);
    else st.selected.delete(p.page);
    syncPdfExportBar();
  });
  st.checks.set(p.page, check);
  name.append(check, el("span", null, `Page ${p.page}`));
  meta.append(name);

  const dims = el("div", "image-dims", pageDims(p, st.dpi));
  st.dims.set(p.page, dims);
  meta.append(dims);
  card.append(meta);

  const save = el("button", "btn btn-small", "Save…");
  save.type = "button";
  save.addEventListener("click", () => savePdfPage(p.page, save));
  card.append(save);
  return card;
}

// Render each page's thumbnail only once its card nears the viewport. Each
// render is a ~15ms PDFKit call, so a few dozen on screen is nothing, while
// eagerly doing all of them would not be.
function observePdfThumbs(grid) {
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (!e.isIntersecting) continue;
        io.unobserve(e.target); // one render per card, ever
        loadPdfThumb(e.target);
      }
    },
    { root: null, rootMargin: "300px" },
  );
  for (const img of grid.querySelectorAll("img[data-page]")) io.observe(img);
}

async function loadPdfThumb(img) {
  const page = Number(img.dataset.page);
  try {
    // `reader_pdf_page` is 0-based and stateless — it re-resolves the PDF per
    // call rather than needing an open reader session.
    const b64 = await window.api.invoke("reader_pdf_page", {
      bookId: session.bookId,
      page: page - 1,
      width: PDF_THUMB_WIDTH,
    });
    if (!img.isConnected) return; // panel repainted or closed mid-render
    img.src = `data:image/jpeg;base64,${b64}`;
  } catch {
    // A page that won't render shouldn't take the grid down with it; the empty
    // frame is the message.
  }
}

async function savePdfPage(page, button) {
  const st = session.pdfPages;
  button.disabled = true;
  try {
    const saved = await window.api.invoke("editor_export_pdf_page", {
      bookId: session.bookId,
      page,
      dpi: st.dpi,
      format: st.format,
    });
    if (saved) toast(`Saved page ${page}.`);
  } catch (err) {
    toast(`Couldn't save page ${page}: ${err}`, true);
  }
  button.disabled = false;
}

async function exportSelectedPdfPages() {
  const st = session.pdfPages;
  const pages = [...st.selected].sort((a, b) => a - b);
  if (!pages.length) return;
  let dir;
  try {
    dir = await window.api.invoke("library_pick_folder");
  } catch (err) {
    toast(`Couldn't pick a folder: ${err}`, true);
    return;
  }
  if (!dir) return; // cancelled
  toast(`Exporting ${pages.length} page${pages.length === 1 ? "" : "s"}…`);
  try {
    const res = await window.api.invoke("editor_export_pdf_pages", {
      bookId: session.bookId,
      pages,
      dir,
      dpi: st.dpi,
      format: st.format,
    });
    toast(`Exported ${res.count} page${res.count === 1 ? "" : "s"}.`);
  } catch (err) {
    toast(`Couldn't export pages: ${err}`, true);
  }
}

// --- table-of-contents panel ----------------------------------------------

// Tiny DOM builder — avoids innerHTML for anything carrying a book's own text
// (chapter labels), which is untrusted.
function el(tag, className, text) {
  const n = document.createElement(tag);
  if (className) n.className = className;
  if (text != null) n.textContent = text;
  return n;
}

async function renderTocPanel() {
  const center = $("#editor-center");
  center.replaceChildren(el("div", "editor-panel editor-muted", "Analyzing the table of contents…"));
  let detail;
  try {
    detail = await window.api.invoke("editor_toc", { bookId: session.bookId });
  } catch (err) {
    center.replaceChildren(wrapPanel(el("div", "editor-notice", `Couldn't read the TOC: ${err}`)));
    return;
  }
  if (session.panel !== "toc") return; // navigated away while loading
  session.tocDetail = detail;
  paintTocPanel(detail);
}

function wrapPanel(...children) {
  const p = el("div", "editor-panel");
  p.append(...children);
  return p;
}

function paintTocPanel(detail) {
  // The editable model — a deep copy so edits and Reset don't mutate the cached
  // detail. For KFX/EPUB this is a proposal that keeps the book's own nesting (a
  // flat Contents page → flat, a Part→chapter page → nested); we never reshape
  // it. For PDF it's the book's existing outline, or empty.
  session.tocTree = JSON.parse(JSON.stringify(detail.proposed || []));

  // PDF has no proposer, so its panel is a hand-authoring surface instead: each
  // row targets a page number the user types, and rows can be added.
  const pdf = detail.page_count != null;
  const pageCount = detail.page_count || 0;

  const panel = el("div", "editor-panel");

  const summary = pdf
    ? detail.nav_count
      ? `This PDF declares ${detail.nav_count} bookmark${detail.nav_count === 1 ? "" : "s"}. Edit them below, or add more.`
      : `This PDF has no table of contents. Add entries below — each jumps to a page in 1–${pageCount}.`
    : detail.verdict === "OK"
      ? "This book's table of contents already lists its chapters."
      : detail.verdict === "SUSPECT"
        ? `The declared table of contents looks deficient — ${detail.nav_count} entr${
            detail.nav_count === 1 ? "y" : "ies"
          }, none of them chapters, though the book itself carries a chapter list.`
        : detail.verdict === "FLATTENED"
          ? `This book's ${detail.flattened_volumes} volumes and their chapters are all listed at one depth. Rebuilding nests ${detail.flattened_entries} entr${
              detail.flattened_entries === 1 ? "y" : "ies"
            } under the volume each belongs to.`
          : "No machine-readable chapter list was found in the book.";
  const head = el("div", "toc-summary");
  head.append(el("span", "editor-chip", chipText(detail.verdict)));
  head.querySelector(".editor-chip").dataset.verdict = detail.verdict;
  head.append(el("p", "editor-muted", summary));
  panel.append(head);

  // Current declared TOC.
  panel.append(el("div", "field-group-title", "Currently declared"));
  if (detail.current.length) {
    const ul = el("ul", "toc-current");
    for (const label of detail.current) ul.append(el("li", null, label || "(untitled)"));
    panel.append(ul);
  } else {
    panel.append(el("p", "editor-muted", "No table of contents is declared."));
  }

  const total = countEntries(session.tocTree);
  panel.append(
    el(
      "div",
      "field-group-title",
      pdf
        ? `Table of contents${total ? ` (${total})` : ""}`
        : `Proposed${total ? ` (${total})` : ""}`,
    ),
  );

  // PDF always gets the editable surface (that's how an absent TOC gets
  // authored); KFX/EPUB only when there's a proposal to review.
  if (total || pdf) {
    if (!total && detail.note) {
      panel.append(el("div", "editor-notice", detail.note));
    }
    const tree = el("div", "toc-proposed");
    tree.id = "toc-tree";
    panel.append(tree);

    const actions = el("div", "toc-actions");
    if (pdf) {
      const add = el("button", "btn", "Add entry");
      add.type = "button";
      add.title = "Append a new top-level entry";
      add.addEventListener("click", () => {
        session.tocTree.push({ label: "", eid: 0, href: "", page: 1, children: [] });
        // Must pass pageCount — a bare re-render drops every row's page input.
        renderProposedTree(pageCount);
        // Focus the row just added so typing can start immediately.
        const rows = $("#toc-tree").querySelectorAll(".toc-label");
        rows[rows.length - 1]?.focus();
      });
      actions.append(add);
    }
    const apply = el("button", "btn btn-primary", "Apply table of contents");
    apply.type = "button";
    apply.addEventListener("click", applyToc);
    actions.append(apply);
    if (detail.can_auto_repair) {
      const auto = el("button", "btn", "Repair automatically");
      auto.type = "button";
      auto.title = "Re-derive the TOC and write it in one step, ignoring edits above";
      auto.addEventListener("click", autoRepairToc);
      actions.append(auto);
    }
    const reset = el("button", "btn", "Reset");
    reset.type = "button";
    reset.addEventListener("click", () => paintTocPanel(session.tocDetail));
    actions.append(reset);
    panel.append(actions);

    $("#editor-center").replaceChildren(panel);
    renderProposedTree(pageCount);
  } else {
    panel.append(el("div", "editor-notice", detail.note || "No table of contents could be proposed."));
    $("#editor-center").replaceChildren(panel);
  }
}

function chipText(verdict) {
  if (verdict === "OK") return "TOC OK";
  if (verdict === "SUSPECT") return "TOC deficient";
  // A multi-work book (合本版) whose volumes and their chapters are all listed
  // at one depth — the TOC lists everything, just without its levels.
  if (verdict === "FLATTENED") return "TOC flattened";
  return "TOC sparse";
}

// Render #toc-tree from session.tocTree, indenting by depth so the book's Part→
// chapter structure is visible. A full re-render after each removal keeps every
// row's splice closure bound to the right (array, index). `pageCount > 0` puts
// rows in PDF mode, adding a page-number input.
function renderProposedTree(pageCount = 0) {
  const tree = $("#toc-tree");
  if (!tree) return;
  tree.replaceChildren();
  const walk = (nodes, depth) => {
    nodes.forEach((node, i) => {
      tree.append(
        tocRow(node, depth, pageCount, () => {
          nodes.splice(i, 1);
          renderProposedTree(pageCount);
        }),
      );
      if (node.children && node.children.length) walk(node.children, depth + 1);
    });
  };
  walk(session.tocTree, 0);
}

// One editable row bound to its model node: a label input (writes back to the
// node), a page input for PDF (the only user-editable target — KFX eids and EPUB
// hrefs round-trip opaquely), and a remove button (splices the node — and its
// sub-entries — out).
function tocRow(node, depth, pageCount, onRemove) {
  const row = el("div", "toc-row");
  if (depth) {
    row.style.marginLeft = `${depth * 20}px`;
    row.classList.add("toc-child");
  }
  const input = el("input", "toc-label");
  input.value = node.label;
  input.placeholder = "Chapter title";
  input.addEventListener("input", () => {
    node.label = input.value;
  });
  row.append(input);

  if (pageCount > 0) {
    const page = el("input", "toc-page");
    page.type = "number";
    page.min = "1";
    page.max = String(pageCount);
    page.value = String(node.page || 1);
    page.title = `Page this entry jumps to (1–${pageCount})`;
    page.addEventListener("input", () => {
      // Clamp here so an out-of-range page can't reach the writer, which would
      // reject the whole TOC over one bad row.
      const n = Math.min(Math.max(parseInt(page.value, 10) || 1, 1), pageCount);
      node.page = n;
    });
    page.addEventListener("blur", () => {
      page.value = String(node.page || 1);
    });
    row.append(el("span", "toc-page-label", "p."), page);
  }

  const remove = el("button", "toc-remove", "×");
  remove.type = "button";
  remove.title = node.children && node.children.length
    ? "Remove this entry and its sub-entries"
    : "Remove this entry";
  remove.addEventListener("click", onRemove);
  row.append(remove);
  return row;
}

function countEntries(nodes) {
  return nodes.reduce((n, e) => n + 1 + countEntries(e.children || []), 0);
}

function anyBlankLabel(nodes) {
  return nodes.some((e) => !e.label.trim() || anyBlankLabel(e.children || []));
}

async function applyToc() {
  if (!session.tocTree.length) {
    toast("Add at least one entry.", true);
    return;
  }
  if (anyBlankLabel(session.tocTree)) {
    toast("Every entry needs a label.", true);
    return;
  }
  await runTocWrite(() =>
    window.api.invoke("editor_set_toc", {
      bookId: session.bookId,
      entries: session.tocTree,
    }),
  );
}

async function autoRepairToc() {
  await runTocWrite(() =>
    window.api.invoke("editor_repair_toc", { bookId: session.bookId }),
  );
}

// Shared commit wrapper for both TOC write paths: disable the buttons, invoke,
// then repaint from the fresh verdict + refresh the top-bar chip.
async function runTocWrite(invoke) {
  const buttons = $("#editor-center").querySelectorAll(".toc-actions button");
  buttons.forEach((b) => (b.disabled = true));
  try {
    const detail = await invoke();
    session.tocDetail = detail;
    renderTocChip({
      verdict: detail.verdict,
      nav_count: detail.nav_count,
      nav_chapters: detail.nav_chapters,
    });
    paintTocPanel(detail);
    // Every source format re-derives its counterpart on save; name the right one.
    const derived = session.data.format === "kfx" ? "EPUB" : "Kindle file";
    toast(
      detail.verdict === "OK"
        ? `Table of contents written — regenerating the derived ${derived}…`
        : "Table of contents written.",
    );
  } catch (err) {
    toast(`Couldn't write the TOC: ${err}`, true);
    buttons.forEach((b) => (b.disabled = false));
  }
}

// --- panel dispatch --------------------------------------------------------

function saveCurrentPanel() {
  if (session.panel === "metadata") return saveMetadata();
}

function revertCurrentPanel() {
  if (session.panel === "metadata") renderMetadataPanel();
  markDirty(false);
}

// --- wiring ----------------------------------------------------------------

function wire() {
  $("#editor-close").addEventListener("click", () => close());
  $("#editor-save").addEventListener("click", () => saveCurrentPanel());
  $("#editor-revert").addEventListener("click", () => revertCurrentPanel());
  for (const item of document.querySelectorAll(".editor-rail-item")) {
    item.addEventListener("click", () => {
      if (!item.disabled) selectPanel(item.dataset.panel);
    });
  }
}

wire();

window.sidleEditor = { open, close, isOpen };
