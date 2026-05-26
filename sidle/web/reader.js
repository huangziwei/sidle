// reader.js — the built-in reader coordinator. Replaces foliate's view.js with
// a thin layer over the vendored paginator: open a library book (KFX→DOM via
// the `reader_open` Tauri command), paginate it, and surface imported
// annotations — highlights painted in place, notes with a note cue + popover,
// bookmarks as margin markers and a jump-list. Exposed as `window.sidleReader`
// so the (classic-script) library.js can drive it across the module boundary.

import "./foliate-kfx/paginator.js"; // defines <foliate-paginator>
import { Overlayer } from "./foliate-kfx/overlayer.js";
import { makeKfxBook } from "./foliate-kfx/kfx-book.js";
import { rangeFor, textBoundary } from "./foliate-kfx/anchor.js";

const $ = (sel) => document.querySelector(sel);
const toast = (msg, isError) => window.showToast?.(msg, isError);

let book = null; // current kfx-book
let dto = null; // raw reader_open DTO (kept for the eid→section index)
let bookId = null; // library id of the open book (for reload-on-sync)
let paginator = null; // <foliate-paginator>
let annotations = []; // AnnotationDto[] for the open book
let overlays = []; // [{ doc, overlayer }] — one per loaded section, for repaint
let eidToSection = null; // Map<eid, sectionIndex>, built lazily for jumps
let keyHandler = null;
let tocEntries = []; // flat [{ li, sectionIndex }] in TOC order, for active-marking

const view = () => $("#reader-view");

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

// Paint every resolvable annotation into one section's overlayer. Idempotent:
// `overlayer.add` replaces an existing key, so re-running after a sync just
// refreshes (device import is add-only, so nothing needs removing).
function paintAnnotations(doc, overlayer) {
  const vertical = (book?.writingMode || "").startsWith("vertical");
  for (const ann of annotations) {
    if (ann.kind === "bookmark") {
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

// ---- note popover ----------------------------------------------------------

// The overlay SVG is pointer-transparent, so clicks land on the iframe doc.
// Find a note whose live rects contain the click and pop its body.
function noteAt(doc, x, y) {
  for (const ann of annotations) {
    if (ann.kind !== "note" || !ann.note_body) continue;
    for (const r of annotationRects(doc, ann)) {
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return ann;
    }
  }
  return null;
}

function onDocClick(e, doc) {
  const ann = noteAt(doc, e.clientX, e.clientY);
  if (!ann) {
    hideNotePopover();
    return;
  }
  showNotePopover(ann, doc, e.clientX, e.clientY);
}

function showNotePopover(ann, doc, clientX, clientY) {
  const pop = $("#reader-note-popover");
  if (!pop) return;
  $("#reader-note-quote").textContent = ann.text || "";
  $("#reader-note-body").textContent = ann.note_body || "";
  // The click is in the iframe's own viewport; offset by the iframe's box to
  // land in the main document's coordinate space.
  const fr = doc.defaultView?.frameElement?.getBoundingClientRect() || { left: 0, top: 0 };
  pop.hidden = false; // unhide first so offsetWidth/Height are real
  let left = fr.left + clientX;
  let top = fr.top + clientY + 14;
  left = Math.max(8, Math.min(left, window.innerWidth - pop.offsetWidth - 8));
  if (top + pop.offsetHeight > window.innerHeight - 8) {
    top = fr.top + clientY - pop.offsetHeight - 14;
  }
  pop.style.left = `${left}px`;
  pop.style.top = `${Math.max(8, top)}px`;
}

function hideNotePopover() {
  const p = $("#reader-note-popover");
  if (p) p.hidden = true;
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
  if (ann.eid_start == null || !paginator) return;
  if (!eidToSection) eidToSection = buildEidIndex(dto);
  const index = eidToSection.get(ann.eid_start);
  if (index == null) {
    toast("Couldn't locate that annotation in the book");
    return;
  }
  hideNotePopover();
  await paginator.goTo({ index, anchor: (d) => rangeFor(d, ann) || 0 });
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
  if (p) p.hidden = !p.hidden;
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
  // Clear overlays for annotations the sync removed (overlayer.add only adds /
  // replaces by key, so a vanished annotation would otherwise linger painted).
  const live = new Set(annotations.map((a) => a.id));
  const gone = prevIds.filter((id) => !live.has(id));
  for (const { overlayer } of overlays) {
    for (const id of gone) overlayer.remove(`ann-${id}`);
  }
  for (const { doc, overlayer } of overlays) paintAnnotations(doc, overlayer);
  renderAnnotationsPanel();
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

// ---- navigation -----------------------------------------------------------

const forward = () => {
  hideNotePopover();
  paginator?.next();
};
const back = () => {
  hideNotePopover();
  paginator?.prev();
};

function onKey(e) {
  if (e.key === "Escape") {
    // Peel back overlays first, then close the reader.
    if (!$("#reader-note-popover")?.hidden) hideNotePopover();
    else if (!$("#reader-annotations-panel")?.hidden) hideAnnotationsPanel();
    else if (!$("#reader-toc-panel")?.hidden) hideTocPanel();
    else close();
    e.preventDefault();
    return;
  }
  // Don't hijack modified combos — shift+arrow extends a text selection in the
  // section iframe, ⌘/ctrl/alt are shortcuts. Let those through.
  if (e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return;
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
    default:
      handled = false;
  }
  if (handled) e.preventDefault();
}

// ---- open / close ---------------------------------------------------------

async function open(id) {
  await close(); // tear down any prior session
  let openDto, anns;
  try {
    [openDto, anns] = await Promise.all([
      window.api.invoke("reader_open", { bookId: id }),
      window.api.invoke("annotations_for_book", { bookId: id }),
    ]);
  } catch (err) {
    toast(`Couldn't open reader: ${err}`, true);
    return;
  }
  bookId = id;
  dto = openDto;
  annotations = anns || [];
  eidToSection = null;
  overlays = [];
  book = makeKfxBook(dto);

  $("#reader-title").textContent = dto.title || "Untitled";
  $("#reader-progress").textContent = "";
  view().hidden = false;
  view().classList.add("open");
  renderAnnotationsPanel();
  renderTocPanel();

  paginator = document.createElement("foliate-paginator");
  paginator.setAttribute("flow", "paginated");
  $("#reader-stage").replaceChildren(paginator);

  paginator.addEventListener("create-overlayer", ({ detail: { doc, attach } }) => {
    const overlayer = new Overlayer();
    attach(overlayer);
    overlays.push({ doc, overlayer });
    paintAnnotations(doc, overlayer);
    doc.addEventListener("click", (e) => onDocClick(e, doc));
    // The paginator focuses the section iframe after navigating (`focusView`),
    // so arrow/space keydowns land in the iframe document, not the parent — the
    // parent-document listener alone would go deaf until you click out (the bug
    // where arrows stop turning pages). Listen on each section's doc too.
    doc.addEventListener("keydown", onKey, true);
  });
  paginator.addEventListener("relocate", ({ detail }) => {
    const pct = Math.round((detail.fraction ?? 0) * 100);
    $("#reader-progress").textContent = `${pct}%`;
    markTocActive(detail.index);
    hideNotePopover();
  });

  paginator.open(book);
  await paginator.goTo({ index: 0 }); // TODO(T2): restore saved reading position
  paginator.focus?.();
  keyHandler = onKey;
  document.addEventListener("keydown", keyHandler, true);
}

async function close() {
  if (keyHandler) {
    document.removeEventListener("keydown", keyHandler, true);
    keyHandler = null;
  }
  if (paginator) {
    $("#reader-stage")?.replaceChildren();
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
  hideNotePopover();
  hideAnnotationsPanel();
  hideTocPanel();
  const v = view();
  if (v) {
    v.classList.remove("open");
    v.hidden = true;
  }
}

function wire() {
  $("#reader-close")?.addEventListener("click", () => close());
  $("#reader-prev")?.addEventListener("click", () => back());
  $("#reader-next")?.addEventListener("click", () => forward());
  $("#reader-annotations")?.addEventListener("click", () => toggleAnnotationsPanel());
  $("#reader-annotations-close")?.addEventListener("click", () => hideAnnotationsPanel());
  $("#reader-toc")?.addEventListener("click", () => toggleTocPanel());
  $("#reader-toc-close")?.addEventListener("click", () => hideTocPanel());
  // Click anywhere in the app chrome (outside the popover) dismisses it. Clicks
  // inside the section iframe live in a separate document and don't reach here,
  // so this never fights the in-text click that opened the popover.
  document.addEventListener("mousedown", (e) => {
    const pop = $("#reader-note-popover");
    if (pop && !pop.hidden && !pop.contains(e.target)) hideNotePopover();
  });
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", wire);
} else {
  wire();
}

window.sidleReader = { open, close, reloadAnnotations };
