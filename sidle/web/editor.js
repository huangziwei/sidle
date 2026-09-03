// Book editor: a full-screen surface over `#editor-view`, one panel per rail item.

import { mountTextPanel } from "./editor-text-panel.js";

const $ = (sel) => document.querySelector(sel);
const toast = (msg, isError) => window.showToast?.(msg, isError);

let textPanel = null;

// Live editor session, or null when closed. `open` snapshots the opening
// values; Revert restores them and Save diffs against them.
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
  // Focus the first field.
  requestAnimationFrame(() => $("#editor-center input")?.focus());
}

// Returns true if the editor is closed, false if the user kept unsaved edits.
function close() {
  if (!session) return true;
  if (session.dirty && !confirm("Discard unsaved changes?")) return false;
  removeKeys();
  unmountTextPanel();
  view().hidden = true;
  $("#editor-center").replaceChildren();
  session = null;
  return true;
}

function unmountTextPanel() {
  if (!textPanel) return;
  textPanel.destroy();
  textPanel = null;
}

function isOpen() {
  return !!session && !view().hidden;
}

// --- keyboard --------------------------------------------------------------

let keyHandler = null;

function installKeys() {
  keyHandler = (e) => {
    if (textPanel?.onKey(e)) return;
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

// Enable the rail items `session.data.panels` names; every other item stays disabled.
function configureRail() {
  const editable = session.data.editable;
  // `panels` is the backend's list of what this source format can back.
  const panels = new Set(session.data.panels || []);
  for (const item of document.querySelectorAll(".editor-rail-item")) {
    const p = item.dataset.panel;
    item.disabled = !(editable && panels.has(p));
    if (!editable || panels.has(p)) {
      item.title = "";
    } else if (p === "text") {
      item.title = "Text and style editing writes to an EPUB source.";
    } else if (p === "spine") {
      item.title =
        session.data.format === "pdf"
          ? "A PDF's reading order is its page order, which this editor doesn't rearrange."
          : "A Kindle file's reading order carries every reading position with it, so reordering it is a rebuild rather than a reorder — not built yet.";
    } else {
      item.title = "";
    }
  }
}

function selectPanel(panel) {
  // Switching away from the metadata panel drops its unsaved edits.
  if (
    session.panel !== panel &&
    session.dirty &&
    !confirm("Discard unsaved changes to this panel?")
  ) {
    return;
  }
  session.panel = panel;
  session.dirty = false;
  unmountTextPanel();
  for (const item of document.querySelectorAll(".editor-rail-item")) {
    item.classList.toggle("active", item.dataset.panel === panel);
  }
  if (panel === "text") {
    markDirty(false);
    textPanel = mountTextPanel({
      bookId: session.bookId,
      center: $("#editor-center"),
      toast,
      onDirty: (dirty) => markDirty(dirty),
      onSaved: (res) => {
        if (res.toc) renderTocChip(res.toc);
      },
      showPanel: selectPanel,
    });
    return;
  }
  // Every panel except Metadata commits via its own in-panel buttons; the
  // top-bar Save/Revert belong to the metadata panel.
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
  } else if (panel === "spine") {
    $("#editor-save").disabled = true;
    $("#editor-revert").disabled = true;
    renderSpinePanel();
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

  // All three source formats are editable; the only way here is a missing
  // source file.
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
      <span>Amazon ASIN</span>
      <input name="amazon_asin" placeholder="B01ABCDEFG" spellcheck="false"
             ${editable ? "" : "readonly"} />
    </label>
    <small class="field-hint">
      Names this book in Amazon's catalogue. Used only to fetch the color cover —
      it is never written into the file.
    </small>
    <label class="field">
      <span>Content ID</span>
      <input name="content_id" readonly tabindex="-1" spellcheck="false" />
    </label>
    <small class="field-hint">
      The id baked into this file. The Kindle keys its library entry, reading
      position and notebooks on it, so it's the file's to state, not yours to choose.
    </small>
  `;

  form.elements.title.value = m.title || "";
  form.elements.author.value = m.author || "";
  form.elements.publisher.value = m.publisher || "";
  form.elements.published_at.value = m.published_at || "";
  form.elements.language.value = m.language || "";
  form.elements.amazon_asin.value = m.amazon_asin || "";
  form.elements.content_id.value = m.content_id || "";

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
    // The content id is shown, not submitted: it belongs to the file.
    amazon_asin: val("amazon_asin") || null,
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
      amazon_asin: row.amazon_asin,
      content_id: row.asin,
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

// The cover flow runs on the library's cover commands (`library_set_cover`, fetch, clear).
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
  // For a PDF this preview is the book's first page, which the library tile
  // renders; the heading names it as the first page.
  panel.append(
    el("div", "field-group-title", pdfCover ? "Current first page" : "Cover image"),
  );

  const preview = el("div", "cover-preview");
  if (coverPath) {
    const img = el("img", "cover-art");
    // Cache-bust: the sidecar path is stable across swaps.
    img.src = `${window.api.fileUrl(coverPath)}?v=${Date.now()}`;
    img.alt = pdfCover ? "The book's current first page" : "Current cover";
    preview.append(img);
  } else {
    preview.append(el("div", "cover-empty", pdfCover ? "No preview" : "No cover set"));
  }
  panel.append(preview);

  // A PDF's cover is the book's first page, not an embeddable resource; the
  // choice is which page edit to make.
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

    // Fetching is by catalogue id: the file's own identity names nothing on
    // Amazon.
    const asin = session.data.metadata.amazon_asin;
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

// KFX and EPUB carry a list of embedded images. A PDF's pages are its images:
// `renderPdfPagesPanel` shows a page grid to select from and export.

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

// The PDF arm of Images: a grid of pages to select and export; thumbnails
// load as their cards near the viewport.

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

// Painted once. Selection and DPI changes patch the affected nodes in place;
// a repaint drops every rendered thumbnail.
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

// Render each page's thumbnail once its card nears the viewport; each render
// is a ~15ms PDFKit call.
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
    // `reader_pdf_page` is 0-based and stateless: it re-resolves the PDF per call.
    const b64 = await window.api.invoke("reader_pdf_page", {
      bookId: session.bookId,
      page: page - 1,
      width: PDF_THUMB_WIDTH,
    });
    if (!img.isConnected) return; // panel repainted or closed mid-render
    img.src = `data:image/jpeg;base64,${b64}`;
  } catch {
    // A page that fails to render leaves an empty frame; the grid stands.
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
  // The editable model: a deep copy of the cached detail.
  session.tocTree = JSON.parse(JSON.stringify(detail.proposed || []));

  // PDF has no proposer: its panel is a hand-authoring surface; each row
  // targets a typed page number, and rows can be added.
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

  // Current declared TOC, with the levels the book declares.
  panel.append(el("div", "field-group-title", "Currently declared"));
  if (detail.current.length) {
    panel.append(declaredList(detail.current));
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
  if (!pdf && total) {
    panel.append(el("p", "editor-muted", proposalSummary(detail, session.tocTree)));
  }

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
        // Focus the row just added.
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

// The declared TOC as a nested list, read-only.
function declaredList(nodes) {
  const ul = el("ul", "toc-current");
  for (const node of nodes) {
    const li = el("li", null, node.label || "(untitled)");
    if (node.children && node.children.length) li.append(declaredList(node.children));
    ul.append(li);
  }
  return ul;
}

// What the proposal changes, in one line: added entries and added levels;
// every declared entry stays.
function proposalSummary(detail, tree) {
  const added = countEntries(tree) - detail.nav_count;
  const levels = treeDepth(tree);
  const declaredLevels = treeDepth(detail.current);
  const parts = [];
  if (added > 0) parts.push(`adds ${added} entr${added === 1 ? "y" : "ies"} the book links but doesn't declare`);
  if (levels > declaredLevels) parts.push(`nests them ${levels} levels deep`);
  if (!parts.length) return "Identical to what the book declares — nothing to change.";
  return `Keeps every declared entry and ${parts.join(", ")}.`;
}

function treeDepth(nodes) {
  return (nodes || []).reduce((d, n) => Math.max(d, 1 + treeDepth(n.children)), 0);
}

// The tree as a flat run of `{node, depth}` in reading order.
function flattenTree(nodes, depth = 0, out = []) {
  for (const node of nodes) {
    out.push({ node, depth });
    flattenTree(node.children || [], depth + 1, out);
  }
  return out;
}

// Rebuild a tree from `{node, depth}` rows. Each row attaches under the nearest
// preceding row with a shallower depth; a depth deeper than one below its
// predecessor clamps.
function rebuildTree(rows) {
  const roots = [];
  const open = []; // ancestors of the current row, outermost first
  for (const { node, depth } of rows) {
    node.children = [];
    while (open.length > depth) open.pop();
    if (open.length) open[open.length - 1].children.push(node);
    else roots.push(node);
    open.push(node);
  }
  return roots;
}

// The rows of `index`'s subtree: itself plus everything after it that is deeper.
function subtreeEnd(rows, index) {
  let end = index + 1;
  while (end < rows.length && rows[end].depth > rows[index].depth) end++;
  return end;
}

// Move entry `index` (and its sub-entries) one level in or out.
function shiftTocDepth(index, delta, pageCount) {
  const rows = flattenTree(session.tocTree);
  const row = rows[index];
  if (!row || !canShift(rows, index, delta)) return;
  const end = subtreeEnd(rows, index);
  for (let i = index; i < end; i++) rows[i].depth += delta;
  session.tocTree = rebuildTree(rows);
  renderProposedTree(pageCount, index);
}

// An entry can only indent under something, and can only outdent while it has a
// level to give up.
function canShift(rows, index, delta) {
  if (delta < 0) return rows[index].depth > 0;
  return index > 0 && rows[index].depth <= rows[index - 1].depth;
}

// Where `index`'s previous sibling starts, or -1 when it is the first child.
function prevSibling(rows, index) {
  const depth = rows[index].depth;
  for (let j = index - 1; j >= 0; j--) {
    if (rows[j].depth === depth) return j;
    if (rows[j].depth < depth) return -1;
  }
  return -1;
}

// Where `index`'s next sibling starts, or -1 when it is the last child. The row
// after this subtree is either the next sibling (equal depth) or an ancestor's
// continuation (shallower) — `subtreeEnd` guarantees it can't be deeper.
function nextSibling(rows, index) {
  const end = subtreeEnd(rows, index);
  return end < rows.length && rows[end].depth === rows[index].depth ? end : -1;
}

// Move entry `index` (and its sub-entries) past the sibling above or below it.
function moveTocEntry(index, delta, pageCount) {
  const rows = flattenTree(session.tocTree);
  if (!rows[index]) return;
  const target = delta < 0 ? prevSibling(rows, index) : nextSibling(rows, index);
  if (target < 0) return;
  const block = rows.splice(index, subtreeEnd(rows, index) - index);
  // Moving up, the target sits above the cut at an unchanged index. Moving
  // down, the next sibling slides into the vacated slot; the block belongs
  // after the whole of it, sub-entries included.
  const at = delta < 0 ? target : subtreeEnd(rows, index);
  rows.splice(at, 0, ...block);
  session.tocTree = rebuildTree(rows);
  renderProposedTree(pageCount, at);
}

// Render #toc-tree from session.tocTree, indented by depth.
function renderProposedTree(pageCount = 0, focusIndex = -1) {
  const tree = $("#toc-tree");
  if (!tree) return;
  const rows = flattenTree(session.tocTree);
  tree.replaceChildren(
    ...rows.map(({ node, depth }, i) =>
      tocRow(node, depth, pageCount, {
        canIndent: canShift(rows, i, +1),
        canOutdent: canShift(rows, i, -1),
        canMoveUp: prevSibling(rows, i) >= 0,
        canMoveDown: nextSibling(rows, i) >= 0,
        onIndent: () => shiftTocDepth(i, +1, pageCount),
        onOutdent: () => shiftTocDepth(i, -1, pageCount),
        onMoveUp: () => moveTocEntry(i, -1, pageCount),
        onMoveDown: () => moveTocEntry(i, +1, pageCount),
        onRemove: () => {
          rows.splice(i, subtreeEnd(rows, i) - i);
          session.tocTree = rebuildTree(rows);
          renderProposedTree(pageCount);
        },
      }),
    ),
  );
  if (focusIndex >= 0) tree.querySelectorAll(".toc-label")[focusIndex]?.focus();
}

// One editable row bound to its model node: a label input, a page input for PDF, the move controls.
function tocRow(node, depth, pageCount, ops) {
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
  // Tab / Shift+Tab change an entry's level, Alt+↑ / Alt+↓ its order among its siblings.
  input.addEventListener("keydown", (e) => {
    if (e.key === "Tab") {
      const shift = e.shiftKey ? ops.onOutdent : ops.onIndent;
      const allowed = e.shiftKey ? ops.canOutdent : ops.canIndent;
      if (!allowed) return; // fall through to normal focus movement
      e.preventDefault();
      shift();
      return;
    }
    if (!e.altKey || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
    const up = e.key === "ArrowUp";
    if (!(up ? ops.canMoveUp : ops.canMoveDown)) return;
    e.preventDefault();
    (up ? ops.onMoveUp : ops.onMoveDown)();
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
      // Clamp here: the writer rejects the whole TOC over one out-of-range page.
      const n = Math.min(Math.max(parseInt(page.value, 10) || 1, 1), pageCount);
      node.page = n;
    });
    page.addEventListener("blur", () => {
      page.value = String(node.page || 1);
    });
    row.append(el("span", "toc-page-label", "p."), page);
  }

  const sub = node.children && node.children.length ? " and its sub-entries" : "";
  const moveUp = el("button", "toc-nudge", "↑");
  moveUp.type = "button";
  moveUp.title = `Move this entry${sub} up (Alt+↑)`;
  moveUp.disabled = !ops.canMoveUp;
  moveUp.addEventListener("click", ops.onMoveUp);

  const moveDown = el("button", "toc-nudge", "↓");
  moveDown.type = "button";
  moveDown.title = `Move this entry${sub} down (Alt+↓)`;
  moveDown.disabled = !ops.canMoveDown;
  moveDown.addEventListener("click", ops.onMoveDown);

  const outdent = el("button", "toc-nudge", "⇤");
  outdent.type = "button";
  outdent.title = "Move out one level (Shift+Tab)";
  outdent.disabled = !ops.canOutdent;
  outdent.addEventListener("click", ops.onOutdent);

  const indent = el("button", "toc-nudge", "⇥");
  indent.type = "button";
  indent.title = "Make this a sub-entry of the one above (Tab)";
  indent.disabled = !ops.canIndent;
  indent.addEventListener("click", ops.onIndent);

  const remove = el("button", "toc-remove", "×");
  remove.type = "button";
  remove.title = `Remove this entry${sub}`;
  remove.addEventListener("click", ops.onRemove);

  // One cluster: five controls as the row's toolbar.
  const toolbar = el("div", "toc-ops");
  toolbar.append(moveUp, moveDown, outdent, indent, remove);
  row.append(toolbar);
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

// --- reading order (spine) panel -------------------------------------------

async function renderSpinePanel() {
  const center = $("#editor-center");
  center.replaceChildren(
    el("div", "editor-panel editor-muted", "Reading the book's reading order…"),
  );
  let detail;
  try {
    detail = await window.api.invoke("editor_spine", { bookId: session.bookId });
  } catch (err) {
    center.replaceChildren(
      wrapPanel(el("div", "editor-notice", `Couldn't read the reading order: ${err}`)),
    );
    return;
  }
  if (session.panel !== "spine") return; // navigated away while loading
  session.spineDetail = detail;
  paintSpinePanel(detail);
}

function paintSpinePanel(detail) {
  // A deep copy of the cached detail.
  session.spineOrder = JSON.parse(JSON.stringify(detail.proposed || []));

  const panel = el("div", "editor-panel");
  const wrong = detail.verdict === "MISORDERED";

  const head = el("div", "toc-summary");
  const chip = el("span", "editor-chip", wrong ? "Order wrong" : "Order OK");
  chip.dataset.verdict = wrong ? "SUSPECT" : "OK";
  head.append(chip);
  head.append(el("p", "editor-muted", spineSummary(detail)));
  panel.append(head);

  // The consequence, stated before the controls.
  panel.append(
    el(
      "div",
      "editor-notice",
      "Reading positions are numbered along the reading order, so changing it " +
        "renumbers them. Bookmarks, highlights and how far through the book you " +
        "are will shift for this book.",
    ),
  );

  panel.append(el("div", "field-group-title", "Reading order"));
  panel.append(
    el(
      "p",
      "editor-muted",
      "Entries the table of contents names are listed by their chapter title. " +
        "The rest — plates, blanks, pages no chapter list mentions — show their " +
        "filename, and travel with the document above them unless you move them.",
    ),
  );
  const list = el("div", "toc-proposed");
  list.id = "spine-list";
  panel.append(list);

  const actions = el("div", "toc-actions");
  const apply = el("button", "btn btn-primary", "Apply reading order");
  apply.type = "button";
  apply.id = "spine-apply";
  apply.addEventListener("click", applySpine);
  const reset = el("button", "btn", "Reset");
  reset.type = "button";
  reset.title = "Back to the order the book's navigation implies";
  reset.addEventListener("click", () => paintSpinePanel(session.spineDetail));
  actions.append(apply, reset);
  panel.append(actions);

  $("#editor-center").replaceChildren(panel);
  renderSpineList();
}

function spineSummary(detail) {
  if (detail.verdict !== "MISORDERED") {
    return "This book reads its chapters in the order its table of contents lists them.";
  }
  const late = detail.first_out_of_order
    ? `, starting with ${detail.first_out_of_order}`
    : "";
  const sorted = detail.machine_sorted
    ? " Its reading order is its own file list in alphabetical order — a packaging" +
      " artifact rather than an order anyone chose."
    : "";
  return (
    `This book reads its chapters out of the order its table of contents lists` +
    ` them, in ${detail.descents} place${detail.descents === 1 ? "" : "s"}${late}.` +
    ` The order below moves ${detail.moved} document${detail.moved === 1 ? "" : "s"}.` +
    sorted
  );
}

// Full re-render after every move; each row's closure binds its index, the
// same rule the TOC tree follows.
function renderSpineList(focusIndex = -1) {
  const list = $("#spine-list");
  if (!list) return;
  const order = session.spineOrder;
  list.replaceChildren(
    ...order.map((doc, i) =>
      spineRow(doc, {
        canMoveUp: i > 0,
        canMoveDown: i < order.length - 1,
        onMoveUp: () => moveSpineDoc(i, -1),
        onMoveDown: () => moveSpineDoc(i, +1),
      }),
    ),
  );
  const apply = $("#spine-apply");
  if (apply) {
    // The backend refuses an order the book declares; the button stays
    // disabled for it.
    const current = (session.spineDetail.current || []).map((d) => d.idref);
    apply.disabled = order.every((d, i) => d.idref === current[i]);
    apply.title = apply.disabled
      ? "This is the order the book already reads in"
      : "";
  }
  if (focusIndex >= 0) {
    list.querySelectorAll(".toc-nudge")[focusIndex * 2]?.focus();
  }
}

// One row: the document's name (not editable — a document's identity isn't a
// label the reader gets to change) and the two controls that move it.
function spineRow(doc, ops) {
  const row = el("div", "toc-row");
  const name = el("span", doc.named ? "spine-label" : "spine-label spine-unnamed", doc.label);
  name.title = doc.named
    ? `Listed in the table of contents as “${doc.label}”`
    : `${doc.label} — no table-of-contents entry names this document`;
  row.append(name);

  const moveUp = el("button", "toc-nudge", "↑");
  moveUp.type = "button";
  moveUp.title = "Move earlier in the book";
  moveUp.disabled = !ops.canMoveUp;
  moveUp.addEventListener("click", ops.onMoveUp);

  const moveDown = el("button", "toc-nudge", "↓");
  moveDown.type = "button";
  moveDown.title = "Move later in the book";
  moveDown.disabled = !ops.canMoveDown;
  moveDown.addEventListener("click", ops.onMoveDown);

  const ops_ = el("div", "toc-ops");
  ops_.append(moveUp, moveDown);
  row.append(ops_);
  return row;
}

// A spine is flat: a move is a swap with the neighbour.
function moveSpineDoc(index, delta) {
  const order = session.spineOrder;
  const to = index + delta;
  if (to < 0 || to >= order.length) return;
  [order[index], order[to]] = [order[to], order[index]];
  renderSpineList(to);
}

async function applySpine() {
  const order = session.spineOrder.map((d) => d.idref);
  const buttons = $("#editor-center").querySelectorAll(".toc-actions button");
  buttons.forEach((b) => (b.disabled = true));
  try {
    const detail = await window.api.invoke("editor_set_spine", {
      bookId: session.bookId,
      order,
    });
    session.spineDetail = detail;
    paintSpinePanel(detail);
    const derived = session.data.format === "kfx" ? "EPUB" : "Kindle file";
    toast(`Reading order written — regenerating the derived ${derived}…`);
  } catch (err) {
    toast(`Couldn't write the reading order: ${err}`, true);
    buttons.forEach((b) => (b.disabled = false));
  }
}

// --- panel dispatch --------------------------------------------------------

function saveCurrentPanel() {
  if (session.panel === "metadata") return saveMetadata();
  if (session.panel === "text") return textPanel?.save();
}

function revertCurrentPanel() {
  if (session.panel === "metadata") renderMetadataPanel();
  if (session.panel === "text") {
    textPanel?.revert();
    markDirty(textPanel?.isDirty() || false);
    return;
  }
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
