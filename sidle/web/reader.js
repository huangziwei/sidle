// reader.js — the built-in reader coordinator. Replaces foliate's view.js with
// a thin layer over the vendored paginator: open a library book (KFX→DOM via
// the `reader_open` Tauri command), paginate it, and surface imported
// annotations — highlights painted in place, notes with a note cue + popover,
// bookmarks as margin markers and a jump-list. Exposed as `window.sidleReader`
// so the (classic-script) library.js can drive it across the module boundary.

import "./foliate-kfx/paginator.js"; // defines <foliate-paginator>
import { Overlayer } from "./foliate-kfx/overlayer.js";
import { makeKfxBook } from "./foliate-kfx/kfx-book.js";
import { rangeFor, textBoundary, anchorFromRange, baseTextOf } from "./foliate-kfx/anchor.js";

const $ = (sel) => document.querySelector(sel);
const toast = (msg, isError) => window.showToast?.(msg, isError);

let book = null; // current kfx-book
let dto = null; // raw reader_open DTO (kept for the eid→section index)
let bookId = null; // library id of the open book (for reload-on-sync)
let readerMode = "reflowable"; // "reflowable" (KFX→DOM) | "pdf" (fixed-layout page images)
let pdf = null; // PDF-mode state: { pageCount, pages, labels, toc, page, img, token } — see openPdf
let nbk = null; // notebook-mode state: { id, title, pageCount, page, cache, token } — see openNotebook
let paginator = null; // <foliate-paginator>
let annotations = []; // AnnotationDto[] for the open book
let overlays = []; // [{ doc, overlayer }] — one per loaded section, for repaint
let eidToSection = null; // Map<eid, sectionIndex>, built lazily for jumps
let keyHandler = null;
let tocEntries = []; // flat [{ li, sectionIndex }] in TOC order, for active-marking
let locByEid = null; // Map<eid, linear position> — Kindle "Location" per element
let maxLoc = 0; // largest position, denominator for whole-book %
let positionedByDoc = new WeakMap(); // section doc → [{ el, loc }] in doc order (cached)
let sectionChars = []; // base-text char count per section, for time estimates
let charsBefore = []; // cumulative chars before section i
let totalChars = 0;
let readSpeed = 0; // chars/min, learned per book (0 until the first valid sample)
let sampleCount = 0; // valid pace samples for the open book — gates "Estimating…"
let lastTurnTime = null; // ms timestamp of the last relocate, for pace sampling
let lastCharPos = null; // char position at the last relocate, for pace sampling
let lastPos = null; // { loc, index, frac } from the last relocate, for mode cycling
let livePosition = null; // { eid, offset, linear_pos } at the top of the current page — saved on close
let sidleResume = null; // Sidle's last-read spot (frozen this session): auto-restored on open + Resume target
let deviceResumes = []; // [{ eid, offset, linear_pos, device_serial }] — each Kindle's imported .yjf spot; Resume targets, never auto-applied
let progressMode = readSavedProgressMode(); // 0 loc · 1 chapter · 2 book · 3 hidden
let searchResults = []; // SearchMatchDto[] for the current query (intra-eid matches, ordered by linear_pos)
let searchQuery = ""; // the query that produced searchResults (for race-checks)
let searchSeq = 0; // request token — late responses for stale queries are dropped
let searchDebounceTimer = null;
let searchComposing = false; // true between compositionstart/end (JP IME)
let lastPaintedCount = 0; // count from last search-paint, so `clearSearchPaint` knows how many keys to remove
let selectedSearchIndex = -1; // -1 = none picked yet; otherwise the index of the row the user last clicked
let pendingSelection = null; // { doc, anchor, text } for the live text selection — fuels createAnnotation
let editingAnn = null; // the native annotation open in the editable note popover (null = read-only / closed)
let editorColor = "yellow"; // the color currently chosen in the open editor
const DEFAULT_HL_COLOR = "yellow"; // a swatch-less highlight (e.g. from "Note") uses this

function readSavedProgressMode() {
  try {
    const n = Number(localStorage.getItem("sidle.reader.progressMode"));
    return Number.isInteger(n) && n >= 0 && n <= 3 ? n : 0;
  } catch {
    return 0;
  }
}

const view = () => $("#reader-view");

// ---- top bar auto-hide ------------------------------------------------------
// While reading, the top bar fades away after a few idle seconds and comes back
// when you click the top of the page. It keeps its layout slot the whole time
// (see styles.css), so this never resizes the stage. It won't fade out while the
// pointer is over it or while a panel/popover it opened is showing, so it can't
// vanish mid-interaction.
const TOPBAR_HIDE_DELAY = 3000; // ms idle before the bar fades out
let topbarHideTimer = null;
let topbarHovered = false;

const topbarEl = () => $(".reader-topbar");

function cancelTopbarHide() {
  if (topbarHideTimer) {
    clearTimeout(topbarHideTimer);
    topbarHideTimer = null;
  }
}

// Panels/popovers the top bar's buttons summon — while one is open the bar is
// "in use", so it should stay put.
function readerChromeOpen() {
  return [
    "#reader-toc-panel",
    "#reader-annotations-panel",
    "#reader-search-panel",
    "#reader-style-panel",
    "#reader-pdf-style-panel",
    "#reader-resume-menu",
  ].some((sel) => {
    const el = $(sel);
    return el && !el.hidden;
  });
}

function scheduleTopbarHide() {
  cancelTopbarHide();
  topbarHideTimer = setTimeout(() => {
    topbarHideTimer = null;
    // Re-arm instead of hiding if the user is still on the bar or mid-task; the
    // next idle window (or a panel close) lets it fade.
    if (topbarHovered || readerChromeOpen()) {
      scheduleTopbarHide();
      return;
    }
    topbarEl()?.classList.add("is-hidden");
  }, TOPBAR_HIDE_DELAY);
}

function revealTopbar() {
  topbarEl()?.classList.remove("is-hidden");
  scheduleTopbarHide();
}

// Named Kindle highlight colors → CSS; falls back to a literal color or yellow.
const COLORS = { yellow: "#f4d03f", blue: "#5dade2", pink: "#ec7fa9", orange: "#e59866" };
const NOTE_CUE = "#b5651d"; // edge line marking a highlight that carries a note
const BOOKMARK_COLOR = "#e07b39";
const KIND_ICON = { highlight: "🖍", note: "📝", bookmark: "🔖" };

const NS = "http://www.w3.org/2000/svg";
const svgEl = (tag) => document.createElementNS(NS, tag);
const svgRect = (x, y, w, h) => {
  const el = svgEl("rect");
  el.setAttribute("x", x);
  el.setAttribute("y", y);
  el.setAttribute("width", w);
  el.setAttribute("height", h);
  return el;
};

function highlightGroup(color) {
  const g = svgEl("g");
  g.setAttribute("fill", color);
  g.style.opacity = "var(--overlayer-highlight-opacity, .3)";
  g.style.mixBlendMode = "var(--overlayer-highlight-blend-mode, normal)";
  return g;
}

// Draw the rects as-is — their thickness (≈1em cross-axis) is already correct.
// The rects come from `annotationRects` (per element), so each one already stops
// at its element's text rather than running to the line end.
function drawHighlight(rects, options = {}) {
  const g = highlightGroup(options.color || COLORS.yellow);
  for (const { left, top, width, height } of rects) g.append(svgRect(left, top, width, height));
  return g;
}

// A note = the same band + a solid cue line along its trailing edge, so a noted
// highlight reads differently from a plain one. The cue is a sibling group at
// full opacity (not inside the .3-opacity band).
function drawNote(rects, options = {}) {
  const wrap = svgEl("g");
  wrap.append(drawHighlight(rects, options));
  const cue = svgEl("g");
  cue.setAttribute("fill", NOTE_CUE);
  const t = 2;
  for (const { left, top, width, height } of rects) {
    if (options.vertical) cue.append(svgRect(left, top, t, height)); // left edge of the column
    else cue.append(svgRect(left, top + height - t, width, t)); // under the line
  }
  wrap.append(cue);
  return wrap;
}

// A small filled marker at the block-start corner of a bookmark's anchor char,
// so bookmarks are visible on the page (the jump-list is the primary surface).
function drawBookmarkMarker(rects, options = {}) {
  const g = svgEl("g");
  const r = rects[0];
  if (!r) return g;
  const cx = options.vertical ? r.right - 5 : r.left + 5;
  const dot = svgEl("circle");
  dot.setAttribute("cx", cx);
  dot.setAttribute("cy", r.top + 5);
  dot.setAttribute("r", "5");
  dot.setAttribute("fill", options.color || BOOKMARK_COLOR);
  g.append(dot);
  return g;
}

// A range within ONE element's base text (ruby excluded), clamped to its end.
function subRange(doc, el, from, to) {
  const s = textBoundary(el, from);
  const e = textBoundary(el, to); // textBoundary clamps when `to` overruns the element
  if (!s || !e) return null;
  const r = doc.createRange();
  try {
    r.setStart(s.node, s.offset);
    r.setEnd(e.node, e.offset);
  } catch {
    return null;
  }
  return r;
}

// Client rects for an annotation, computed PER `data-eid` element. A single
// Range spanning several block elements makes getClientRects fill the blank tail
// of each element's short last line (it's a continuous selection), which paints
// the highlight onto the whitespace up to the line end. Splitting per element —
// each sub-range ending at its own element's text — stops the band at the text.
// Returns [] when the annotation isn't in this section.
function annotationRects(doc, ann) {
  if (ann.eid_start == null) return [];
  if (readerMode === "pdf") return pdfAnnotationRects(ann);
  const all = [...doc.querySelectorAll("[data-eid]")];
  if (!all.length) return [];
  const startEl = doc.querySelector(`[data-eid="${ann.eid_start}"]`);
  const endEl = ann.eid_end != null ? doc.querySelector(`[data-eid="${ann.eid_end}"]`) : startEl;
  if (!startEl && !endEl) return [];
  let si = startEl ? all.indexOf(startEl) : 0;
  let ei = endEl ? all.indexOf(endEl) : all.length - 1;
  if (si < 0) si = 0;
  if (ei < 0) ei = all.length - 1;
  if (ei < si) return [];
  const rects = [];
  for (let i = si; i <= ei; i++) {
    const el = all[i];
    const from = el === startEl ? (ann.off_start ?? 0) : 0;
    const to = el === endEl ? (ann.off_end ?? 0) + 1 : Number.MAX_SAFE_INTEGER;
    const r = subRange(doc, el, from, to);
    if (r) rects.push(...r.getClientRects());
  }
  return rects;
}

// PDF highlight/search rects, computed from the text-layer **spans' own boxes**
// (`getBoundingClientRect`) rather than a text Range's `getClientRects`. Each
// span is positioned + sized to its KFX run box, which the renderer aligns to
// the page image; the Range path instead returns the transparent fallback font's
// line box, which drifts (notably downward) from the run box. The first/last run
// is clipped horizontally by char-offset proportion — the transparent text isn't
// glyph-for-glyph with the image, so proportional is the closest fit. Spans are
// queried in DOM order (= reading order), so the start→end walk is monotonic.
function pdfAnnotationRects(ann) {
  if (ann.eid_start == null) return [];
  const all = [...document.querySelectorAll(".reader-pdf-text [data-eid]")];
  if (!all.length) return [];
  const startEl = document.querySelector(`.reader-pdf-text [data-eid="${ann.eid_start}"]`);
  const endEl =
    ann.eid_end != null
      ? document.querySelector(`.reader-pdf-text [data-eid="${ann.eid_end}"]`)
      : startEl;
  let si = startEl ? all.indexOf(startEl) : -1;
  let ei = endEl ? all.indexOf(endEl) : -1;
  if (si < 0 && ei < 0) return []; // neither end is on a visible page
  if (si < 0) si = ei;
  if (ei < 0) ei = si;
  if (ei < si) return [];
  const rects = [];
  for (let i = si; i <= ei; i++) {
    const el = all[i];
    const r = el.getBoundingClientRect();
    const len = (el.textContent || "").length || 1;
    let left = r.left;
    let right = r.right;
    if (el === startEl && ann.off_start) {
      left = r.left + (r.width * Math.min(ann.off_start, len)) / len;
    }
    if (el === endEl) {
      const end = Math.min((ann.off_end ?? len - 1) + 1, len);
      right = r.left + (r.width * end) / len;
    }
    if (right > left) {
      rects.push({ left, top: r.top, right, bottom: r.bottom, width: right - left, height: r.height });
    }
  }
  return rects;
}

// Paint every resolvable annotation into one section's overlayer. Idempotent:
// `overlayer.add` replaces an existing key, so re-running after a sync just
// refreshes (device import is add-only, so nothing needs removing).
function paintAnnotations(doc, overlayer) {
  const vertical = (book?.writingMode || "").startsWith("vertical");
  for (const ann of annotations) {
    if (ann.kind === "bookmark") {
      // PDF mode draws bookmarks as a page-corner marker (paintPdfPageBookmarks),
      // matching the Kindle's top-right corner ribbon — consistent whether the
      // bookmark is native or imported, never at the anchor char.
      if (readerMode === "pdf") continue;
      const range = rangeFor(doc, ann);
      if (range) {
        overlayer.add(`ann-${ann.id}`, range, drawBookmarkMarker, { color: BOOKMARK_COLOR, vertical });
      }
      continue;
    }
    if (!annotationRects(doc, ann).length) continue; // not in this section
    const color = (ann.color && COLORS[ann.color]) || ann.color || COLORS.yellow;
    // overlayer.add (and redraw on resize) calls range.getClientRects(); hand it
    // a range-like that recomputes our per-element rects each time.
    const rangeLike = { getClientRects: () => annotationRects(doc, ann) };
    overlayer.add(`ann-${ann.id}`, rangeLike, ann.kind === "note" ? drawNote : drawHighlight, { color, vertical });
  }
}

// ---- create / edit / delete native annotations -----------------------------
// Reverse of the paint path: a DOM selection → (eid, offset) → a stored 'sidle'
// annotation. A floating toolbar offers highlight colors + Note on selection;
// clicking a native highlight/note opens an editable popover (textarea + color +
// Save/Delete); a topbar button toggles a page bookmark. Imported ('yjr')
// annotations stay read-only (the device sync owns them).

// Build 4 color swatches into `container`; the `current` COLORS key (or null) is
// marked active; `onPick(name)` fires on click.
function renderColorSwatches(container, current, onPick) {
  if (!container) return;
  container.replaceChildren(
    ...Object.keys(COLORS).map((name) => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "reader-swatch" + (name === current ? " active" : "");
      b.style.background = COLORS[name];
      b.title = name;
      b.setAttribute("aria-label", `${name} highlight`);
      b.addEventListener("click", (e) => {
        e.preventDefault();
        onPick(name);
      });
      return b;
    }),
  );
}

// --- selection → floating toolbar ---

// Position the highlight/note toolbar above the live selection (in the parent
// document, over the iframe). Flips below if there's no room above.
function showSelectionToolbar(doc, range) {
  const bar = $("#reader-selection-toolbar");
  if (!bar) return;
  bar.hidden = false; // unhide first so offset sizes are real
  const fr = doc.defaultView?.frameElement?.getBoundingClientRect() || { left: 0, top: 0 };
  const rect = range.getBoundingClientRect();
  let left = fr.left + rect.left + rect.width / 2 - bar.offsetWidth / 2;
  left = Math.max(8, Math.min(left, window.innerWidth - bar.offsetWidth - 8));
  let top = fr.top + rect.top - bar.offsetHeight - 8;
  if (top < 8) top = fr.top + rect.bottom + 8;
  bar.style.left = `${left}px`;
  bar.style.top = `${Math.max(8, top)}px`;
}

function hideSelectionToolbar() {
  const bar = $("#reader-selection-toolbar");
  if (bar) bar.hidden = true;
  pendingSelection = null;
}

// On mouseup in a section: a resolvable non-empty selection shows the toolbar
// (stashing its anchor + base text); anything else dismisses it.
function onSelection(doc) {
  const sel = doc.getSelection?.();
  if (!sel || sel.isCollapsed || sel.rangeCount === 0) {
    hideSelectionToolbar();
    return;
  }
  const range = sel.getRangeAt(0);
  const anchor = anchorFromRange(doc, range);
  if (!anchor) {
    hideSelectionToolbar();
    return;
  }
  pendingSelection = { doc, anchor, text: baseTextOf(doc, anchor) };
  showSelectionToolbar(doc, range);
}

// Create a native highlight from the pending selection (optionally opening the
// note editor on it). `text` is the ruby-free base-text slice so it re-paints
// exactly via rangeFor; loc/linear ride along from the start eid's Location.
async function createAnnotation(color, openEditor) {
  if (!pendingSelection || bookId == null) return;
  const { doc, anchor, text } = pendingSelection;
  const linear = locByEid?.get(anchor.eid_start) ?? null;
  // Capture a parent-doc point from the selection rect before clearing it, for
  // an optional editor.
  const sel = doc.getSelection?.();
  const rect = sel && sel.rangeCount ? sel.getRangeAt(0).getBoundingClientRect() : null;
  const fr = doc.defaultView?.frameElement?.getBoundingClientRect() || { left: 0, top: 0 };
  const px = rect ? fr.left + rect.left : 16;
  const py = rect ? fr.top + rect.bottom : 16;
  let created;
  try {
    created = await window.api.invoke("annotation_create", {
      bookId,
      kind: "highlight",
      eidStart: anchor.eid_start,
      offStart: anchor.off_start,
      eidEnd: anchor.eid_end,
      offEnd: anchor.off_end,
      locStart: linear,
      linearPos: linear,
      text,
      noteBody: null,
      color,
    });
  } catch (err) {
    toast(`Couldn't save highlight: ${err}`, true);
    return;
  }
  sel?.removeAllRanges();
  hideSelectionToolbar();
  await reloadAnnotations(bookId);
  if (openEditor && created) {
    const ann = annotations.find((a) => a.id === created.id) || created;
    openAnnotationEditor(ann, px, py);
  }
}

// --- click an annotation: edit (native) or read its note (imported) ---

// The overlay SVG is pointer-transparent, so clicks land on the iframe doc. The
// topmost highlight/note under the point (bookmarks aren't edit targets here).
function annotationAt(doc, x, y) {
  for (const ann of annotations) {
    if (ann.kind === "bookmark") continue;
    for (const r of annotationRects(doc, ann)) {
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return ann;
    }
  }
  return null;
}

function onDocClick(e, doc) {
  const ann = annotationAt(doc, e.clientX, e.clientY);
  if (!ann) {
    hideNotePopover();
    return;
  }
  // The click is in the iframe's own viewport; offset by the iframe box to land
  // in the parent document's coordinate space.
  const fr = doc.defaultView?.frameElement?.getBoundingClientRect() || { left: 0, top: 0 };
  const px = fr.left + e.clientX;
  const py = fr.top + e.clientY;
  if (ann.source === "sidle") openAnnotationEditor(ann, px, py);
  else if (ann.note_body) showReadOnlyNote(ann, px, py);
  else hideNotePopover();
}

// Place the popover near a parent-document point, flipping above if it would
// overflow the bottom, clamped to the viewport.
function positionPopover(pop, px, py) {
  pop.hidden = false; // unhide so offset sizes are real
  const left = Math.max(8, Math.min(px, window.innerWidth - pop.offsetWidth - 8));
  let top = py + 14;
  if (top + pop.offsetHeight > window.innerHeight - 8) top = py - pop.offsetHeight - 14;
  pop.style.left = `${left}px`;
  pop.style.top = `${Math.max(8, top)}px`;
}

// Read-only popover for an IMPORTED note: quote + body, no controls.
function showReadOnlyNote(ann, px, py) {
  const pop = $("#reader-note-popover");
  if (!pop) return;
  editingAnn = null;
  pop.classList.remove("editing");
  $("#reader-note-quote").textContent = ann.text || "";
  $("#reader-note-body").textContent = ann.note_body || "";
  $("#reader-note-body").hidden = false;
  $("#reader-note-edit").hidden = true;
  $("#reader-note-edit-controls").hidden = true;
  positionPopover(pop, px, py);
}

function renderEditorColors() {
  renderColorSwatches($("#reader-note-colors"), editorColor, setEditorColor);
}
function setEditorColor(name) {
  editorColor = name;
  renderEditorColors();
}

// Editor for a NATIVE annotation: quote + textarea + color swatches + Save/Delete.
// `px`/`py` are already in the parent document's coordinate space.
function openAnnotationEditor(ann, px, py) {
  const pop = $("#reader-note-popover");
  if (!pop) return;
  editingAnn = ann;
  editorColor = ann.color && COLORS[ann.color] ? ann.color : DEFAULT_HL_COLOR;
  pop.classList.add("editing");
  $("#reader-note-quote").textContent = ann.text || "";
  $("#reader-note-body").hidden = true;
  const ta = $("#reader-note-edit");
  ta.hidden = false;
  ta.value = ann.note_body || "";
  $("#reader-note-edit-controls").hidden = false;
  renderEditorColors();
  positionPopover(pop, px, py);
  ta.focus();
}

// Persist the editor: a non-empty body promotes a highlight to a note (and an
// emptied note demotes back to a highlight) — the backend recomputes the hash.
async function saveEditor() {
  if (!editingAnn || bookId == null) return;
  const body = ($("#reader-note-edit")?.value || "").trim();
  const kind = editingAnn.kind === "bookmark" ? "bookmark" : body ? "note" : "highlight";
  try {
    await window.api.invoke("annotation_update", {
      id: editingAnn.id,
      kind,
      noteBody: body || null,
      color: editorColor,
    });
  } catch (err) {
    toast(`Couldn't save: ${err}`, true);
    return;
  }
  hideNotePopover();
  await reloadAnnotations(bookId);
}

async function deleteEditor() {
  if (!editingAnn || bookId == null) return;
  const id = editingAnn.id;
  hideNotePopover();
  try {
    await window.api.invoke("annotation_delete", { id });
  } catch (err) {
    toast(`Couldn't delete: ${err}`, true);
    return;
  }
  await reloadAnnotations(bookId);
}

function hideNotePopover() {
  const p = $("#reader-note-popover");
  if (p) {
    p.hidden = true;
    p.classList.remove("editing");
  }
  editingAnn = null;
}

// --- bookmark: toggle on the current page (top-of-page eid) ---

// The native bookmark anchored at the current page's top eid, if any.
function currentBookmark() {
  const eid = livePosition?.eid;
  if (eid == null) return null;
  return (
    annotations.find(
      (a) => a.kind === "bookmark" && a.source === "sidle" && a.eid_start === eid,
    ) || null
  );
}

async function toggleBookmark() {
  if (bookId == null) return;
  const eid = livePosition?.eid;
  if (eid == null) return;
  const existing = currentBookmark();
  try {
    if (existing) {
      await window.api.invoke("annotation_delete", { id: existing.id });
    } else {
      const linear = locByEid?.get(eid) ?? null;
      await window.api.invoke("annotation_create", {
        bookId,
        kind: "bookmark",
        eidStart: eid,
        offStart: 0,
        eidEnd: null,
        offEnd: null,
        locStart: linear,
        linearPos: linear,
        text: "",
        noteBody: null,
        color: null,
      });
    }
  } catch (err) {
    toast(`Bookmark failed: ${err}`, true);
    return;
  }
  await reloadAnnotations(bookId);
}

// Reflect whether the current page carries a native bookmark (filled vs empty).
function updateBookmarkButton() {
  const btn = $("#reader-bookmark");
  if (!btn) return;
  const marked = currentBookmark() != null;
  btn.classList.toggle("is-active", marked);
  btn.setAttribute("aria-pressed", marked ? "true" : "false");
}

// ---- annotations panel + jump ----------------------------------------------

function buildEidIndex(d) {
  const map = new Map();
  for (let i = 0; i < (d?.sections?.length || 0); i++) {
    const html = d.sections[i].html || "";
    for (const m of html.matchAll(/data-eid="(\d+)"/g)) {
      const eid = Number(m[1]);
      if (!map.has(eid)) map.set(eid, i);
    }
  }
  return map;
}

async function jumpTo(ann) {
  if (ann.eid_start == null) return;
  hideNotePopover();
  if (readerMode === "pdf") {
    const page = pdf?.eidToPage.get(ann.eid_start);
    if (page == null) {
      toast("Couldn't locate that annotation in the book");
      return;
    }
    pdfGoTo(page);
    return;
  }
  if (!paginator) return;
  if (!eidToSection) eidToSection = buildEidIndex(dto);
  const index = eidToSection.get(ann.eid_start);
  if (index == null) {
    toast("Couldn't locate that annotation in the book");
    return;
  }
  await paginator.goTo({ index, anchor: (d) => rangeFor(d, ann) || 0 });
}

// ---- resume: jump to a saved position (Sidle's own / the Kindle's) ----------

// Navigate to an eid's element — resolve the section via the eid index, anchor
// on the `[data-eid]` element (same path as the annotation jump). Returns false
// (a no-op) when the eid isn't in the book, e.g. after a re-convert.
async function goToEid(eid) {
  if (eid == null) return false;
  if (readerMode === "pdf") {
    const page = pdf?.eidToPage.get(eid);
    if (page == null) return false;
    pdfGoTo(page);
    return true;
  }
  if (!paginator) return false;
  if (!eidToSection) eidToSection = buildEidIndex(dto);
  const index = eidToSection.get(eid);
  if (index == null) return false;
  await paginator.goTo({ index, anchor: (d) => d.querySelector(`[data-eid="${eid}"]`) || 0 });
  return true;
}

async function jumpToPosition(pos) {
  if (!(await goToEid(pos?.eid))) toast("Couldn't locate that position in the book");
}

// The Resume targets in menu order — Sidle's own spot first (the common case),
// then each Kindle's imported position. Each is present only if it has an eid;
// device rows are labeled by a short serial tail when more than one device has
// synced this book (so two Kindles are distinguishable).
function resumeTargets() {
  const out = [];
  if (sidleResume?.eid != null) out.push({ label: "Sidle", pos: sidleResume });
  const devices = deviceResumes.filter((p) => p?.eid != null);
  for (const pos of devices) {
    const tail = pos.device_serial ? pos.device_serial.slice(-4) : "";
    const label = devices.length > 1 && tail ? `Kindle ${tail}` : "Kindle";
    out.push({ label, pos });
  }
  return out;
}

// Show the Resume button only when there's somewhere to resume to.
function renderResumeControl() {
  const btn = $("#reader-resume");
  if (btn) btn.hidden = resumeTargets().length === 0;
  hideResumeMenu();
}

function hideResumeMenu() {
  const m = $("#reader-resume-menu");
  if (m) m.hidden = true;
}

// Toggle the chooser, building a row per target and anchoring it above the
// button (which lives in the status bar). Clicking a row jumps and dismisses.
function toggleResumeMenu() {
  const menu = $("#reader-resume-menu");
  const btn = $("#reader-resume");
  if (!menu || !btn) return;
  if (!menu.hidden) {
    hideResumeMenu();
    return;
  }
  const targets = resumeTargets();
  if (!targets.length) return;
  menu.replaceChildren(
    ...targets.map(({ label, pos }) => {
      const item = document.createElement("button");
      item.type = "button";
      item.className = "reader-resume-item";
      item.setAttribute("role", "menuitem");
      const name = document.createElement("span");
      name.className = "rri-label";
      name.textContent = label;
      const loc = document.createElement("span");
      loc.className = "rri-loc";
      loc.textContent = pos.linear_pos != null ? `Loc ${pos.linear_pos}` : "";
      item.append(name, loc);
      item.addEventListener("click", async () => {
        hideResumeMenu();
        await jumpToPosition(pos);
      });
      return item;
    }),
  );
  menu.hidden = false;
  const r = btn.getBoundingClientRect();
  const mw = menu.offsetWidth;
  const left = Math.max(8, Math.min(r.left + r.width / 2 - mw / 2, window.innerWidth - mw - 8));
  menu.style.left = `${left}px`;
  menu.style.top = `${Math.max(8, r.top - menu.offsetHeight - 6)}px`;
}

function annotationRow(ann) {
  const li = document.createElement("li");
  li.className = `ann-row ann-${ann.kind}`;

  const icon = document.createElement("span");
  icon.className = "ann-icon";
  icon.textContent = KIND_ICON[ann.kind] || "•";

  const body = document.createElement("div");
  body.className = "ann-row-body";
  const quote = document.createElement("div");
  quote.className = "ann-quote";
  quote.textContent = ann.text || (ann.kind === "bookmark" ? "Bookmark" : "");
  body.appendChild(quote);
  if (ann.note_body) {
    const note = document.createElement("div");
    note.className = "ann-note";
    note.textContent = ann.note_body;
    body.appendChild(note);
  }

  li.append(icon, body);
  li.addEventListener("click", () => jumpTo(ann));
  return li;
}

function renderAnnotationsPanel() {
  const list = $("#reader-annotations-list");
  if (!list) return;
  const sorted = [...annotations].sort(
    (a, b) => (a.loc_start ?? a.eid_start ?? 0) - (b.loc_start ?? b.eid_start ?? 0),
  );
  list.replaceChildren(...sorted.map(annotationRow));

  const n = sorted.length;
  $("#reader-annotations-empty").hidden = n > 0;
  const countEl = $("#reader-annotations-count");
  if (countEl) {
    countEl.textContent = String(n);
    countEl.hidden = n === 0;
  }
}

function toggleAnnotationsPanel() {
  const p = $("#reader-annotations-panel");
  if (!p) return;
  if (p.hidden) {
    hideSearchPanel(); // shares the right slot with search
    p.hidden = false;
  } else {
    p.hidden = true;
  }
}

function hideAnnotationsPanel() {
  const p = $("#reader-annotations-panel");
  if (p) p.hidden = true;
}

// Re-fetch + repaint when a device sync lands while this book is open, so new
// highlights show up — and ones deleted on the device (full-mirror sync)
// disappear — without forcing the reader closed. No-op for other books.
async function reloadAnnotations(forBookId) {
  if (bookId == null) return;
  if (forBookId != null && forBookId !== bookId) return;
  const prevIds = annotations.map((a) => a.id);
  let next;
  try {
    next = (await window.api.invoke("annotations_for_book", { bookId })) || [];
  } catch {
    return;
  }
  annotations = next;
  if (readerMode === "pdf") {
    // PDF repaints the whole overlay from `annotations` (incl. page-level
    // bookmark markers), so a removed annotation drops with the fresh overlayer.
    repaintPdfOverlay();
    renderAnnotationsPanel();
    updateBookmarkButton();
    return;
  }
  // Clear overlays for annotations the sync removed (overlayer.add only adds /
  // replaces by key, so a vanished annotation would otherwise linger painted).
  const live = new Set(annotations.map((a) => a.id));
  const gone = prevIds.filter((id) => !live.has(id));
  for (const { overlayer } of overlays) {
    for (const id of gone) overlayer.remove(`ann-${id}`);
  }
  for (const { doc, overlayer } of overlays) paintAnnotations(doc, overlayer);
  renderAnnotationsPanel();
  updateBookmarkButton();
}

// ---- search panel + paint --------------------------------------------------

const SEARCH_COLOR = "#5ad1e3"; // cyan — every match
const SEARCH_COLOR_SELECTED = "#ff5722"; // deep-orange — the row the user last clicked,
// so when a page carries multiple hits of the same word it's clear which one was jumped to

// One painted search match keyed `search-<i>`, so closing search can remove
// only its own rects and leave annotation paint untouched.
function paintOneSearchMatch(doc, overlayer, m, i) {
  const matchAsAnn = { eid_start: m.eid, eid_end: m.eid, off_start: m.off_start, off_end: m.off_end };
  if (!annotationRects(doc, matchAsAnn).length) return; // not in this section
  const vertical = (book?.writingMode || "").startsWith("vertical");
  const color = i === selectedSearchIndex ? SEARCH_COLOR_SELECTED : SEARCH_COLOR;
  const rangeLike = { getClientRects: () => annotationRects(doc, matchAsAnn) };
  overlayer.add(`search-${i}`, rangeLike, drawHighlight, { color, vertical });
}

// Paint every current match into one overlayer. Called from the
// `create-overlayer` handler (when a section first loads with active results)
// and from `runSearch` (when fresh results arrive).
function paintSearchMatches(doc, overlayer) {
  for (let i = 0; i < searchResults.length; i++) {
    paintOneSearchMatch(doc, overlayer, searchResults[i], i);
  }
}

// Remove any painted search rects across all loaded sections (closing the
// panel, or clearing before a new query's paint). Iterates up to the previously
// painted count so a longer prior result set doesn't leave orphans behind.
function clearSearchPaint() {
  for (const { overlayer } of overlays) {
    for (let i = 0; i < lastPaintedCount; i++) overlayer.remove(`search-${i}`);
  }
  lastPaintedCount = 0;
}

function renderSearchPanel() {
  const list = $("#reader-search-list");
  const status = $("#reader-search-status");
  if (!list || !status) return;
  list.replaceChildren(...searchResults.map((m, i) => searchRow(m, i)));
  if (!searchQuery) status.textContent = "";
  else if (searchResults.length === 0) status.textContent = "No matches.";
  else status.textContent = `${searchResults.length} match${searchResults.length === 1 ? "" : "es"}`;
}

function searchRow(m, i) {
  const li = document.createElement("li");
  li.className = "search-row";
  if (i === selectedSearchIndex) li.classList.add("search-row-selected");
  li.tabIndex = 0;
  const preview = document.createElement("div");
  preview.className = "search-preview";
  preview.append(document.createTextNode(m.preview_before || ""));
  const mark = document.createElement("mark");
  mark.textContent = m.preview_match || "";
  preview.append(mark);
  preview.append(document.createTextNode(m.preview_after || ""));
  li.append(preview);
  if (m.linear_pos != null) {
    const loc = document.createElement("div");
    loc.className = "search-row-loc";
    loc.textContent = `Loc ${m.linear_pos}`;
    li.append(loc);
  }
  li.addEventListener("click", () => jumpToSearchMatch(m, i));
  li.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      jumpToSearchMatch(m, i);
    }
  });
  return li;
}

async function jumpToSearchMatch(m, i) {
  if (readerMode === "pdf") {
    selectedSearchIndex = i;
    renderSearchPanel();
    $(".search-row-selected")?.scrollIntoView({ block: "nearest" });
    const page = pdf?.eidToPage.get(m.eid);
    if (page == null) {
      toast("Couldn't locate that match in the book");
      return;
    }
    pdfGoTo(page); // re-render → repaintPdfOverlay paints the match in the selected color
    repaintPdfOverlay(); // also repaint in place when the match is already on-page
    return;
  }
  if (!paginator) return;
  if (!eidToSection) eidToSection = buildEidIndex(dto);
  const index = eidToSection.get(m.eid);
  if (index == null) {
    toast("Couldn't locate that match in the book");
    return;
  }
  // Mark this match as the selected one BEFORE repainting + jumping, so:
  //   - every loaded overlayer redraws this i in the selected color (and any
  //     previously-selected i goes back to the base color);
  //   - the panel row gets the .selected class;
  //   - the new section, when it loads, also paints this i selected
  //     (paintOneSearchMatch reads `selectedSearchIndex` at paint time).
  selectedSearchIndex = i;
  for (const { doc, overlayer } of overlays) paintSearchMatches(doc, overlayer);
  renderSearchPanel();
  $(".search-row-selected")?.scrollIntoView({ block: "nearest" });
  const matchAsAnn = { eid_start: m.eid, eid_end: m.eid, off_start: m.off_start, off_end: m.off_end };
  await paginator.goTo({ index, anchor: (d) => rangeFor(d, matchAsAnn) || 0 });
}

async function runSearch(q) {
  if (bookId == null) return;
  const trimmed = (q || "").trim();
  searchQuery = trimmed;
  // A new query drops any prior selection — the indices don't carry over.
  selectedSearchIndex = -1;
  // Empty query: clear results + paint immediately, don't hit the backend.
  if (!trimmed) {
    searchResults = [];
    clearSearchPaint();
    renderSearchPanel();
    return;
  }
  // Race token — if a faster later request lands first, ignore this slower one.
  const seq = ++searchSeq;
  const requestedBookId = bookId;
  const status = $("#reader-search-status");
  if (status) status.textContent = "Searching…";
  let matches;
  try {
    matches = (await window.api.invoke("book_search", { bookId, query: trimmed })) || [];
  } catch (err) {
    if (seq !== searchSeq) return;
    if (status) status.textContent = `Search failed: ${err}`;
    return;
  }
  // Stale response (newer query in flight, or the book was closed/swapped).
  if (seq !== searchSeq || bookId !== requestedBookId) return;
  clearSearchPaint();
  searchResults = matches;
  renderSearchPanel();
  for (const { doc, overlayer } of overlays) paintSearchMatches(doc, overlayer);
  lastPaintedCount = searchResults.length;
}

function scheduleSearch(value) {
  clearTimeout(searchDebounceTimer);
  // 150ms is short enough to feel live, long enough to coalesce typing bursts.
  searchDebounceTimer = setTimeout(() => runSearch(value), 150);
}

function toggleSearchPanel() {
  const p = $("#reader-search-panel");
  if (!p) return;
  if (p.hidden) {
    hideAnnotationsPanel(); // shares the right slot
    p.hidden = false;
    $("#reader-search-input")?.focus();
  } else {
    hideSearchPanel();
  }
}

function hideSearchPanel() {
  const p = $("#reader-search-panel");
  if (p) p.hidden = true;
  // Closing the panel drops the transient paint AND the results. Per the plan,
  // there's no last-search memory across opens.
  clearTimeout(searchDebounceTimer);
  searchSeq++; // invalidate any in-flight request
  searchResults = [];
  searchQuery = "";
  selectedSearchIndex = -1;
  clearSearchPaint();
  const input = $("#reader-search-input");
  if (input) input.value = "";
  const status = $("#reader-search-status");
  if (status) status.textContent = "";
  const list = $("#reader-search-list");
  if (list) list.replaceChildren();
}

// ---- TOC panel + jump -------------------------------------------------------

// Resolve a TOC href ("c5.xhtml#frag") to a section index + optional fragment.
// boko emits TOC hrefs in the same OEBPS-relative form as the spine section
// hrefs (both from one `build_output`), so `book.hrefs.indexOf` matches.
function tocTarget(href) {
  const [path, frag] = String(href || "").split("#");
  const index = book?.hrefs?.indexOf(path) ?? -1;
  return { index, frag: frag || null };
}

async function goToToc(href) {
  if (!paginator) return;
  const { index, frag } = tocTarget(href);
  if (index < 0) {
    toast("Couldn't locate that chapter in the book");
    return;
  }
  hideNotePopover();
  // The paginator accepts a fraction, Range, or Element as the anchor: hand it
  // the fragment's element when present, else 0 (the section start).
  await paginator.goTo({ index, anchor: (doc) => (frag && doc.getElementById(frag)) || 0 });
}

// Build rows depth-first, indenting by depth. Each row remembers its section
// index so `markTocActive` can highlight the current chapter on relocate.
function tocRowsFor(point, depth, out) {
  const li = document.createElement("li");
  li.className = "toc-row";
  li.style.paddingLeft = `${14 + depth * 14}px`; // UI chrome is always LTR
  li.textContent = point.label || "—";
  li.addEventListener("click", () => goToToc(point.href));
  tocEntries.push({ li, sectionIndex: tocTarget(point.href).index });
  out.push(li);
  for (const child of point.children || []) tocRowsFor(child, depth + 1, out);
}

function renderTocPanel() {
  tocEntries = [];
  const toc = book?.toc || [];
  const btn = $("#reader-toc");
  if (btn) btn.hidden = toc.length === 0; // no TOC → no button (panel unreachable)
  const list = $("#reader-toc-list");
  if (!list) return;
  const rows = [];
  for (const p of toc) tocRowsFor(p, 0, rows);
  list.replaceChildren(...rows);
  $("#reader-toc-empty").hidden = rows.length > 0;
}

// Highlight the TOC entry for the current section: the first entry landing in
// it, else the last entry before it. (Most books are one entry per section.)
function markTocActive(currentIndex) {
  if (typeof currentIndex !== "number" || !tocEntries.length) return;
  let active = tocEntries.findIndex((e) => e.sectionIndex === currentIndex);
  if (active < 0) {
    for (let i = 0; i < tocEntries.length; i++) {
      if (tocEntries[i].sectionIndex >= 0 && tocEntries[i].sectionIndex < currentIndex) active = i;
    }
  }
  tocEntries.forEach(({ li }, i) => li.classList.toggle("active", i === active));
  if (active >= 0 && !$("#reader-toc-panel")?.hidden) {
    tocEntries[active].li.scrollIntoView({ block: "nearest" });
  }
}

function toggleTocPanel() {
  const p = $("#reader-toc-panel");
  if (p) p.hidden = !p.hidden;
}

function hideTocPanel() {
  const p = $("#reader-toc-panel");
  if (p) p.hidden = true;
}

// ---- progress: Loc + whole-book % ------------------------------------------

// Positioned [data-eid] elements in a section doc, with their linear position,
// in document order. Cached per doc (the set is stable once the section loads).
function positionedFor(doc) {
  let arr = positionedByDoc.get(doc);
  if (!arr) {
    arr = [...doc.querySelectorAll("[data-eid]")]
      .map((el) => ({ el, loc: locByEid?.get(Number(el.getAttribute("data-eid"))) }))
      .filter((x) => x.loc != null);
    positionedByDoc.set(doc, arr);
  }
  return arr;
}

// Anchor at the top of the current page: the last positioned [data-eid] element
// at or before the visible range's start, with its Location and eid. The loc
// feeds the progress readout; the eid is what the Sidle-native position saves
// and restores (the same `data-eid` anchoring a highlight uses).
function pageAnchor(doc, range) {
  const arr = positionedFor(doc);
  if (!arr.length) return { loc: null, eid: null };
  const start = range?.startContainer;
  let best = arr[0];
  if (start) {
    for (const item of arr) {
      const following = (item.el.compareDocumentPosition(start) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
      if (item.el === start || item.el.contains(start) || following) best = item;
      else break;
    }
  }
  const eid = Number(best.el.getAttribute("data-eid"));
  return { loc: best.loc, eid: Number.isInteger(eid) ? eid : null };
}

// Char position (base-text) of a (section index, intra-section fraction) — the
// content measure behind both the time estimates and the adaptive pace sampler.
function charPosOf(index, frac) {
  return (charsBefore[index] || 0) + (sectionChars[index] || 0) * frac;
}

// Remaining reading time from a char count and the live `readSpeed`. Under a
// minute → words; ≥ an hour → "H hr M min(s)".
function timeLeft(chars, where) {
  const m = Math.round(Math.max(0, chars) / readSpeed);
  if (m < 1) return `Less than a minute left in ${where}`;
  if (m < 60) return `${m} min${m === 1 ? "" : "s"} left in ${where}`;
  const h = Math.floor(m / 60);
  const mm = m % 60;
  const mPart = mm > 0 ? ` ${mm} min${mm === 1 ? "" : "s"}` : "";
  return `${h} hr${mPart} left in ${where}`;
}

const clampSpeed = (v) => Math.min(5000, Math.max(30, v));
const READY_SAMPLES = 5; // valid page-turns before a time is shown (else "Estimating…")
const paceKey = () => `sidle.reader.pace.${bookId}`;
const paceReady = () => sampleCount >= READY_SAMPLES && readSpeed > 0;

// Per-book reading pace, persisted as { s: chars/min, n: sample count }.
function loadPace() {
  try {
    const raw = localStorage.getItem(paceKey());
    if (raw) {
      const { s, n } = JSON.parse(raw);
      if (Number.isFinite(s) && Number.isFinite(n)) return { speed: clampSpeed(s), count: n };
    }
  } catch {
    /* ignore */
  }
  return { speed: 0, count: 0 };
}

function savePace() {
  try {
    localStorage.setItem(paceKey(), JSON.stringify({ s: Math.round(readSpeed), n: sampleCount }));
  } catch {
    /* ignore */
  }
}

// Fold one page-turn into the per-book `readSpeed` (EMA, seeded by the FIRST
// sample so it's purely the user's measured pace — no language guess). Rejects
// outliers like the device's `timer.average.calculator.outliers`: only forward
// sequential reading counts — skip backward/jumps (non-positive or implausibly
// large chars), quick flips (<1.5s), and idle gaps (>5min).
function sampleReadingSpeed(charPos) {
  const now = Date.now();
  if (lastTurnTime != null && lastCharPos != null) {
    const dtMs = now - lastTurnTime;
    const dChars = charPos - lastCharPos;
    if (dChars > 0 && dtMs >= 1500 && dtMs <= 300000) {
      const sample = dChars / (dtMs / 60000); // chars per minute
      if (sample >= 30 && sample <= 5000) {
        readSpeed = sampleCount === 0 ? sample : clampSpeed(readSpeed * 0.85 + sample * 0.15);
        sampleCount += 1;
        savePace();
      }
    }
  }
  lastTurnTime = now;
  lastCharPos = charPos;
}

// On each relocate: feed the adaptive pace sampler, remember the resolved
// Location + section position, then render per the current mode.
function updateProgress(detail) {
  const doc = detail?.range?.startContainer?.ownerDocument;
  const anchor = doc ? pageAnchor(doc, detail.range) : { loc: null, eid: null };
  const index = detail?.index ?? 0;
  const frac = Number.isFinite(detail?.fraction) ? Math.min(1, Math.max(0, detail.fraction)) : 0;
  sampleReadingSpeed(charPosOf(index, frac));
  lastPos = { loc: anchor.loc, index, frac };
  // Track the live top-of-page anchor in memory only; it's persisted (source
  // 'sidle') on close, so the Sidle Resume target stays frozen at where this
  // session opened until you leave the book.
  if (anchor.eid != null) livePosition = { eid: anchor.eid, offset: 0, linear_pos: anchor.loc };
  renderProgress();
}

// Four click-cycled modes: 0 Loc · 1 min-left-in-chapter · 2 min-left-in-book ·
// 3 hidden (both sides blank, bar dimmed but still tappable to cycle back). The
// right side is the whole-book % in modes 0–2.
function renderProgress() {
  const locEl = $("#reader-loc");
  const pctEl = $("#reader-percent");
  if (!locEl || !pctEl) return;
  $("#reader-statusbar")?.classList.toggle("is-hidden", progressMode === 3);
  if (progressMode === 3 || !lastPos || !maxLoc) {
    locEl.textContent = "";
    pctEl.textContent = "";
    return;
  }
  const { loc, index, frac } = lastPos;
  pctEl.textContent = `${Math.min(100, Math.max(0, Math.round(((loc ?? 0) / maxLoc) * 100)))}%`;
  if (progressMode === 0) {
    locEl.textContent = loc != null ? `Loc ${loc}` : "";
  } else if (!paceReady()) {
    locEl.textContent = "Estimating\u2026";
  } else if (progressMode === 1) {
    locEl.textContent = timeLeft((sectionChars[index] || 0) * (1 - frac), "chapter");
  } else {
    locEl.textContent = timeLeft(totalChars - charPosOf(index, frac), "book");
  }
}

// Cycle 0→1→2→3→0 on a tap; remembered across books/sessions.
function cycleProgressMode() {
  if (readerMode === "reflowable") {
    progressMode = (progressMode + 1) % 4;
  } else {
    // Fixed-layout (PDF, notebook) has no reading-pace modes (no linear text), so
    // a tap toggles the page readout shown ↔ hidden — modes 0 ↔ 3.
    progressMode = progressMode === 3 ? 0 : 3;
  }
  try {
    localStorage.setItem("sidle.reader.progressMode", String(progressMode));
  } catch {
    /* ignore storage errors */
  }
  // Re-render via the active mode's own writer — renderProgress is reflowable-only
  // (it blanks a fixed-layout bar, which has no lastPos).
  if (readerMode === "pdf") pdfUpdateProgress();
  else if (readerMode === "notebook") nbkUpdateProgress();
  else renderProgress();
}

// Base-text char count of a section (ruby `<rt>` excluded, whitespace collapsed)
// — the content measure behind the time estimates.
function baseTextLen(html) {
  const doc = new DOMParser().parseFromString(html, "text/html");
  doc.querySelectorAll("rt").forEach((el) => el.remove());
  return (doc.body?.textContent || "").replace(/\s+/g, " ").trim().length;
}

// ---- display style (per-book: font, size, colors, spacing, margins) --------

// Bi-script font stacks: each renders Latin and Japanese in a fitting face, so
// one global pick works for vertical-JP and horizontal-EN books alike. "" means
// no override — the publisher's own fonts stay.
const FONT_STACKS = {
  "": "",
  serif: 'Georgia, "Hiragino Mincho ProN", serif',
  sans: '"Helvetica Neue", Helvetica, "Hiragino Sans", sans-serif',
  mincho: '"Hiragino Mincho ProN", "YuMincho", serif',
  gothic: '"Hiragino Sans", "Hiragino Kaku Gothic ProN", sans-serif',
  maru: '"Hiragino Maru Gothic ProN", sans-serif',
};
const LINE_HEIGHTS = { auto: 0, tight: 1.35, normal: 1.6, relaxed: 1.9, loose: 2.2 };
const WEIGHTS = { "": 0, light: 300, normal: 400, medium: 500, semibold: 600 };
// Margin presets, adaptive to writing mode (applied in `applyLayout`): the
// control adds whitespace at the line ends — TOP/BOTTOM for vertical text,
// LEFT/RIGHT for horizontal — the same logical axis, rotated. Vertical varies
// the block (top/bottom) margin in px; horizontal varies a single column's width
// in px (the leftover is the left/right margin). "normal" reproduces the old
// vertical layout.
const VMARGIN = { narrow: 24, normal: 48, wide: 112 }; // vertical top/bottom margin px
const HMEASURE = { narrow: 940, normal: 760, wide: 560 }; // horizontal column width px

// Defaults reproduce the pre-customization look exactly: publisher fonts, the
// iframe's 16px root, white page, near-black ink, the paginator's own margins.
const DEFAULT_STYLE = {
  font: "",
  size: 16,
  weight: "",
  spacing: "auto",
  align: "",
  fg: "#111111",
  bg: "#ffffff",
  margin: "normal",
  columns: "auto",
};
let styleSettings = null;
let imageSections = new Set(); // section indices that are a single full-page image
let layoutMode = null; // current paginator layout: "text" | "image"
const styleKey = () => `sidle.reader.style.${bookId}`;

function loadStyle() {
  try {
    const raw = localStorage.getItem(styleKey());
    if (raw) return { ...DEFAULT_STYLE, ...JSON.parse(raw) };
  } catch {
    /* ignore */
  }
  return { ...DEFAULT_STYLE };
}
function saveStyle() {
  try {
    localStorage.setItem(styleKey(), JSON.stringify(styleSettings));
  } catch {
    /* ignore */
  }
}

// #rrggbb / #rgb → [r,g,b].
function hexRgb(hex) {
  const c = String(hex || "").replace("#", "");
  const h = c.length === 3 ? c.replace(/./g, (x) => x + x) : c;
  return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16) || 0);
}
const luminance = (hex) => {
  const [r, g, b] = hexRgb(hex);
  return (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
};
const isDark = (hex) => luminance(hex) < 0.5;
// Blend two hex colors: `a` toward `b` by t (0..1) → "#rrggbb".
function mix(a, b, t) {
  const ca = hexRgb(a);
  const cb = hexRgb(b);
  const ch = ca.map((v, i) => Math.round(v + (cb[i] - v) * t).toString(16).padStart(2, "0"));
  return `#${ch.join("")}`;
}

// A section that is just a full-page image (the cover, a full-bleed
// illustration) — no real text. Such sections get a zero-margin, single-column
// layout so the image fills the page instead of shrinking into one text column.
function isImageOnlySection(html) {
  const doc = new DOMParser().parseFromString(html, "text/html");
  doc.querySelectorAll("rt").forEach((el) => el.remove());
  const text = (doc.body?.textContent || "").replace(/\s+/g, "");
  return text.length === 0 && (doc.body?.querySelector("img, image, svg") != null);
}

// The CSS injected into each section iframe via the paginator's `setStyles`. It
// lands in the last <style> of the head, so `!important` here beats both the
// book's synthesized stylesheet and the static READER_CSS. font-size rides the
// root because boko emits text sizes in rem/% (root-relative) — one anchor
// scales everything, like Kindle's slider. At DEFAULT settings this emits the
// exact equivalent of the old READER_CSS (white bg, non-important #111 body
// color, no font-size/family override) so a default book renders identically to
// before customization existed; only changed fields override.
function buildSectionCss(s) {
  const customFg = s.fg.toLowerCase() !== DEFAULT_STYLE.fg;
  const out = [
    `:root { color-scheme: ${isDark(s.bg) ? "dark" : "light"}; }`,
    `html, body { background: ${s.bg} !important; }`,
    // Default = non-important so the book's own text colors still win (as before);
    // a custom color forces over them.
    customFg ? `html, body { color: ${s.fg} !important; }` : `body { color: ${s.fg}; }`,
  ];
  // Anchor the root size only when changed, so a default book keeps its shipped
  // root sizing untouched.
  if (s.size !== DEFAULT_STYLE.size) out.push(`html { font-size: ${s.size}px !important; }`);
  const stack = FONT_STACKS[s.font];
  if (stack) out.push(`* { font-family: ${stack} !important; }`);
  const w = WEIGHTS[s.weight];
  if (w) out.push(`body, p, li, blockquote, dd { font-weight: ${w} !important; }`);
  const lh = LINE_HEIGHTS[s.spacing];
  if (lh) out.push(`p, li, blockquote, dd { line-height: ${lh} !important; }`);
  if (s.align) out.push(`p, li, blockquote, dd { text-align: ${s.align} !important; }`);
  return out.join("\n");
}

// Tint the reader chrome (gutter, bars, panels) to the page colors so a sepia or
// dark page doesn't sit framed by white UI. At DEFAULT colors we set nothing —
// the CSS `--reader-*` defaults (the original warm-light palette) stay, so the
// chrome is identical to before. Custom colors derive coherent fg→bg blends.
function applyChrome(s) {
  const v = view();
  if (!v) return;
  const props = [
    "color-scheme",
    "--reader-bg",
    "--reader-fg",
    "--reader-gutter",
    "--reader-border",
    "--reader-muted",
    "--overlayer-highlight-blend-mode",
  ];
  const isDefault =
    s.bg.toLowerCase() === DEFAULT_STYLE.bg && s.fg.toLowerCase() === DEFAULT_STYLE.fg;
  if (isDefault) {
    for (const p of props) v.style.removeProperty(p);
    return;
  }
  const dark = isDark(s.bg);
  v.style.colorScheme = dark ? "dark" : "light"; // adapt the panel's <select>/color inputs
  v.style.setProperty("--reader-bg", s.bg);
  v.style.setProperty("--reader-fg", s.fg);
  v.style.setProperty("--reader-gutter", mix(s.bg, s.fg, 0.06));
  v.style.setProperty("--reader-border", mix(s.bg, s.fg, 0.16));
  v.style.setProperty("--reader-muted", mix(s.bg, s.fg, 0.5));
  v.style.setProperty("--overlayer-highlight-blend-mode", dark ? "screen" : "multiply");
}

const HUGE_MEASURE = 100000; // forces a single full-width column for image pages

// Set the paginator's layout attributes for `index`'s mode. Text pages use the
// user's margin preset (= the original 720/48/7%/2-col defaults at the default
// setting). Image-only pages (cover) go full-bleed: zero margin/gap, one column,
// unbounded measure — so the image fits the whole page, no frame. Skips redundant
// work unless `force` (used when a settings change must re-apply the same mode).
function applyLayout(index, force) {
  if (!paginator || !styleSettings) return;
  const mode = imageSections.has(index) ? "image" : "text";
  if (!force && mode === layoutMode) return;
  // These attributes feed CSS vars used inside minmax()/calc(), so lengths MUST
  // carry a unit — a bare "48" makes `minmax(48, 1fr)` invalid and the margin
  // silently collapses. (0 is the one length that's valid unitless.)
  layoutMode = mode;
  if (mode === "image") {
    paginator.setAttribute("margin", "0px");
    paginator.setAttribute("gap", "0%");
    paginator.setAttribute("max-inline-size", `${HUGE_MEASURE}px`);
    paginator.setAttribute("max-column-count", "1");
  } else {
    // Text. The margin preset adds whitespace at the line ends, on the axis the
    // writing mode runs across.
    const vertical = (book?.writingMode || "").startsWith("vertical");
    paginator.setAttribute("gap", "7%");
    if (vertical) {
      // Block (top/bottom) margin from the preset. For the measure (= column
      // height), branch on the page's orientation — matching the paginator's own
      // container-query rule (`#top.vertical` in `@container (portrait)`):
      //   - LANDSCAPE: one tall column. Leave the measure UNCAPPED so content
      //     fills the page height and the `margin` attribute is the *actual*
      //     top/bottom margin, not just a floor.
      //   - PORTRAIT with columns=auto: two STACKED columns. Cap the measure at
      //     (pageH - 2*margin) / 2 so max-height = cap * 2 ≈ pageH - 2*margin
      //     (margin honored exactly) AND ceil(avail/cap) = 2 so the divisor
      //     formula picks two columns. (Uncapped would yield ceil≈1 → one col.)
      //   - PORTRAIT with columns=1: same as Single — max-column-count=1 forces
      //     one column regardless of orientation, so leave the measure uncapped.
      const vm = VMARGIN[styleSettings.margin] ?? VMARGIN.normal;
      const rect = paginator.getBoundingClientRect();
      const portrait = rect.height > rect.width;
      const twoCol = styleSettings.columns !== "1" && portrait;
      const pageH = rect.height || window.innerHeight;
      paginator.setAttribute("margin", `${vm}px`);
      paginator.setAttribute("max-inline-size", twoCol ? `${Math.floor((pageH - 2 * vm) / 2)}px` : `${HUGE_MEASURE}px`);
      paginator.setAttribute("max-column-count", styleSettings.columns === "1" ? "1" : "2");
    } else {
      // A column at most maxInlineSize wide; leftover is L/R margin. With
      // columns=auto, the paginator splits to 2 once the window is wide
      // enough (≈ window > maxInlineSize); columns=1 forces single regardless.
      paginator.setAttribute("margin", "48px");
      paginator.setAttribute("max-inline-size", `${HMEASURE[styleSettings.margin] ?? HMEASURE.normal}px`);
      paginator.setAttribute("max-column-count", styleSettings.columns === "1" ? "1" : "2");
    }
  }
  // The block-`margin` attribute only re-paginates via a ResizeObserver, which
  // can miss; force it so a vertical top/bottom-margin change always takes hold.
  // (No-op before the first section loads — render() bails without a view.)
  paginator.render?.();
}

// Push the current settings into the live view: section CSS + chrome + layout
// for the section in view (forced, since margins may have just changed).
function applyStyle() {
  if (!styleSettings) return;
  if (paginator) paginator.setStyles(buildSectionCss(styleSettings));
  applyChrome(styleSettings);
  applyLayout(lastPos?.index ?? 0, true);
}

// Reflect settings into the panel controls (on open + reset).
function syncStylePanel() {
  if (!styleSettings) return;
  const s = styleSettings;
  const set = (id, val) => {
    const el = $(id);
    if (el) el.value = val;
  };
  set("#rs-font", s.font);
  set("#rs-size", s.size);
  const sv = $("#rs-size-val");
  if (sv) sv.textContent = `${s.size}px`;
  set("#rs-weight", s.weight);
  set("#rs-spacing", s.spacing);
  set("#rs-margin", s.margin);
  set("#rs-columns", s.columns);
  set("#rs-align", s.align);
  set("#rs-fg", s.fg);
  set("#rs-bg", s.bg);
}

// One control changed → fold into settings, persist, re-apply live.
function onStyleInput() {
  if (!styleSettings) return;
  const val = (id, fallback) => $(id)?.value ?? fallback;
  styleSettings = {
    font: val("#rs-font", ""),
    size: Number(val("#rs-size", 16)) || 16,
    weight: val("#rs-weight", ""),
    spacing: val("#rs-spacing", "auto"),
    margin: val("#rs-margin", "normal"),
    columns: val("#rs-columns", "auto"),
    align: val("#rs-align", ""),
    fg: val("#rs-fg", "#111111"),
    bg: val("#rs-bg", "#ffffff"),
  };
  const sv = $("#rs-size-val");
  if (sv) sv.textContent = `${styleSettings.size}px`;
  saveStyle();
  applyStyle();
}

function resetStyle() {
  styleSettings = { ...DEFAULT_STYLE };
  saveStyle();
  syncStylePanel();
  applyStyle();
}

function toggleStylePanel() {
  // Fixed-layout modes (PDF, notebook) share the page-layout panel; reflowable
  // has its own font/size/spacing panel.
  if (readerMode !== "reflowable") return togglePdfStylePanel();
  const p = $("#reader-style-panel");
  if (p) p.hidden = !p.hidden;
}
function hideStylePanel() {
  const p = $("#reader-style-panel");
  if (p) p.hidden = true;
}

// ---- navigation -----------------------------------------------------------

const forward = () => {
  if (readerMode === "pdf") return pdfGoTo(pdf.page + pdfStep());
  if (readerMode === "notebook") return nbkShowPage(nbk.page + nbkStep());
  hideNotePopover();
  hideSelectionToolbar();
  paginator?.next();
};
const back = () => {
  if (readerMode === "pdf") return pdfGoTo(pdf.page - pdfStep());
  if (readerMode === "notebook") return nbkShowPage(nbk.page - nbkStep());
  hideNotePopover();
  hideSelectionToolbar();
  paginator?.prev();
};

// Reading direction of the active book — PDF carries it on `pdf`, reflowable on
// `book`; anything not "rtl" is ltr. The physical (left/right) inputs use this
// to map a side to forward/back; `forward`/`back` themselves stay logical.
function readerPpd() {
  return (readerMode === "pdf" ? pdf?.ppd : book?.ppd) === "rtl" ? "rtl" : "ltr";
}

// Jump to section `idx` clamped to the spine. `anchor` is a fraction: 0 = start
// of section (default), 1 = last page of section (used for "jump to book end").
function jumpToSection(idx, anchor = 0) {
  if (!paginator || !dto?.sections?.length) return;
  const max = dto.sections.length - 1;
  const i = Math.max(0, Math.min(max, idx));
  hideNotePopover();
  hideSelectionToolbar();
  paginator.goTo({ index: i, anchor });
}

// Nudge the font-size slider by `delta` px (clamped to the slider's 12-26
// range) and re-apply styles. Lets `[` / `]` shortcuts work without opening
// the Aa panel.
function bumpFontSize(delta) {
  if (!styleSettings) return;
  const next = Math.max(12, Math.min(26, (styleSettings.size || 16) + delta));
  if (next === styleSettings.size) return;
  styleSettings = { ...styleSettings, size: next };
  const sizeInput = $("#rs-size");
  if (sizeInput) sizeInput.value = String(next);
  const sv = $("#rs-size-val");
  if (sv) sv.textContent = `${next}px`;
  saveStyle();
  applyStyle();
}

// `g g` chord state: timestamp of the most recent armed `g`. A second `g`
// within G_CHORD_MS fires "jump to start"; anything else resets.
const G_CHORD_MS = 1500;
let gArmed = 0;

function onKey(e) {
  // PDF (fixed-layout) books use a simpler key map — no search/highlight/style.
  if (readerMode === "pdf") return pdfOnKey(e);
  // A handwritten notebook is a fixed-layout SVG page with its own minimal map.
  if (readerMode === "notebook") return notebookOnKey(e);
  // ⌘F / Ctrl+F → open search (replacing the browser's find-in-page, which
  // would search the host doc, not the section iframe's content). Handled
  // before the modifier filter below.
  if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && (e.key === "f" || e.key === "F")) {
    toggleSearchPanel();
    e.preventDefault();
    return;
  }
  if (e.key === "Escape") {
    // Peel back overlays first, then close the reader.
    if (!$("#reader-selection-toolbar")?.hidden) hideSelectionToolbar();
    else if (!$("#reader-resume-menu")?.hidden) hideResumeMenu();
    else if (!$("#reader-note-popover")?.hidden) hideNotePopover();
    else if (!$("#reader-style-panel")?.hidden) hideStylePanel();
    else if (!$("#reader-search-panel")?.hidden) hideSearchPanel();
    else if (!$("#reader-annotations-panel")?.hidden) hideAnnotationsPanel();
    else if (!$("#reader-toc-panel")?.hidden) hideTocPanel();
    else close();
    e.preventDefault();
    return;
  }
  // A focused settings control (select/slider/color) owns its own arrow keys —
  // don't steal them to turn pages. Same for the search input.
  if (e.target?.closest?.("#reader-style-panel")) return;
  if (e.target?.closest?.("#reader-search-panel")) return;
  // Typing in the note editor owns its own keys (arrows/space/etc.) — don't
  // hijack them to turn pages. (Escape is already handled above, so it closes.)
  if (e.target?.closest?.("#reader-note-popover")) return;
  // `Shift+G` → jump to last page of the book. Must run BEFORE the modifier
  // filter (which would skip it as a shifted key). All other unmodified
  // shortcuts go through the switch below.
  if (e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey && e.key === "G") {
    jumpToSection((dto?.sections?.length ?? 1) - 1, 1);
    gArmed = 0;
    e.preventDefault();
    return;
  }
  // Don't hijack modified combos — shift+arrow extends a text selection in the
  // section iframe, ⌘/ctrl/alt are shortcuts. Let those through.
  if (e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return;
  // `g g` chord (vim): first `g` arms; second within G_CHORD_MS jumps to start.
  if (e.key === "g") {
    const now = Date.now();
    if (now - gArmed < G_CHORD_MS) { jumpToSection(0, 0); gArmed = 0; }
    else gArmed = now;
    e.preventDefault();
    return;
  }
  // Any non-`g` key resets the chord — pressing `g` then `t` shouldn't fire.
  gArmed = 0;
  const rtl = book?.ppd === "rtl"; // vertical-rl / RTL: next page is to the left
  let handled = true;
  switch (e.key) {
    case "ArrowLeft":
      rtl ? forward() : back();
      break;
    case "ArrowRight":
      rtl ? back() : forward();
      break;
    case "ArrowDown":
    case "PageDown":
    case " ":
      forward();
      break;
    case "ArrowUp":
    case "PageUp":
      back();
      break;
    case "t":
      toggleTocPanel();
      break;
    case "a":
      toggleAnnotationsPanel();
      break;
    case "b":
      toggleBookmark();
      break;
    case "s":
      toggleStylePanel();
      break;
    case "/":
      toggleSearchPanel();
      break;
    case "n":
      jumpToSection((lastPos?.index ?? 0) + 1);
      break;
    case "p":
      jumpToSection((lastPos?.index ?? 0) - 1);
      break;
    case "]":
      bumpFontSize(1);
      break;
    case "[":
      bumpFontSize(-1);
      break;
    default:
      handled = false;
  }
  if (handled) e.preventDefault();
}

// ---- open / close ---------------------------------------------------------

// ---- PDF (fixed-layout) mode ----------------------------------------------
//
// A PDF-backed book renders server-side via PDFKit (`reader_pdf_page`) as a page
// image, with the KFX text layer laid over it as transparent, selectable
// `data-eid` spans — so select / highlight / bookmark / search work exactly as
// the reflowable reader (the page image is the backdrop, the spans host the same
// eid-anchored overlay machinery). An image-only / scanned page has no spans:
// it shows the image with page-level bookmarking only. We reuse the topbar / TOC
// / status chrome; reading position rides the same `reading_position` model,
// anchored to a page's representative eid so it maps back to a page.

// Search has nothing to find on an image-only book, so it's hidden there;
// bookmark + annotations apply to both. (`#reader-style` carries the spread +
// night-mode controls.) Computed per book in `openPdf` from whether any page
// carries a text layer.
const PDF_NO_TEXT_HIDDEN = ["#reader-search"];

let pdfTocRows = []; // [{ li, page }] in TOC order, for active-marking

// PDF display settings — a global reading preference (not per-book), persisted.
// `spread`: auto | single | double; `invert`: night mode (invert the page image);
// `cover`: in double mode, show page 0 as a standalone cover (else pair from 0).
const PDF_STYLE_KEY = "sidle.reader.pdf-style";
const PDF_STYLE_DEFAULT = { spread: "auto", invert: false, ink: true, zoom: 1, cover: true };
const PDF_ZOOM_MIN = 1; // 1 = fit; below it the page already fits, so no point
const PDF_ZOOM_MAX = 3;
const pdfStyle = loadPdfStyle();
function loadPdfStyle() {
  try {
    return { ...PDF_STYLE_DEFAULT, ...JSON.parse(localStorage.getItem(PDF_STYLE_KEY) || "{}") };
  } catch {
    return { ...PDF_STYLE_DEFAULT };
  }
}
function savePdfStyle() {
  try {
    localStorage.setItem(PDF_STYLE_KEY, JSON.stringify(pdfStyle));
  } catch {
    /* ignore */
  }
}

function clampPage(i, count) {
  const n = count || 1;
  return Math.max(0, Math.min(n - 1, Math.floor(i) || 0));
}

// Effective single/double for the current stage size + setting. `auto` picks
// double only when two full-height pages actually fit side by side — the same
// "let the window decide" behaviour the reflowable Columns:auto uses.
function pdfSpreadMode() {
  if (pdfStyle.spread === "single" || pdfStyle.spread === "double") return pdfStyle.spread;
  const stage = $("#reader-stage");
  const sw = stage?.clientWidth || 0;
  const sh = stage?.clientHeight || 1;
  const p = pdf?.pages[pdf.page] || { width: 612, height: 792 };
  const aspect = p.width / Math.max(1, p.height);
  return sw >= 2 * sh * aspect * 0.98 ? "double" : "single";
}
function pdfStep() {
  return pdfSpreadMode() === "double" ? 2 : 1;
}
// First page of the spread that contains `p`. In double mode pages pair up; the
// "Cover page" option (default) keeps the cover (page 0) standalone and pairs the
// rest odd-aligned — cover → 1·2 → 3·4 …, like a physical book's facing pages —
// while turning it off pairs from page 0 — 0·1 → 2·3 …. Single mode: `p`.
function pdfSpreadStart(p) {
  if (p <= 0) return 0;
  if (pdfSpreadMode() !== "double") return p;
  // Cover-alone shifts the pairing by one: odd-aligned starts vs even-aligned.
  const wantParity = pdfStyle.cover === false ? 0 : 1;
  return p % 2 === wantParity ? p : p - 1;
}
// Whether the spread starting at boundary page `s` shows a right-hand page: never
// in single mode, nor on a standalone cover (page 0 with "Cover page" on).
function pdfSpreadHasRight(s) {
  if (pdfSpreadMode() !== "double") return false;
  if (pdfStyle.cover !== false && s === 0) return false; // standalone cover
  return s + 1 < pdf.pageCount;
}
// Whether the current spread shows a right-hand page. Assumes `pdf.page` is on a
// spread boundary (pdfGoTo / pdfRenderCurrent snap it there).
function pdfHasRight() {
  return pdfSpreadHasRight(pdf.page);
}

async function openPdf(id, openDto, anns, positions) {
  readerMode = "pdf";
  pdfStyle.zoom = 1; // zoom is per-open: a book opens at fit, not the last zoom
  bookId = id;
  dto = openDto;
  annotations = anns || [];
  sidleResume = (positions || []).find((p) => p.source === "sidle") || null;
  deviceResumes = (positions || []).filter((p) => p.source === "device");
  const pages = openDto.pages || [];
  const eidToPage = buildPdfEidIndex(pages);
  const hasText = pages.some((p) => p.words?.length);
  // Auto-restore Sidle's own last spot: its eid → page when it resolves (the new
  // anchor model), else the legacy page index stored in linear_pos.
  const resumeEid = sidleResume?.eid;
  const start = clampPage(
    (resumeEid != null ? eidToPage.get(resumeEid) : undefined) ?? sidleResume?.linear_pos ?? 0,
    openDto.page_count,
  );

  // One spread = up to two pages (left/right) in a flex host. Each page is a
  // positioned wrapper: the rendered <img> backdrop + a text layer of selectable
  // spans over it. Persistent elements (content swapped per turn) avoid a decode
  // flash. A single viewport-fixed overlay paints highlights/bookmarks/search,
  // so its SVG coordinates match the spans' `getClientRects()` directly.
  const host = document.createElement("div");
  host.className = "reader-pdf-spread";
  // RTL (Japanese/manga): the spread renders right-to-left (lower page on the
  // right). CSS row-reverse flips only the visual order — pageL stays the lower
  // index, so every coordinate/overlay calc downstream is unchanged.
  if (openDto.page_progression_direction === "rtl") host.classList.add("rtl");
  const mkPage = () => {
    const wrap = document.createElement("div");
    wrap.className = "reader-pdf-page";
    const img = document.createElement("img");
    img.className = "reader-pdf-img";
    img.alt = "";
    img.draggable = false;
    // Ink layer sits between the image and the (topmost) text layer.
    const ink = document.createElement("div");
    ink.className = "reader-pdf-ink";
    const text = document.createElement("div");
    text.className = "reader-pdf-text";
    wrap.append(img, ink, text);
    return { wrap, img, ink, text };
  };
  const L = mkPage();
  const R = mkPage();
  host.append(L.wrap, R.wrap);

  pdf = {
    pageCount: openDto.page_count,
    pages,
    ppd: openDto.page_progression_direction === "rtl" ? "rtl" : "ltr",
    labels: openDto.page_labels || [],
    toc: openDto.toc || [],
    page: start,
    host,
    pageL: L.wrap,
    pageR: R.wrap,
    imgL: L.img,
    imgR: R.img,
    textL: L.text,
    textR: R.text,
    inkL: L.ink,
    inkR: R.ink,
    overlayer: null,
    eidToPage,
    hasText,
    token: 0,
    renderTimer: null, // debounce handle for pdfScheduleRender
    cache: new Map(), // `${page}@${width}` → data URL (bounded; LRU-ish by insertion)
    inflight: new Map(), // key → Promise<url|null>, so a turn can await a prefetch
    inkPages: new Set(), // host pages that carry handwritten ink
    inkCache: new Map(), // page → Promise<string[]> (the overlay SVG(s))
  };

  // Which pages carry handwritten ink drawn on the Scribe — fetched once so the
  // spread renderer only requests the SVG for pages that actually have it.
  try {
    for (const p of (await window.api.invoke("reader_pdf_ink_pages", { bookId })) || []) {
      pdf.inkPages.add(p);
    }
  } catch {
    /* no ink layer — leave the set empty */
  }

  // PDF books have no reflowable Location map; native annotations carry a null
  // Loc (the device computes its own). eidToSection is reflowable-only.
  locByEid = null;
  eidToSection = null;

  $("#reader-title").textContent = openDto.title || "Untitled";
  $("#reader-loc").textContent = "";
  $("#reader-percent").textContent = "";
  // Search applies only when there's a text layer; bookmark + annotations apply
  // to both. Reset everything visible first (a prior book may have hidden them).
  for (const sel of ["#reader-bookmark", "#reader-search", "#reader-annotations"]) {
    const el = $(sel);
    if (el) el.hidden = false;
  }
  if (!hasText) {
    for (const sel of PDF_NO_TEXT_HIDDEN) {
      const el = $(sel);
      if (el) el.hidden = true;
    }
  }
  view().hidden = false;
  view().classList.add("open");
  revealTopbar();
  renderPdfTocPanel();
  renderAnnotationsPanel();
  renderResumeControl();
  syncPdfStylePanel();
  applyPdfStyle(); // night-mode class before the first paint

  $("#reader-paginator-host").replaceChildren(host);
  clearPdfOverlay(); // create + mount the viewport overlay
  // Selection / click on the spread reuse the reflowable handlers against the
  // main document (the spans live there, not in an iframe).
  host.addEventListener("mouseup", () => onSelection(document));
  host.addEventListener("click", (e) => onDocClick(e, document));
  host.addEventListener("contextmenu", (e) => e.preventDefault());
  // (Resize handling — re-render the spread at the new size — is folded into the
  // shared rAF-debounced window resize listener wired in init.)

  await pdfRenderCurrent();
  pdfUpdateProgress();
  markPdfTocActive();

  keyHandler = onKey; // onKey forwards to pdfOnKey while readerMode === "pdf"
  document.addEventListener("keydown", keyHandler, true);
}

// eid → page index for a PDF book: every word's eid and every page's structural
// eids (image/container/page_template) map to that page. First page wins, so an
// eid shared structurally resolves to its earliest page. Backs annotation /
// search / resume → page navigation, including image-only pages (whose bookmark
// anchors to a page eid that has no word).
function buildPdfEidIndex(pages) {
  const map = new Map();
  pages.forEach((p, i) => {
    for (const w of p.words || []) if (!map.has(w.eid)) map.set(w.eid, i);
    for (const e of p.eids || []) if (!map.has(e)) map.set(e, i);
  });
  return map;
}

// (Re)create the single viewport-fixed overlay that paints PDF highlights /
// bookmarks / search. A fresh Overlayer per call drops ranges that point at the
// previous spread's now-detached spans (cheaper + safer than tracking keys).
function clearPdfOverlay() {
  if (!pdf) return;
  pdf.overlayer?.element.remove();
  const ov = new Overlayer();
  // The Overlayer sets `position:absolute; width:100%` inline; override to a
  // viewport-fixed box so its SVG user-coords match the spans' viewport
  // `getClientRects()` directly. Inline (beats any class). Below topbar/panels
  // (z 1100), above the page image; pointer-transparent so selection reaches
  // the spans.
  Object.assign(ov.element.style, {
    position: "fixed",
    inset: "0",
    top: "0",
    left: "0",
    width: "100vw",
    height: "100vh",
    pointerEvents: "none",
    zIndex: "6",
  });
  ($("#reader-stage") || document.body).appendChild(ov.element);
  pdf.overlayer = ov;
  overlays = [{ doc: document, overlayer: ov }];
}

async function closePdf() {
  // Save Sidle's own last position (best-effort): the current page's
  // representative eid (so it maps back to a page on reopen, like the device's
  // last-read), plus the page index in linear_pos as a legacy fallback.
  if (bookId != null && pdf) {
    try {
      await window.api.invoke("reading_position_set", {
        bookId,
        eid: pdfRepresentativeEid(pdf.page),
        offset: 0,
        linearPos: pdf.page,
      });
    } catch {
      /* best-effort */
    }
  }
  if (keyHandler) {
    document.removeEventListener("keydown", keyHandler, true);
    keyHandler = null;
  }
  if (pdf?.renderTimer) clearTimeout(pdf.renderTimer);
  pdf?.overlayer?.element.remove();
  $("#reader-paginator-host")?.replaceChildren();
  // Restore any buttons hidden for an image-only book (next book may need them).
  for (const sel of ["#reader-bookmark", "#reader-search", "#reader-annotations"]) {
    const el = $(sel);
    if (el) el.hidden = false;
  }
  hideAnnotationsPanel();
  hideSearchPanel();
  if ($("#reader-pdf-style-panel")) $("#reader-pdf-style-panel").hidden = true;
  overlays = [];
  annotations = [];
  sidleResume = null;
  deviceResumes = [];
  searchResults = [];
  searchQuery = "";
  pdf = null;
  pdfTocRows = [];
  dto = null;
  bookId = null;
  readerMode = "reflowable";
  if ($("#reader-loc")) $("#reader-loc").textContent = "";
  if ($("#reader-percent")) $("#reader-percent").textContent = "";
  hideTocPanel();
  cancelTopbarHide();
  topbarEl()?.classList.remove("is-hidden");
  const v = view();
  if (v) {
    v.classList.remove("open");
    v.hidden = true;
  }
}

function pdfGoTo(i) {
  if (!pdf) return;
  const p = pdfSpreadStart(clampPage(i, pdf.pageCount));
  if (p === pdf.page && pdf.imgL.src) return;
  pdf.page = p;
  pdfUpdateProgress(); // cheap — reflect the new page number immediately
  markPdfTocActive();
  pdfScheduleRender();
}

// Render the current spread — instant when it's already cached (the prefetched
// common case), else debounced. Holding the page-turn key then flips through
// cached pages smoothly and coalesces past the cache to the page you land on,
// so renders don't pile onto the PDFKit render backend.
function pdfScheduleRender() {
  clearTimeout(pdf.renderTimer);
  const half = pdfSpreadMode() === "double";
  if (pdf.cache.has(`${pdf.page}@${pdfRenderWidth(pdf.page, half)}`)) {
    pdfRenderCurrent();
  } else {
    pdf.renderTimer = setTimeout(() => pdf && pdfRenderCurrent(), 90);
  }
}

// Render width to request for a page: fit it to the stage height, but in a
// `half` (double-page) layout also cap to half the stage width so the spread
// fits. At device resolution, capped (so a huge HiDPI window doesn't ask for an
// absurd bitmap) and quantized to 50px so layout jitter doesn't bust the cache.
function pdfRenderWidth(page, half) {
  const stage = $("#reader-stage");
  const sw = stage?.clientWidth || 1200;
  const sh = (stage?.clientHeight || 800) - 16; // a little breathing room
  const p = pdf.pages[page] || { width: 612, height: 792 };
  const aspect = p.width / Math.max(1, p.height);
  const dpr = window.devicePixelRatio || 1;
  let dispW = sh * aspect; // fit to height
  const budget = half ? (sw - 24) / 2 : sw; // ...but never exceed the width share
  if (dispW > budget) dispW = budget;
  const zoom = pdfStyle.zoom || 1; // request a bigger raster when zoomed, so it stays crisp
  const raw = Math.max(200, Math.min(Math.round(dispW * dpr * zoom), 3000));
  return Math.round(raw / 50) * 50;
}

const PDF_CACHE_MAX = 12; // rendered pages kept in memory (current ± neighbours)

function pdfTrimCache() {
  // Map preserves insertion order — drop the oldest entries past the cap.
  while (pdf.cache.size > PDF_CACHE_MAX) {
    pdf.cache.delete(pdf.cache.keys().next().value);
  }
}

// Fetch (and cache) a page's image, returning its data URL (or null on error).
// Coalesces with an in-flight request for the same key — so navigating onto a
// page whose prefetch is still running awaits that prefetch instead of erroring.
function pdfFetchPage(page, width) {
  const key = `${page}@${width}`;
  const hit = pdf.cache.get(key);
  if (hit) return Promise.resolve(hit);
  const pending = pdf.inflight.get(key);
  if (pending) return pending;
  const p = (async () => {
    try {
      const b64 = await window.api.invoke("reader_pdf_page", { bookId, page, width });
      const url = `data:image/jpeg;base64,${b64}`;
      if (pdf) {
        pdf.cache.set(key, url);
        pdfTrimCache();
      }
      return url;
    } catch {
      return null;
    } finally {
      pdf?.inflight.delete(key);
    }
  })();
  pdf.inflight.set(key, p);
  return p;
}

// The displayed size (CSS px) of a page in the current spread: fit to the stage
// height, capped to its width share (half the stage in a double spread). Matches
// the contained-image box exactly, so the wrapper holds the image *and* the text
// layer with no letterboxing — spans positioned by page-fraction then align.
function pdfDisplaySize(page, half) {
  const stage = $("#reader-stage");
  const sw = (stage?.clientWidth || 1200) - 16; // paginator-host padding (8×2)
  const sh = (stage?.clientHeight || 800) - 16;
  const p = pdf.pages[page] || { width: 612, height: 792 };
  const aspect = (p.width || 612) / Math.max(1, p.height || 792);
  let h = Math.max(1, sh);
  let w = h * aspect;
  const budget = half ? (sw - 12) / 2 : sw; // .double gap is 12px
  if (w > budget) {
    w = budget;
    h = w / aspect;
  }
  const zoom = pdfStyle.zoom || 1; // enlarge the page box past fit; the viewport scrolls
  return { w: Math.floor(w * zoom), h: Math.floor(h * zoom) };
}

function sizePdfPage(wrap, page, half) {
  const { w, h } = pdfDisplaySize(page, half);
  wrap.style.width = `${w}px`;
  wrap.style.height = `${h}px`;
}

// Lay a page's KFX text layer over its image as transparent, selectable spans:
// each run absolutely positioned by its page-fraction box, the text scaled
// horizontally (scaleX) to fill the run width so selection + highlight rects
// track the underlying glyphs. Empty for an image-only page.
function renderPdfTextLayer(textEl, page) {
  textEl.replaceChildren();
  const words = pdf.pages[page]?.words;
  if (!words?.length) return;
  const box = textEl.getBoundingClientRect();
  const frag = document.createDocumentFragment();
  const spans = [];
  for (const w of words) {
    const s = document.createElement("span");
    s.className = "reader-pdf-word";
    s.setAttribute("data-eid", w.eid);
    s.textContent = w.text;
    s.style.left = `${w.left * 100}%`;
    s.style.top = `${w.top * 100}%`;
    s.style.height = `${w.height * 100}%`;
    s.style.fontSize = `${Math.max(1, w.height * box.height)}px`;
    frag.appendChild(s);
    spans.push([s, w]);
  }
  textEl.appendChild(frag);
  // Batch the reads (one reflow) then the writes, so fitting N runs doesn't
  // thrash layout.
  const natW = spans.map(([s]) => s.getBoundingClientRect().width);
  spans.forEach(([s, w], i) => {
    const target = w.width * box.width;
    if (natW[i] > 0 && target > 0) s.style.transform = `scaleX(${target / natW[i]})`;
  });
}

// Lay a page's handwritten-ink SVG(s) over its image, under the text layer.
// Async (the cached overlay SVG is fetched once per page) and token-guarded so a
// superseded turn never paints stale ink. A no-ink page — or ink toggled off —
// clears the layer. The inner SVG (canvas-unit viewBox) is stretched to the page
// box: the device maps its drawing surface onto the page rectangle.
async function renderPdfInkLayer(inkEl, page, token) {
  if (!inkEl) return;
  inkEl.replaceChildren();
  if (page == null || !pdfStyle.ink || !pdf?.inkPages.has(page)) return;
  const svgs = await pdfFetchInk(page);
  if (!pdf || pdf.token !== token) return; // a newer turn superseded us
  inkEl.replaceChildren(); // the element may have been reused meanwhile
  for (const svg of svgs) {
    if (!svg) continue;
    const holder = document.createElement("div");
    holder.innerHTML = svg;
    const svgEl = holder.querySelector("svg");
    if (!svgEl) continue;
    svgEl.setAttribute("preserveAspectRatio", "none");
    inkEl.appendChild(svgEl);
  }
}

// Fetch (and cache) a page's ink overlay SVG(s) — usually one per page. Caches
// the promise so repeated renders of the same page coalesce.
function pdfFetchInk(page) {
  const hit = pdf.inkCache.get(page);
  if (hit) return hit;
  const p = (async () => {
    try {
      const rows = await window.api.invoke("reader_pdf_ink", { bookId, page });
      return (rows || []).map((r) => r.svg).filter(Boolean);
    } catch {
      return [];
    }
  })();
  pdf.inkCache.set(page, p);
  return p;
}

// The page's representative eid: its first text run, else its first structural
// eid (image-only). Anchors the live position (bookmark) + saved last-read so
// they map back to a page.
function pdfRepresentativeEid(page) {
  const p = pdf?.pages[page];
  if (!p) return null;
  if (p.words?.length) return p.words[0].eid;
  if (p.eids?.length) return p.eids[0];
  return null;
}

// The visible wrapper holding `page`, or null if `page` isn't in the spread.
function pdfVisibleWrapper(page) {
  if (page == null || !pdf) return null;
  if (page === pdf.page) return pdf.pageL;
  if (pdf.pageR.style.display !== "none" && page === pdf.page + 1) return pdf.pageR;
  return null;
}

// Repaint the overlay for the visible spread: a fresh overlayer (dropping the
// previous spread's stale ranges), then highlights/notes + bookmarks + any
// active search, all resolved against the live spans.
function repaintPdfOverlay() {
  if (!pdf) return;
  clearPdfOverlay();
  const ov = pdf.overlayer;
  paintAnnotations(document, ov); // highlights/notes + text-anchored bookmarks
  paintPdfPageBookmarks(ov); // corner marker for page-level (image-only) bookmarks
  if (searchResults.length) paintSearchMatches(document, ov);
}

// Paint a corner marker at the top-right of each bookmarked page in the spread —
// the Kindle's bookmark-ribbon convention, applied to every PDF bookmark
// (native or imported) so they're consistent. A bookmark's anchor eid resolves
// only to its *page* here (not a text range), matching how a fixed-layout
// bookmark reads on the device. Same `ann-<id>` key as the reflowable painter,
// so a removed bookmark clears with the fresh overlayer.
function paintPdfPageBookmarks(ov) {
  for (const ann of annotations) {
    if (ann.kind !== "bookmark" || ann.eid_start == null) continue;
    const wrap = pdfVisibleWrapper(pdf.eidToPage.get(ann.eid_start));
    if (!wrap) continue;
    const r = wrap.getBoundingClientRect();
    const rect = { left: r.right - 22, top: r.top + 6, right: r.right - 6, bottom: r.top + 22, width: 16, height: 16 };
    ov.add(`ann-${ann.id}`, { getClientRects: () => [rect] }, drawBookmarkMarker, { color: BOOKMARK_COLOR });
  }
}

// Render the current spread (1 or 2 pages by the spread mode), then warm the
// next + previous spread so a turn is immediate. The page wrapper(s) + text
// layer(s) are placed synchronously (so selection + the overlay are live before
// the image arrives); both <img> srcs swap **together** when fetched — so a
// two-page turn updates as one frame, never left-then-right.
async function pdfRenderCurrent() {
  if (!pdf) return;
  const token = ++pdf.token;
  const half = pdfSpreadMode() === "double";
  // Snap onto a spread boundary (cover alone, then odd-aligned pairs) so a
  // resize that flips single↔double — or an init/jump straight to a mid-spread
  // page — still renders a clean spread.
  pdf.page = pdfSpreadStart(pdf.page);
  const left = pdf.page;
  const hasRight = pdfHasRight();
  pdf.host.classList.toggle("double", half);
  pdf.pageR.style.display = hasRight ? "" : "none";

  // Wrappers + text layers first — independent of the image fetch.
  sizePdfPage(pdf.pageL, left, half);
  renderPdfTextLayer(pdf.textL, left);
  if (hasRight) {
    sizePdfPage(pdf.pageR, left + 1, half);
    renderPdfTextLayer(pdf.textR, left + 1);
  } else {
    pdf.textR.replaceChildren();
  }
  // Handwritten-ink overlays (async; token-guarded) — fire-and-forget alongside
  // the image fetch, so they paint as soon as the cached SVG resolves.
  renderPdfInkLayer(pdf.inkL, left, token);
  renderPdfInkLayer(pdf.inkR, hasRight ? left + 1 : null, token);
  // The current page's eid is the live position (drives the bookmark toggle).
  livePosition = { eid: pdfRepresentativeEid(left), offset: 0, linear_pos: null };
  updateBookmarkButton();
  repaintPdfOverlay();

  const [urlL, urlR] = await Promise.all([
    pdfFetchPage(left, pdfRenderWidth(left, half)),
    hasRight ? pdfFetchPage(left + 1, pdfRenderWidth(left + 1, true)) : Promise.resolve(null),
  ]);
  if (!pdf || pdf.token !== token) return; // a newer turn superseded us
  if (urlL) pdf.imgL.src = urlL;
  else toast(`Couldn't render page ${left + 1}`, true);
  if (hasRight && urlR) pdf.imgR.src = urlR;
  pdf.overlayer?.redraw(); // wrapper size is final now — settle any sub-pixel drift

  // Prefetch the neighbouring spreads (best-effort, off the critical path). Use
  // the spread starts so the warm targets match real turns — notably cover →
  // (1,2), which the old even-aligned `left ± step` would have missed.
  const step = half ? 2 : 1;
  const nextStart = pdfSpreadStart(left + step);
  const prevStart = pdfSpreadStart(left - step);
  const warm = [nextStart, prevStart];
  if (half) {
    if (pdfSpreadHasRight(nextStart)) warm.push(nextStart + 1);
    if (pdfSpreadHasRight(prevStart)) warm.push(prevStart + 1);
  }
  for (const n of warm) {
    if (n >= 0 && n < pdf.pageCount) pdfFetchPage(n, pdfRenderWidth(n, half));
  }
}

function pdfUpdateProgress() {
  if (!pdf) return;
  // Mode 3 = hidden (the fixed-layout footer's only "off" state); else show.
  $("#reader-statusbar")?.classList.toggle("is-hidden", progressMode === 3);
  if (progressMode === 3) {
    $("#reader-loc").textContent = "";
    $("#reader-percent").textContent = "";
    return;
  }
  // The PDF's own label when it differs from the ordinal ("Cover", "xvii").
  const lbl = (i) => {
    const human = String(i + 1);
    const l = pdf.labels[i];
    return l && l !== human ? l : human;
  };
  const left = pdf.page;
  const right = pdfHasRight() ? left + 1 : null;
  $("#reader-loc").textContent =
    right != null ? `Pages ${lbl(left)}–${lbl(right)}` : `Page ${lbl(left)}`;
  const human = (right != null ? right : left) + 1;
  const pct = Math.round((human / pdf.pageCount) * 100);
  $("#reader-percent").textContent = `${human} / ${pdf.pageCount} · ${pct}%`;
}

// Mirrors the reflowable reader's key map (`onKey`) for the shortcuts that apply
// to a fixed page image. Same keys, same actions; drops the ones with no PDF
// meaning (search `⌘F`//`, annotations `a`, bookmark `b`, font `[`/`]`).
function pdfOnKey(e) {
  // A focused style-panel control owns its keys — only Esc (close it) is ours.
  if (e.target?.closest?.("#reader-pdf-style-panel")) {
    if (e.key === "Escape") {
      togglePdfStylePanel();
      e.preventDefault();
    }
    return;
  }
  if (e.key === "Escape") {
    // Peel back overlays/panels first, then close the reader.
    if (!$("#reader-selection-toolbar")?.hidden) hideSelectionToolbar();
    else if (!$("#reader-note-popover")?.hidden) hideNotePopover();
    else if (!$("#reader-pdf-style-panel")?.hidden) togglePdfStylePanel();
    else if (!$("#reader-search-panel")?.hidden) hideSearchPanel();
    else if (!$("#reader-annotations-panel")?.hidden) hideAnnotationsPanel();
    else if (!$("#reader-toc-panel")?.hidden) hideTocPanel();
    else close();
    e.preventDefault();
    return;
  }
  // ⌘F / Ctrl+F → open search (text books only), before the modifier filter —
  // mirrors the reflowable reader, replacing the browser's find-in-page.
  if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && (e.key === "f" || e.key === "F")) {
    if (pdf?.hasText) toggleSearchPanel();
    e.preventDefault();
    return;
  }
  // A focused search input / note editor owns its keys (arrows/space/typing) —
  // don't steal them to turn pages. (Escape above already closes them.)
  if (e.target?.closest?.("#reader-search-panel")) return;
  if (e.target?.closest?.("#reader-note-popover")) return;
  // `Shift+G` → last page (before the modifier filter, as the reflowable does).
  if (e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey && e.key === "G") {
    pdfGoTo(pdf.pageCount - 1);
    gArmed = 0;
    e.preventDefault();
    return;
  }
  // Zoom in/out/reset — shared fixed-layout control (same keys as the notebook).
  if (!e.ctrlKey && !e.metaKey && !e.altKey && handleZoomKey(e)) return;
  if (e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return;
  // `g g` chord → first page (shares the reflowable reader's arm timer).
  if (e.key === "g") {
    const now = Date.now();
    if (now - gArmed < G_CHORD_MS) {
      pdfGoTo(0);
      gArmed = 0;
    } else {
      gArmed = now;
    }
    e.preventDefault();
    return;
  }
  gArmed = 0;
  let handled = true;
  const rtl = pdf?.ppd === "rtl"; // RTL: the next page is to the left
  switch (e.key) {
    case "ArrowRight": // physical right
      rtl ? back() : forward();
      break;
    case "ArrowLeft": // physical left
      rtl ? forward() : back();
      break;
    case "ArrowDown":
    case "PageDown":
    case " ":
      forward(); // logical advance (steps by the spread: 1 or 2 pages)
      break;
    case "ArrowUp":
    case "PageUp":
      back();
      break;
    case "t":
      toggleTocPanel();
      break;
    case "s":
      togglePdfStylePanel();
      break;
    case "b":
      toggleBookmark();
      break;
    case "/":
      if (pdf?.hasText) toggleSearchPanel();
      else handled = false;
      break;
    case "n":
      pdfJumpBookmark(1);
      break;
    case "p":
      pdfJumpBookmark(-1);
      break;
    default:
      handled = false;
  }
  if (handled) e.preventDefault();
}

// `n`/`p` analogue of the reflowable next/prev-chapter: jump to the next or
// previous TOC bookmark's page. No-op when the PDF has no outline.
function pdfJumpBookmark(dir) {
  const pages = pdf?.tocPages;
  if (!pages?.length) return;
  if (dir > 0) {
    const next = pages.find((p) => p > pdf.page);
    if (next != null) pdfGoTo(next);
  } else {
    let prev = null;
    for (const p of pages) {
      if (p < pdf.page) prev = p;
      else break;
    }
    if (prev != null) pdfGoTo(prev);
  }
}

function renderPdfTocPanel() {
  pdfTocRows = [];
  const btn = $("#reader-toc");
  if (btn) btn.hidden = (pdf.toc?.length ?? 0) === 0;
  const list = $("#reader-toc-list");
  if (!list) return;
  const rows = [];
  for (const t of pdf.toc) pdfTocRowsFor(t, 0, rows);
  list.replaceChildren(...rows);
  $("#reader-toc-empty").hidden = rows.length > 0;
  // Sorted unique bookmark pages, for `n`/`p` next/prev-bookmark navigation.
  pdf.tocPages = [...new Set(pdfTocRows.map((r) => r.page))].sort((a, b) => a - b);
}

function pdfTocRowsFor(entry, depth, rows) {
  // Same DOM/class/indent as the reflowable `tocRowsFor`, so the panel looks and
  // behaves identically — only the target differs (a page index, not an href).
  const li = document.createElement("li");
  li.className = "toc-row";
  li.style.paddingLeft = `${14 + depth * 14}px`; // UI chrome is always LTR
  li.textContent = entry.label || "—";
  // Match the reflowable TOC: jump but leave the panel open (pick another entry).
  li.addEventListener("click", () => pdfGoTo(entry.page_index));
  rows.push(li);
  pdfTocRows.push({ li, page: entry.page_index });
  for (const c of entry.children || []) pdfTocRowsFor(c, depth + 1, rows);
}

function markPdfTocActive() {
  if (!pdfTocRows.length) return;
  let active = -1;
  for (let i = 0; i < pdfTocRows.length; i++) {
    if (pdfTocRows[i].page <= pdf.page) active = i;
  }
  pdfTocRows.forEach(({ li }, i) => li.classList.toggle("active", i === active));
}

// ---- PDF display settings (spread + night mode) ---------------------------

function togglePdfStylePanel() {
  const p = $("#reader-pdf-style-panel");
  if (p) p.hidden = !p.hidden;
}

function syncPdfStylePanel() {
  // The notebook shares this panel: page layout (spread) + night mode apply, but
  // a handwritten page has no separable ink layer, so hide the ink-toggle row.
  const notebook = readerMode === "notebook";
  $("#rps-ink")?.closest(".rs-row")?.toggleAttribute("hidden", notebook);
  // The standalone cover is a PDF concept; a notebook always pairs from page 0.
  $("#rps-cover")?.closest(".rs-row")?.toggleAttribute("hidden", notebook);
  const sp = $("#rps-spread");
  if (sp) sp.value = pdfStyle.spread;
  const cov = $("#rps-cover");
  if (cov) cov.checked = pdfStyle.cover !== false;
  const inv = $("#rps-invert");
  if (inv) inv.checked = !!pdfStyle.invert;
  const ink = $("#rps-ink");
  if (ink) ink.checked = pdfStyle.ink !== false;
  syncZoomControl();
}

// Reflect the current zoom on the panel slider + its "150%" label.
function syncZoomControl() {
  const z = pdfStyle.zoom || 1;
  const sl = $("#rps-zoom");
  if (sl) sl.value = String(z);
  const lbl = $("#rps-zoom-val");
  if (lbl) lbl.textContent = `${Math.round(z * 100)}%`;
}

// Night mode = CSS-invert the page (white→black) on the spread host (PDF) or the
// paginator host (notebook, where the class survives page turns). No re-render.
// (Zoom is applied by the page-sizing math — pdfDisplaySize / nbkDisplaySize — not
// here, so it scales the box uniformly and the viewport scrolls.)
function applyPdfStyle() {
  if (pdf) pdf.host.classList.toggle("invert", !!pdfStyle.invert);
  else if (nbk) $("#reader-paginator-host")?.classList.toggle("invert", !!pdfStyle.invert);
}

function setPdfSpread(v) {
  pdfStyle.spread = v;
  savePdfStyle();
  if (nbk) {
    nbkShowPage(nbk.page); // re-render the notebook spread at the new layout
  } else {
    pdfRenderCurrent();
    pdfUpdateProgress();
  }
}

// Toggle the standalone cover (page 0 alone, then odd-aligned pairs) vs pairing
// from page 0. PDF-only — the notebook always pairs from 0 — and a no-op unless
// the current spread is double; pdfRenderCurrent re-snaps `pdf.page` to the new
// boundary so a turn from either alignment lands cleanly.
function setPdfCover(on) {
  pdfStyle.cover = !!on;
  savePdfStyle();
  if (pdf) {
    pdfRenderCurrent();
    pdfUpdateProgress();
  }
}

function setPdfInvert(on) {
  pdfStyle.invert = !!on;
  savePdfStyle();
  applyPdfStyle();
}

// Show/hide the handwritten-ink overlay. Re-renders just the visible spread's
// ink layers (no page re-fetch — the SVGs are cached).
function setPdfInk(on) {
  pdfStyle.ink = !!on;
  savePdfStyle();
  if (!pdf) return;
  renderPdfInkLayer(pdf.inkL, pdf.page, pdf.token);
  renderPdfInkLayer(pdf.inkR, pdfHasRight() ? pdf.page + 1 : null, pdf.token);
}

let zoomCommitTimer = null;
// Shared fixed-layout zoom (PDF + notebook), clamped to [1, 3]. Resizes the visible
// page(s) immediately and cheaply — no SVG re-parse (notebook) or raster re-fetch
// (PDF) — so slider drags and trackpad pinches stay smooth. The notebook is vector,
// so the resize is already crisp; the PDF raster just scales until a debounced
// re-render redraws it crisply at the settled zoom.
function setPdfZoom(z, anchor) {
  const old = pdfStyle.zoom || 1;
  const next = Math.max(PDF_ZOOM_MIN, Math.min(PDF_ZOOM_MAX, Math.round((z || 1) * 100) / 100));
  if (next === old) return;
  pdfStyle.zoom = next;
  savePdfStyle();
  syncZoomControl();
  if (pdf) {
    pdfResize();
    clearTimeout(zoomCommitTimer);
    zoomCommitTimer = setTimeout(() => pdf && pdfScheduleRender(), 160);
  } else if (nbk) {
    nbkResize();
  }
  zoomAnchorScroll(next / old, anchor); // zoom toward the pinch point, not the corner
}

// Keep the content point under `anchor` (client coords; falls back to the viewport
// centre for slider/keys) fixed while the page scales by `f` — so a pinch zooms
// toward the fingers. A no-op when the page fits (scrollLeft/Top clamp to 0).
function zoomAnchorScroll(f, anchor) {
  const host = $("#reader-paginator-host");
  if (!host) return;
  const rect = host.getBoundingClientRect();
  const pad = 8; // .reader-paginator-host padding (keep in sync with styles.css)
  const ax = anchor && Number.isFinite(anchor.x) ? anchor.x : rect.left + rect.width / 2;
  const ay = anchor && Number.isFinite(anchor.y) ? anchor.y : rect.top + rect.height / 2;
  const u = ax - rect.left - pad;
  const v = ay - rect.top - pad;
  host.scrollLeft = (host.scrollLeft + u) * f - u;
  host.scrollTop = (host.scrollTop + v) * f - v;
}

// Cheap re-fit of the on-screen PDF page boxes to the current zoom; the existing
// raster scales until the debounced crisp render lands.
function pdfResize() {
  if (!pdf) return;
  const half = pdfSpreadMode() === "double";
  sizePdfPage(pdf.pageL, pdf.page, half);
  if (pdfHasRight()) sizePdfPage(pdf.pageR, pdf.page + 1, half);
  pdf.overlayer?.redraw();
}
function bumpZoom(delta) {
  setPdfZoom((pdfStyle.zoom || 1) + delta);
}
// Handle a zoom keystroke (+ / = zoom in · - / _ zoom out · 0 reset to fit).
// Returns true if `e` was a zoom key (so the caller stops processing it).
function handleZoomKey(e) {
  if (e.key === "+" || e.key === "=") bumpZoom(0.25);
  else if (e.key === "-" || e.key === "_") bumpZoom(-0.25);
  else if (e.key === "0") setPdfZoom(1);
  else return false;
  e.preventDefault();
  return true;
}

// ---- notebook (handwritten Scribe) mode -----------------------------------
//
// A Scribe notebook is a fixed-layout handwritten page: one inline SVG per page
// (`notebook_page_svg`), with no text / search / annotations / reflow. It rides
// the same reader shell as a book — the PDF pattern taken one step further — so
// it inherits the topbar + auto-hide, footer progress, nav zones, `--reader-*`
// tokens, and Esc-peel keyboard skeleton for free. The page SVG carries its own
// viewBox and self-sizes (`.reader-notebook-page`), so there's no raster pipeline
// and no JS sizing: the renderer is simpler than the PDF's.
//
// Phase-gated capability list — everything a handwritten page can't act on is
// hidden. Go-to-page and the page bookmark arrive in later phases and re-reveal
// their controls then. (Aa / display settings is wired as of Phase 2.)
const NOTEBOOK_HIDDEN = [
  "#reader-toc",
  "#reader-bookmark",
  "#reader-search",
  "#reader-annotations",
];

// `desc` = `{ id, title, pageCount }`, built by notebooks.js from the library row
// (it owns the title fallback). Mirrors how library.js opens a book via
// `sidleReader.open(id)`; keeps the book/PDF entry path untouched.
async function openNotebook(desc) {
  await close(); // tear down any prior session
  readerMode = "notebook";
  pdfStyle.zoom = 1; // zoom is per-open: a notebook opens at fit, not the last zoom
  nbk = {
    id: desc.id,
    title: desc.title || "Notebook",
    pageCount: desc.pageCount || 0,
    page: 0,
    cache: new Map(), // page → SVG string (prefetched neighbours included)
    token: 0, // bumps per turn so a slow fetch can't paint a stale page
    aspect: 0.75, // page W/H (portrait default); learned from the first rendered SVG
  };
  $("#reader-title").textContent = nbk.title;
  $("#reader-loc").textContent = "";
  $("#reader-percent").textContent = "";
  // Hide every chrome affordance a handwritten page (this phase) can't use.
  for (const sel of NOTEBOOK_HIDDEN) {
    const el = $(sel);
    if (el) el.hidden = true;
  }
  view().hidden = false;
  view().classList.add("open");
  revealTopbar();
  syncPdfStylePanel(); // shared fixed-layout panel: sync values + per-mode rows
  applyPdfStyle(); // night-mode (invert) on the host per the saved global pref
  if (nbk.pageCount > 0) {
    await nbkShowPage(0);
  } else {
    $("#reader-paginator-host")?.replaceChildren();
    nbkUpdateProgress();
  }
  keyHandler = onKey; // onKey forwards to notebookOnKey while readerMode === "notebook"
  document.addEventListener("keydown", keyHandler, true);
}

// Synchronous in this phase; becomes async when Phase 4 saves the last-read page.
function closeNotebook() {
  if (keyHandler) {
    document.removeEventListener("keydown", keyHandler, true);
    keyHandler = null;
  }
  $("#reader-paginator-host")?.replaceChildren();
  $("#reader-paginator-host")?.classList.remove("invert"); // drop night-mode state
  if ($("#reader-pdf-style-panel")) $("#reader-pdf-style-panel").hidden = true;
  // Restore the chrome hidden on open (the next book needs it). #reader-toc is
  // omitted — every open re-evaluates its visibility via renderTocPanel.
  for (const sel of ["#reader-bookmark", "#reader-search", "#reader-annotations"]) {
    const el = $(sel);
    if (el) el.hidden = false;
  }
  nbk = null;
  bookId = null;
  readerMode = "reflowable";
  if ($("#reader-loc")) $("#reader-loc").textContent = "";
  if ($("#reader-percent")) $("#reader-percent").textContent = "";
  cancelTopbarHide();
  topbarHovered = false;
  topbarEl()?.classList.remove("is-hidden");
  const v = view();
  if (v) {
    v.classList.remove("open");
    v.hidden = true;
  }
}

// Render the spread at page `i` (clamped): one page, or two side by side in double
// mode. SVGs are cached and the next spread prefetched, so a turn is usually
// instant. Token-guarded: a slow fetch for a page you've already left can't paint
// over the current one. The page SVG is viewBox-only and self-sizes via CSS
// (`.reader-notebook-page`), reusing the PDF `.reader-pdf-spread` host + gutter.
async function nbkShowPage(i) {
  if (!nbk || nbk.pageCount === 0) return;
  i = clampPage(i, nbk.pageCount);
  nbk.page = i;
  const token = ++nbk.token;
  nbkUpdateProgress(); // immediate page readout (uses the cached aspect)

  const leftSvg = await nbkFetch(i);
  if (leftSvg == null || !nbk || nbk.token !== token || nbk.page !== i) return;
  const learned = nbkAspect(leftSvg);
  if (learned && learned !== nbk.aspect) {
    nbk.aspect = learned; // page geometry now known — may flip an auto spread
    nbkUpdateProgress();
  }

  const double = nbkSpreadMode() === "double";
  let rightSvg = "";
  if (double && i + 1 < nbk.pageCount) {
    const r = await nbkFetch(i + 1);
    if (!nbk || nbk.token !== token || nbk.page !== i) return;
    rightSvg = r || "";
  }

  const host = $("#reader-paginator-host");
  if (!host) return;
  const spread = document.createElement("div");
  spread.className = double ? "reader-pdf-spread double" : "reader-pdf-spread";
  spread.innerHTML = leftSvg + rightSvg;
  // Size each page box explicitly (fit × zoom) so it scrolls when zoomed. Only the
  // outermost (page) SVGs get sized/classed — not the nested template SVG inside.
  const { w, h } = nbkDisplaySize(nbk.aspect, double);
  for (const el of spread.querySelectorAll(":scope > svg")) {
    el.classList.add("reader-notebook-page");
    el.style.width = `${w}px`;
    el.style.height = `${h}px`;
  }
  host.replaceChildren(spread);

  const step = double ? 2 : 1;
  nbkPrefetch(i + step);
  if (double) nbkPrefetch(i + step + 1);
}

// Fetch (and cache) one page's SVG string; null + toast on error.
async function nbkFetch(i) {
  const v = nbk;
  let svg = v.cache.get(i);
  if (svg == null) {
    try {
      svg = await window.api.invoke("notebook_page_svg", { notebookId: v.id, page: i });
      if (nbk === v) v.cache.set(i, svg);
    } catch (e) {
      toast(`Couldn't render page ${i + 1}: ${e}`, true);
      return null;
    }
  }
  return svg;
}

// Warm a page's SVG so the next turn paints without a fetch.
function nbkPrefetch(i) {
  const v = nbk;
  if (!v || i < 0 || i >= v.pageCount || v.cache.has(i)) return;
  window.api
    .invoke("notebook_page_svg", { notebookId: v.id, page: i })
    .then((svg) => {
      if (nbk === v) v.cache.set(i, svg);
    })
    .catch(() => {});
}

// Page aspect (W/H) from the outer SVG's viewBox — 0 if unparseable.
function nbkAspect(svg) {
  const m =
    typeof svg === "string" &&
    svg.match(/viewBox=["']\s*[-\d.]+\s+[-\d.]+\s+([\d.]+)\s+([\d.]+)/);
  if (m) {
    const w = parseFloat(m[1]);
    const h = parseFloat(m[2]);
    if (w > 0 && h > 0) return w / h;
  }
  return 0;
}

// Effective single/double for the current stage + setting — mirrors pdfSpreadMode,
// using the (uniform) page aspect learned from the rendered SVG.
function nbkSpreadMode() {
  if (pdfStyle.spread === "single" || pdfStyle.spread === "double") return pdfStyle.spread;
  const stage = $("#reader-stage");
  const sw = stage?.clientWidth || 0;
  const sh = stage?.clientHeight || 1;
  const aspect = nbk?.aspect || 0.75;
  return sw >= 2 * sh * aspect * 0.98 ? "double" : "single";
}
function nbkStep() {
  return nbkSpreadMode() === "double" ? 2 : 1;
}

// Display size (CSS px) of one notebook page: fit to the stage height, capped to
// its width share (half in a double spread), then × zoom. Mirrors pdfDisplaySize —
// the SVG is sized explicitly (not CSS-contained) so zoom enlarges the box and the
// viewport scrolls. Aspect is the page's viewBox W/H, so the SVG fills with no gap.
function nbkDisplaySize(aspect, half) {
  const stage = $("#reader-stage");
  const sw = (stage?.clientWidth || 1200) - 16; // paginator-host padding (8×2)
  const sh = (stage?.clientHeight || 800) - 16;
  const a = aspect || 0.75;
  let h = Math.max(1, sh);
  let w = h * a;
  const budget = half ? (sw - 12) / 2 : sw; // .double gap is 12px
  if (w > budget) {
    w = budget;
    h = w / a;
  }
  const zoom = pdfStyle.zoom || 1;
  return { w: Math.max(1, Math.floor(w * zoom)), h: Math.max(1, Math.floor(h * zoom)) };
}

// Cheap re-fit of the on-screen notebook page(s) to the current zoom — resize the
// existing SVGs (crisp, vector) rather than rebuilding/re-parsing them.
function nbkResize() {
  const host = $("#reader-paginator-host");
  if (!nbk || !host) return;
  const { w, h } = nbkDisplaySize(nbk.aspect, nbkSpreadMode() === "double");
  for (const el of host.querySelectorAll(".reader-notebook-page")) {
    el.style.width = `${w}px`;
    el.style.height = `${h}px`;
  }
}

// Footer status: `Page X` left, `X / N · P%` right — exactly like pdfUpdateProgress,
// honoring the shared hidden mode (3) that the statusbar tap toggles to.
function nbkUpdateProgress() {
  if (!nbk) return;
  // Mode 3 = hidden (the fixed-layout footer's only "off" state); else show.
  $("#reader-statusbar")?.classList.toggle("is-hidden", progressMode === 3);
  if (progressMode === 3) {
    $("#reader-loc").textContent = "";
    $("#reader-percent").textContent = "";
    return;
  }
  if (!nbk.pageCount) {
    $("#reader-loc").textContent = "";
    $("#reader-percent").textContent = "—";
    return;
  }
  const left = nbk.page;
  const right = nbkSpreadMode() === "double" && left + 1 < nbk.pageCount ? left + 1 : null;
  $("#reader-loc").textContent =
    right != null ? `Pages ${left + 1}–${right + 1}` : `Page ${left + 1}`;
  const human = (right != null ? right : left) + 1;
  const pct = Math.round((human / nbk.pageCount) * 100);
  $("#reader-percent").textContent = `${human} / ${nbk.pageCount} · ${pct}%`;
}

// Go-to-page: swap the footer "Page X" readout for a number input (no native
// prompt). Enter jumps; Esc/blur cancels — then the readout is restored. notebookOnKey
// ignores the input's keys (its target guard below), so digits/arrows reach it.
function openNotebookGoTo() {
  if (!nbk || !nbk.pageCount) return;
  const locEl = $("#reader-loc");
  if (!locEl || locEl.querySelector("input")) return; // already open
  const input = document.createElement("input");
  input.type = "number";
  input.min = "1";
  input.max = String(nbk.pageCount);
  input.value = String(nbk.page + 1);
  input.className = "reader-goto-input";
  input.setAttribute("aria-label", `Go to page (1–${nbk.pageCount})`);
  locEl.replaceChildren(input);
  input.focus();
  input.select();
  let done = false;
  const finish = (jump) => {
    if (done) return;
    done = true;
    const n = parseInt(input.value, 10);
    if (jump && Number.isFinite(n)) nbkShowPage(n - 1); // also refreshes the readout
    else nbkUpdateProgress(); // restore "Page X"
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      finish(true);
    } else if (e.key === "Escape") {
      e.preventDefault();
      finish(false);
    }
  });
  input.addEventListener("blur", () => finish(false));
}

// Minimal fixed-layout key map (analog of pdfOnKey): page turns, first/last, go-to-
// page (`g`), display settings (`s`), zoom; Esc peels the panel/input, then closes.
function notebookOnKey(e) {
  if (!nbk) return;
  // The go-to-page input owns its keys (digits, Enter, Esc) while focused.
  if (e.target?.closest?.(".reader-goto-input")) return;
  // A focused style-panel control owns its keys — only Esc (close it) is ours.
  if (e.target?.closest?.("#reader-pdf-style-panel")) {
    if (e.key === "Escape") {
      togglePdfStylePanel();
      e.preventDefault();
    }
    return;
  }
  if (e.key === "Escape") {
    // Peel the display-settings panel first, then close the reader.
    if (!$("#reader-pdf-style-panel")?.hidden) togglePdfStylePanel();
    else close();
    e.preventDefault();
    return;
  }
  // Shift+G → last page (before the modifier filter, as the reflowable does).
  if (e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey && e.key === "G") {
    nbkShowPage(nbk.pageCount - 1);
    gArmed = 0;
    e.preventDefault();
    return;
  }
  // Zoom in/out/reset (before the modifier filter; "+" is Shift+"=" on many layouts).
  if (!e.ctrlKey && !e.metaKey && !e.altKey && handleZoomKey(e)) return;
  if (e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return;
  // `g` → go to page by number. Notebooks have no TOC, so this is the jump; Home/
  // End already cover first/last, so there's no vim `gg` chord here.
  if (e.key === "g") {
    openNotebookGoTo();
    e.preventDefault();
    return;
  }
  let handled = true;
  switch (e.key) {
    case "ArrowRight":
    case "ArrowDown":
    case "PageDown":
    case " ":
      nbkShowPage(nbk.page + nbkStep());
      break;
    case "ArrowLeft":
    case "ArrowUp":
    case "PageUp":
      nbkShowPage(nbk.page - nbkStep());
      break;
    case "Home":
      nbkShowPage(0);
      break;
    case "End":
      nbkShowPage(nbk.pageCount - 1);
      break;
    case "s":
      togglePdfStylePanel();
      break;
    default:
      handled = false;
  }
  if (handled) e.preventDefault();
}

// True while the reader is showing a notebook — lets library.js keep suppressing
// the Notes grid's keyboard/lasso while the overlay is up (notebooks.js owned
// this when the viewer lived there).
function isNotebookOpen() {
  return readerMode === "notebook" && !view().hidden;
}

async function open(id) {
  await close(); // tear down any prior session
  let openDto, anns, positions;
  try {
    [openDto, anns, positions] = await Promise.all([
      window.api.invoke("reader_open", { bookId: id }),
      window.api.invoke("annotations_for_book", { bookId: id }),
      window.api.invoke("reading_position_get", { bookId: id }),
    ]);
  } catch (err) {
    toast(`Couldn't open reader: ${err}`, true);
    return;
  }
  // PDF-backed (fixed-layout) books take the page-image path, reusing the same
  // topbar / TOC / status chrome. The reflowable setup below is skipped.
  if (openDto.mode === "pdf") {
    await openPdf(id, openDto, anns || [], positions || []);
    return;
  }
  readerMode = "reflowable";
  bookId = id;
  dto = openDto;
  annotations = anns || [];
  eidToSection = null;
  overlays = [];
  locByEid = new Map(dto.locations || []);
  maxLoc = dto.max_location || 0;
  positionedByDoc = new WeakMap();
  sectionChars = dto.sections.map((sec) => baseTextLen(sec.html));
  charsBefore = [];
  let cumChars = 0;
  for (const n of sectionChars) {
    charsBefore.push(cumChars);
    cumChars += n;
  }
  totalChars = cumChars;
  imageSections = new Set();
  dto.sections.forEach((sec, i) => {
    if (isImageOnlySection(sec.html)) imageSections.add(i);
  });
  layoutMode = null;
  const pace = loadPace();
  readSpeed = pace.speed;
  sampleCount = pace.count;
  lastTurnTime = null;
  lastCharPos = null;
  lastPos = null;
  livePosition = null;
  sidleResume = (positions || []).find((p) => p.source === "sidle") || null;
  deviceResumes = (positions || []).filter((p) => p.source === "device");
  styleSettings = loadStyle();
  book = makeKfxBook(dto);

  $("#reader-title").textContent = dto.title || "Untitled";
  $("#reader-loc").textContent = "";
  $("#reader-percent").textContent = "";
  view().hidden = false;
  view().classList.add("open");
  revealTopbar(); // shown now; fades out after the idle delay
  renderAnnotationsPanel();
  renderTocPanel();
  renderResumeControl();

  paginator = document.createElement("foliate-paginator");
  paginator.setAttribute("flow", "paginated");
  $("#reader-paginator-host").replaceChildren(paginator);

  paginator.addEventListener("create-overlayer", ({ detail: { doc, attach } }) => {
    const overlayer = new Overlayer();
    attach(overlayer);
    overlays.push({ doc, overlayer });
    paintAnnotations(doc, overlayer);
    // If a search is active when a new section first paints, paint its matches
    // into this section too — otherwise scrolling/navigating into the section
    // would leave the matches there invisible until you re-queried.
    if (searchResults.length) paintSearchMatches(doc, overlayer);
    doc.addEventListener("click", (e) => onDocClick(e, doc));
    // Text selected in the section → offer the highlight/note toolbar.
    doc.addEventListener("mouseup", () => onSelection(doc));
    // The paginator focuses the section iframe after navigating (`focusView`),
    // so arrow/space keydowns land in the iframe document, not the parent — the
    // parent-document listener alone would go deaf until you click out (the bug
    // where arrows stop turning pages). Listen on each section's doc too.
    doc.addEventListener("keydown", onKey, true);
    // Kill the native context menu inside the section iframe — its only items
    // are the useless "Open Frame in New Window" and a Reload that boots you
    // back to the library. Book content isn't editable and selection is handled
    // by our own toolbar (mouseup above), so suppress it unconditionally. The
    // parent-document suppressor in library.js can't reach here: contextmenu
    // events don't bubble out of an iframe to the host document.
    doc.addEventListener("contextmenu", (e) => e.preventDefault());
  });
  paginator.addEventListener("relocate", ({ detail }) => {
    updateProgress(detail);
    markTocActive(detail.index);
    updateBookmarkButton(); // after updateProgress sets the page's top eid
    hideNotePopover();
    hideSelectionToolbar();
    // Switch to/from full-bleed layout for image-only pages. A mode change
    // re-paginates and fires a fresh relocate with the new geometry, which
    // updates progress again — so this runs last.
    applyLayout(detail.index);
  });

  applyStyle(); // seed #styles + layout attrs before the first section loads
  syncStylePanel();
  paginator.open(book);
  // Auto-restore Sidle's OWN last position (never the device's — that's a manual
  // Resume target). Falls back to the start when nothing's saved or the saved
  // eid no longer resolves (e.g. the book was re-converted).
  if (!(await goToEid(sidleResume?.eid))) await paginator.goTo({ index: 0 });
  paginator.focus?.();
  keyHandler = onKey;
  document.addEventListener("keydown", keyHandler, true);
}

async function close() {
  // PDF mode has its own teardown (page index as the saved position); do it and
  // skip the reflowable path entirely.
  if (readerMode === "pdf") {
    await closePdf();
    return;
  }
  // The notebook mode has its own (lighter) teardown — no reflowable state.
  if (readerMode === "notebook") {
    closeNotebook();
    return;
  }
  // Persist Sidle's own last position — ONLY here, so the Resume target stays
  // frozen at where this session opened until you leave the book. Best-effort:
  // a failed write just means the next open falls back to the start.
  if (bookId != null && livePosition?.eid != null) {
    try {
      await window.api.invoke("reading_position_set", {
        bookId,
        eid: livePosition.eid,
        offset: livePosition.offset ?? 0,
        linearPos: livePosition.linear_pos ?? null,
      });
    } catch {
      /* ignore — position save is best-effort */
    }
  }
  if (keyHandler) {
    document.removeEventListener("keydown", keyHandler, true);
    keyHandler = null;
  }
  if (paginator) {
    $("#reader-paginator-host")?.replaceChildren();
    paginator = null;
  }
  if (book) {
    book.destroy();
    book = null;
  }
  dto = null;
  bookId = null;
  annotations = [];
  overlays = [];
  eidToSection = null;
  tocEntries = [];
  locByEid = null;
  maxLoc = 0;
  positionedByDoc = new WeakMap();
  sectionChars = [];
  charsBefore = [];
  totalChars = 0;
  lastPos = null;
  livePosition = null;
  sidleResume = null;
  deviceResumes = [];
  searchResults = [];
  searchQuery = "";
  searchSeq++;
  clearTimeout(searchDebounceTimer);
  lastPaintedCount = 0;
  selectedSearchIndex = -1;
  hideSearchPanel();
  hideResumeMenu();
  if ($("#reader-resume")) $("#reader-resume").hidden = true;
  lastTurnTime = null;
  lastCharPos = null;
  readSpeed = 0;
  sampleCount = 0;
  styleSettings = null;
  imageSections = new Set();
  layoutMode = null;
  if ($("#reader-loc")) $("#reader-loc").textContent = "";
  if ($("#reader-percent")) $("#reader-percent").textContent = "";
  hideNotePopover();
  hideSelectionToolbar();
  hideAnnotationsPanel();
  hideTocPanel();
  hideStylePanel();
  // Stop the auto-hide timer and un-fade, so a stray timeout can't fire after
  // teardown and the next book opens with its bar shown.
  cancelTopbarHide();
  topbarHovered = false;
  topbarEl()?.classList.remove("is-hidden");
  const v = view();
  if (v) {
    v.classList.remove("open");
    v.hidden = true;
    // Drop the per-book tint so the next book opens on defaults until its own
    // settings apply (no flash of the previous book's theme).
    for (const p of [
      "color-scheme",
      "--reader-bg",
      "--reader-fg",
      "--reader-gutter",
      "--reader-border",
      "--reader-muted",
      "--overlayer-highlight-blend-mode",
    ]) {
      v.style.removeProperty(p);
    }
  }
}

function wire() {
  $("#reader-close")?.addEventListener("click", () => close());
  // Top bar auto-hide: hovering the top edge brings it back (if faded) and pauses
  // the fade; leaving re-arms it. A click on the dormant (faded) bar also brings
  // it back — the capture-phase click + stopPropagation keeps that revealing click
  // (or a tap, where there's no hover) from also firing the invisible button under
  // the cursor, so it can't accidentally hit the ← close button and drop you out.
  const topbar = topbarEl();
  if (topbar) {
    topbar.addEventListener("mouseenter", () => {
      topbarHovered = true;
      topbarEl()?.classList.remove("is-hidden"); // un-hide on hover, not just click
      cancelTopbarHide(); // no countdown while the pointer rests on the bar
    });
    topbar.addEventListener("mouseleave", () => {
      topbarHovered = false;
      scheduleTopbarHide();
    });
    topbar.addEventListener(
      "click",
      (e) => {
        if (topbar.classList.contains("is-hidden")) {
          e.stopPropagation();
          e.preventDefault();
          revealTopbar();
        }
      },
      true,
    );
  }
  // Page-turn margins: the left margin goes to the physically-left page (= next
  // in a vertical-rl / RTL book, prev otherwise), mirroring the arrow keys.
  $("#reader-nav-left")?.addEventListener("click", () => (readerPpd() === "rtl" ? forward() : back()));
  $("#reader-nav-right")?.addEventListener("click", () => (readerPpd() === "rtl" ? back() : forward()));
  $("#reader-statusbar")?.addEventListener("click", () => cycleProgressMode());
  // The notebook's "Page X" readout is a go-to-page trigger — its own click region,
  // so it doesn't also cycle the bar; other modes let the click bubble to cycle.
  $("#reader-loc")?.addEventListener("click", (e) => {
    if (readerMode !== "notebook" || !nbk?.pageCount) return;
    e.stopPropagation();
    openNotebookGoTo();
  });
  // Resume is its own tap region: open the chooser, and stop the click from
  // bubbling to the status bar (which would also cycle the progress display).
  $("#reader-resume")?.addEventListener("click", (e) => {
    e.stopPropagation();
    toggleResumeMenu();
  });
  $("#reader-annotations")?.addEventListener("click", () => toggleAnnotationsPanel());
  $("#reader-annotations-close")?.addEventListener("click", () => hideAnnotationsPanel());
  // Native annotations: page-bookmark toggle, the selection toolbar (color
  // swatches create a highlight; Note creates one + opens the editor), and the
  // editable note popover's Save/Delete.
  $("#reader-bookmark")?.addEventListener("click", () => toggleBookmark());
  renderColorSwatches($("#rst-colors"), null, (name) => createAnnotation(name, false));
  $("#rst-note")?.addEventListener("click", () => createAnnotation(DEFAULT_HL_COLOR, true));
  $("#reader-note-save")?.addEventListener("click", () => saveEditor());
  $("#reader-note-delete")?.addEventListener("click", () => deleteEditor());
  $("#reader-search")?.addEventListener("click", () => toggleSearchPanel());
  $("#reader-search-close")?.addEventListener("click", () => hideSearchPanel());
  const searchInput = $("#reader-search-input");
  if (searchInput) {
    // IME-safe: a composing JP IME fires `input` for every romaji keystroke; we
    // defer until the user commits (`compositionend`). Outside composition,
    // debounced `input` runs the live search.
    searchInput.addEventListener("compositionstart", () => (searchComposing = true));
    searchInput.addEventListener("compositionend", (e) => {
      searchComposing = false;
      scheduleSearch(e.target.value);
    });
    searchInput.addEventListener("input", (e) => {
      if (searchComposing) return;
      scheduleSearch(e.target.value);
    });
  }
  $("#reader-toc")?.addEventListener("click", () => toggleTocPanel());
  $("#reader-toc-close")?.addEventListener("click", () => hideTocPanel());
  // PDF display settings (spread + night mode). Persist + apply on change.
  $("#rps-spread")?.addEventListener("change", (e) => setPdfSpread(e.target.value));
  $("#rps-cover")?.addEventListener("change", (e) => setPdfCover(e.target.checked));
  $("#rps-invert")?.addEventListener("change", (e) => setPdfInvert(e.target.checked));
  $("#rps-ink")?.addEventListener("change", (e) => setPdfInk(e.target.checked));
  $("#rps-zoom")?.addEventListener("input", (e) => setPdfZoom(parseFloat(e.target.value)));
  // Trackpad pinch-zoom for fixed-layout modes. macOS WebKit fires the proprietary
  // gesture* events with a cumulative `scale`; other engines surface a pinch as
  // ctrl+wheel. Both feed the shared zoom. preventDefault stops the webview's own
  // magnification / page-zoom; a plain (no-ctrl) wheel is left alone so a zoomed
  // page still scrolls.
  const fixedLayout = () => readerMode === "pdf" || readerMode === "notebook";
  const stageEl = $("#reader-stage");
  let pinchBase = 0; // zoom captured at gesturestart; >0 while a pinch is active
  let pinchAnchor = null; // pinch centre (client coords) to zoom toward
  if (stageEl) {
    stageEl.addEventListener("gesturestart", (e) => {
      if (!fixedLayout()) return;
      e.preventDefault();
      pinchBase = pdfStyle.zoom || 1;
      pinchAnchor = Number.isFinite(e.clientX) ? { x: e.clientX, y: e.clientY } : null;
    });
    stageEl.addEventListener("gesturechange", (e) => {
      if (!fixedLayout() || !pinchBase) return;
      e.preventDefault();
      if (Number.isFinite(e.clientX)) pinchAnchor = { x: e.clientX, y: e.clientY };
      setPdfZoom(pinchBase * e.scale, pinchAnchor);
    });
    stageEl.addEventListener("gestureend", (e) => {
      if (!fixedLayout()) return;
      e.preventDefault();
      pinchBase = 0;
    });
    stageEl.addEventListener(
      "wheel",
      (e) => {
        if (!fixedLayout() || !e.ctrlKey || pinchBase) return; // gesture* owns it if active
        e.preventDefault();
        setPdfZoom((pdfStyle.zoom || 1) * (1 - e.deltaY / 100), { x: e.clientX, y: e.clientY });
      },
      { passive: false },
    );
  }
  // Display-settings popover: the Aa button toggles it; every control writes
  // through to the live view; reset restores defaults.
  $("#reader-style")?.addEventListener("click", () => toggleStylePanel());
  $("#rs-reset")?.addEventListener("click", () => resetStyle());
  $("#reader-style-panel")
    ?.querySelectorAll("select, input")
    .forEach((el) => {
      // `input` for live drag of the slider/color; `change` as a fallback for
      // WebKit color inputs that only commit on close. Idempotent, so both is fine.
      el.addEventListener("input", onStyleInput);
      el.addEventListener("change", onStyleInput);
    });
  // Vertical 2-col mode keys off the paginator's live aspect ratio + height, so
  // we re-apply layout on window resize. rAF coalesces drag-resize bursts into
  // one apply per frame.
  let resizeTick = null;
  window.addEventListener("resize", () => {
    if (resizeTick) cancelAnimationFrame(resizeTick);
    resizeTick = requestAnimationFrame(() => {
      resizeTick = null;
      if (paginator && styleSettings) applyLayout(lastPos?.index ?? 0, true);
      // Fixed-layout modes: re-render the current spread (auto single/double at
      // the new size). Shares this rAF so drag-resize coalesces to one apply.
      if (readerMode === "pdf" && pdf) pdfRenderCurrent();
      else if (readerMode === "notebook" && nbk) nbkShowPage(nbk.page);
    });
  });
  // Click anywhere in the app chrome (outside a popover) dismisses it. Clicks
  // inside the section iframe live in a separate document and don't reach here,
  // so this never fights the in-text click that opened the note popover.
  document.addEventListener("mousedown", (e) => {
    const pop = $("#reader-note-popover");
    if (pop && !pop.hidden && !pop.contains(e.target)) hideNotePopover();
    // Dismiss the selection toolbar when clicking the app chrome outside it.
    // (In-iframe clicks don't bubble here; those settle via the mouseup handler.)
    const selBar = $("#reader-selection-toolbar");
    if (selBar && !selBar.hidden && !selBar.contains(e.target)) hideSelectionToolbar();
    const resumeMenu = $("#reader-resume-menu");
    const resumeBtn = $("#reader-resume");
    if (
      resumeMenu &&
      !resumeMenu.hidden &&
      !resumeMenu.contains(e.target) &&
      !resumeBtn?.contains(e.target)
    ) {
      hideResumeMenu();
    }
    const stylePanel = $("#reader-style-panel");
    const styleBtn = $("#reader-style");
    if (
      stylePanel &&
      !stylePanel.hidden &&
      !stylePanel.contains(e.target) &&
      !styleBtn?.contains(e.target)
    ) {
      hideStylePanel();
    }
    // The shared fixed-layout (PDF / notebook) panel dismisses on outside-click too.
    const pdfStylePanel = $("#reader-pdf-style-panel");
    if (
      pdfStylePanel &&
      !pdfStylePanel.hidden &&
      !pdfStylePanel.contains(e.target) &&
      !styleBtn?.contains(e.target)
    ) {
      pdfStylePanel.hidden = true;
    }
  });
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", wire);
} else {
  wire();
}

window.sidleReader = { open, close, openNotebook, isNotebookOpen, reloadAnnotations };
