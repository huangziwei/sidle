// Book editor — a Calibre "Edit book"-style surface built into Sidle. Full-screen
// #editor-view, parallel to #reader-view, driven entirely from the built
// boko-kai KFX edit primitives via the `editor_*` Tauri commands. v1 edits
// KFX-source books; the left rail's other panels (Cover / Images / TOC / Text)
// light up in later phases. Exposed as `window.sidleEditor`.

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

// Metadata is always available (read-only for non-KFX sources); Cover, Images
// and Table of Contents light up for KFX-source books. Text (in-place typo
// fixes) needs the surgical text-replace primitive that isn't built yet, so it
// stays gated with an explanatory tooltip.
function configureRail() {
  const editable = session.data.editable;
  const live = new Set(["cover", "images", "toc"]);
  for (const item of document.querySelectorAll(".editor-rail-item")) {
    const p = item.dataset.panel;
    item.disabled = !(p === "metadata" || (editable && live.has(p)));
    if (p === "text") {
      item.title = editable
        ? "In-place text editing is coming in a later tier."
        : "";
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
  chip.textContent =
    toc.verdict === "OK"
      ? "TOC OK"
      : toc.verdict === "SUSPECT"
        ? "TOC deficient"
        : "TOC sparse";
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

  const notice = editable
    ? ""
    : `<div class="editor-notice">Editing ${session.data.format.toUpperCase()} sources isn't
         supported yet — KFX and EPUB books are editable now. These fields are read-only.</div>`;

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
  const panel = el("div", "editor-panel");
  panel.append(el("div", "field-group-title", "Cover image"));

  const preview = el("div", "cover-preview");
  if (coverPath) {
    const img = el("img", "cover-art");
    // Cache-bust: the sidecar path is stable across swaps, so force a re-fetch.
    img.src = `${window.api.fileUrl(coverPath)}?v=${Date.now()}`;
    img.alt = "Current cover";
    preview.append(img);
  } else {
    preview.append(el("div", "cover-empty", "No cover set"));
  }
  panel.append(preview);

  panel.append(
    el(
      "p",
      "editor-muted",
      "The cover is embedded in the KFX — the on-device home tile and sleep-screen art — " +
        "and written to the library, keeping the same on-device identity.",
    ),
  );

  const actions = el("div", "cover-actions");
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
  panel.append(actions);
  $("#editor-center").replaceChildren(panel);
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

async function renderImagesPanel() {
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
  // The editable model — a deep copy of the proposal so edits and Reset don't
  // mutate the cached detail. The proposal keeps the book's own nesting (a flat
  // Contents page → flat, a Part→chapter page → nested); we never reshape it.
  session.tocTree = JSON.parse(JSON.stringify(detail.proposed || []));

  const panel = el("div", "editor-panel");

  // Verdict summary.
  const summary =
    detail.verdict === "OK"
      ? "This book's table of contents already lists its chapters."
      : detail.verdict === "SUSPECT"
        ? `The declared table of contents looks deficient — ${detail.nav_count} entr${
            detail.nav_count === 1 ? "y" : "ies"
          }, none of them chapters, though the book itself carries a chapter list.`
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

  // Proposed TOC (editable, nesting preserved).
  const total = countEntries(session.tocTree);
  panel.append(
    el("div", "field-group-title", `Proposed${total ? ` (${total})` : ""}`),
  );
  if (total) {
    const tree = el("div", "toc-proposed");
    tree.id = "toc-tree";
    panel.append(tree);

    const actions = el("div", "toc-actions");
    const apply = el("button", "btn btn-primary", "Apply table of contents");
    apply.type = "button";
    apply.addEventListener("click", applyToc);
    const auto = el("button", "btn", "Repair automatically");
    auto.type = "button";
    auto.title = "Re-derive the TOC and write it in one step, ignoring edits above";
    auto.addEventListener("click", autoRepairToc);
    const reset = el("button", "btn", "Reset");
    reset.type = "button";
    reset.addEventListener("click", () => paintTocPanel(session.tocDetail));
    actions.append(apply, auto, reset);
    panel.append(actions);

    $("#editor-center").replaceChildren(panel);
    renderProposedTree();
  } else {
    panel.append(el("div", "editor-notice", detail.note || "No table of contents could be proposed."));
    $("#editor-center").replaceChildren(panel);
  }
}

function chipText(verdict) {
  return verdict === "OK" ? "TOC OK" : verdict === "SUSPECT" ? "TOC deficient" : "TOC sparse";
}

// Render #toc-tree from session.tocTree, indenting by depth so the book's Part→
// chapter structure is visible. A full re-render after each removal keeps every
// row's splice closure bound to the right (array, index).
function renderProposedTree() {
  const tree = $("#toc-tree");
  if (!tree) return;
  tree.replaceChildren();
  const walk = (nodes, depth) => {
    nodes.forEach((node, i) => {
      tree.append(
        tocRow(node, depth, () => {
          nodes.splice(i, 1);
          renderProposedTree();
        }),
      );
      if (node.children && node.children.length) walk(node.children, depth + 1);
    });
  };
  walk(session.tocTree, 0);
}

// One editable row bound to its model node: a label input (writes back to the
// node) and a remove button (splices the node — and its sub-entries — out).
function tocRow(node, depth, onRemove) {
  const row = el("div", "toc-row");
  if (depth) {
    row.style.marginLeft = `${depth * 20}px`;
    row.classList.add("toc-child");
  }
  const input = el("input", "toc-label");
  input.value = node.label;
  input.addEventListener("input", () => {
    node.label = input.value;
  });
  const remove = el("button", "toc-remove", "×");
  remove.type = "button";
  remove.title = node.children && node.children.length
    ? "Remove this entry and its sub-entries"
    : "Remove this entry";
  remove.addEventListener("click", onRemove);
  row.append(input, remove);
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
    toast(
      detail.verdict === "OK"
        ? "Table of contents fixed — regenerating the derived EPUB…"
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
