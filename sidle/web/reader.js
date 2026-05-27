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

function readSavedProgressMode() {
  try {
    const n = Number(localStorage.getItem("sidle.reader.progressMode"));
    return Number.isInteger(n) && n >= 0 && n <= 3 ? n : 0;
  } catch {
    return 0;
  }
}

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

// ---- resume: jump to a saved position (Sidle's own / the Kindle's) ----------

// Navigate to an eid's element — resolve the section via the eid index, anchor
// on the `[data-eid]` element (same path as the annotation jump). Returns false
// (a no-op) when the eid isn't in the book, e.g. after a re-convert.
async function goToEid(eid) {
  if (eid == null || !paginator) return false;
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
  progressMode = (progressMode + 1) % 4;
  try {
    localStorage.setItem("sidle.reader.progressMode", String(progressMode));
  } catch {
    /* ignore storage errors */
  }
  renderProgress();
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
      // Vary the block (top/bottom) margin. The measure (column height) is left
      // UNCAPPED so the content fills the page height and the `margin` attribute
      // is the *actual* top/bottom margin — not just a floor. With the default
      // 720 cap, a tall/fullscreen window would cap the content and center it,
      // splitting the leftover as fixed margins that ignore the setting.
      paginator.setAttribute("margin", `${VMARGIN[styleSettings.margin] ?? VMARGIN.normal}px`);
      paginator.setAttribute("max-inline-size", `${HUGE_MEASURE}px`);
      paginator.setAttribute("max-column-count", "2");
    } else {
      // A single column narrower than the window; the leftover is the L/R margin.
      paginator.setAttribute("margin", "48px");
      paginator.setAttribute("max-inline-size", `${HMEASURE[styleSettings.margin] ?? HMEASURE.normal}px`);
      paginator.setAttribute("max-column-count", "1");
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
  const p = $("#reader-style-panel");
  if (p) p.hidden = !p.hidden;
}
function hideStylePanel() {
  const p = $("#reader-style-panel");
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
    if (!$("#reader-resume-menu")?.hidden) hideResumeMenu();
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
    // The paginator focuses the section iframe after navigating (`focusView`),
    // so arrow/space keydowns land in the iframe document, not the parent — the
    // parent-document listener alone would go deaf until you click out (the bug
    // where arrows stop turning pages). Listen on each section's doc too.
    doc.addEventListener("keydown", onKey, true);
  });
  paginator.addEventListener("relocate", ({ detail }) => {
    updateProgress(detail);
    markTocActive(detail.index);
    hideNotePopover();
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
  hideAnnotationsPanel();
  hideTocPanel();
  hideStylePanel();
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
  // Page-turn margins: the left margin goes to the physically-left page (= next
  // in a vertical-rl / RTL book, prev otherwise), mirroring the arrow keys.
  $("#reader-nav-left")?.addEventListener("click", () => (book?.ppd === "rtl" ? forward() : back()));
  $("#reader-nav-right")?.addEventListener("click", () => (book?.ppd === "rtl" ? back() : forward()));
  $("#reader-statusbar")?.addEventListener("click", () => cycleProgressMode());
  // Resume is its own tap region: open the chooser, and stop the click from
  // bubbling to the status bar (which would also cycle the progress display).
  $("#reader-resume")?.addEventListener("click", (e) => {
    e.stopPropagation();
    toggleResumeMenu();
  });
  $("#reader-annotations")?.addEventListener("click", () => toggleAnnotationsPanel());
  $("#reader-annotations-close")?.addEventListener("click", () => hideAnnotationsPanel());
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
  // Click anywhere in the app chrome (outside a popover) dismisses it. Clicks
  // inside the section iframe live in a separate document and don't reach here,
  // so this never fights the in-text click that opened the note popover.
  document.addEventListener("mousedown", (e) => {
    const pop = $("#reader-note-popover");
    if (pop && !pop.hidden && !pop.contains(e.target)) hideNotePopover();
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
  });
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", wire);
} else {
  wire();
}

window.sidleReader = { open, close, reloadAnnotations };
