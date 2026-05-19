// Library view: gallery + list, drag-drop, right-click menu, live status.

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));

const state = {
  books: [],
  view: "gallery", // 'gallery' | 'list'
  sort: { key: "imported_at", asc: false },
  // Facet filters: AND across facets, OR within. Each Set holds the
  // currently-selected values for that facet. See extractFacetValues for
  // how values are derived per book.
  filters: {
    language: new Set(),
    author: new Set(),
    on_kindle: new Set(), // "yes" | "no"
    publisher: new Set(),
    series: new Set(),
    tags: new Set(),
  },
  search: "", // global free-text search across title, author, series, tags
  device: null, // DeviceInfo | null
  sent: [],     // Vec<DeviceBookRow>
  sentSet: new Set(), // sha256s currently on device, derived from `sent`
  columnWidths: {}, // { title: 280, ... } persisted px widths
  // Ordered array of { key, visible } — drives the list view's column
  // order and show/hide state. Built from localStorage in loadPreferences;
  // defaults to defaultColumnConfig() on first install.
  columnConfig: [],
  selected: new Set(), // book ids currently selected
  lastClicked: null,   // last single-clicked book id, anchor for shift-range
  // Bumped every time a cover is overwritten (worker tail-step fetch, manual
  // recrawl, conversion completion). Appended as `?v=N` to each cover URL so
  // the browser doesn't keep serving the stale grayscale image from cache
  // after we've swapped the file on disk.
  coverCacheBust: 0,
  // When non-null, an autopull from /dedrm is in progress. `{ done, total }`
  // — surfaced in the status bar and used by renderQueue to know it shouldn't
  // clobber the autopull line with the queue summary.
  autopull: null,
};

// Sort keys exposed in the gallery-visible sort popover and as
// data-sort attrs on the list-view column headers. Order here is the
// order shown in the popover. Series will be added in Phase 5.
const SORT_KEYS = [
  ["title", "Title"],
  ["author", "Author"],
  ["series", "Series"],
  ["publisher", "Publisher"],
  ["language", "Language"],
  ["imported_at", "Date added"],
  ["file_size", "Size"],
  ["on_kindle", "On Kindle"],
];

const FACETS = ["language", "author", "on_kindle", "publisher", "series", "tags"];

// All columns the list view knows about, keyed by column id. The label is
// what shows in the header; sortable=false skips data-sort wiring (Tags is
// multi-value, Formats is rendered widgets — neither sorts cleanly).
const COLUMN_DEFS = {
  title:        { label: "Title",      sortable: true  },
  author:       { label: "Author",     sortable: true  },
  series:       { label: "Series",     sortable: true  },
  publisher:    { label: "Publisher",  sortable: true  },
  published_at: { label: "Published",  sortable: true  },
  language:     { label: "Lang",       sortable: true  },
  tags:         { label: "Tags",       sortable: false },
  imported_at:  { label: "Date added", sortable: true  },
  file_size:    { label: "Size",       sortable: true  },
  formats:      { label: "Formats",    sortable: false },
  on_kindle:    { label: "On Kindle",  sortable: true  },
};

// First-install ordering. After load, state.columnConfig is what governs
// the rendered order; this only seeds it when there's nothing in
// localStorage, plus serves as the "where to append new columns added in
// future versions" anchor for mergeColumnConfig.
const DEFAULT_COLUMN_ORDER = [
  "title",
  "author",
  "series",
  "publisher",
  "published_at",
  "language",
  "tags",
  "imported_at",
  "file_size",
  "formats",
  "on_kindle",
];

function defaultColumnConfig() {
  return DEFAULT_COLUMN_ORDER.map((key) => ({ key, visible: true }));
}

// Merge persisted config with the current column set. Drops unknown keys
// (e.g. a column removed in a later version) and appends any newly-added
// columns at the end with visible:true — so a fresh feature column shows
// up automatically without nuking the user's order.
function mergeColumnConfig(stored) {
  const known = new Set(Object.keys(COLUMN_DEFS));
  const valid = (stored || [])
    .filter((c) => c && known.has(c.key))
    .map((c) => ({ key: c.key, visible: c.visible !== false }));
  const present = new Set(valid.map((c) => c.key));
  for (const key of DEFAULT_COLUMN_ORDER) {
    if (!present.has(key)) valid.push({ key, visible: true });
  }
  return valid;
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

document.addEventListener("DOMContentLoaded", async () => {
  await loadPreferences();
  wireToolbar();
  wireDragDrop();
  wireContextMenu();
  wireQueueDrawer();
  wireDevice();
  // Header interactions (sort, resize, drag-to-reorder, visibility menu)
  // are wired by renderList() itself since the thead is rebuilt on every
  // render — no separate boot call needed.
  wireSelection();
  wireFilterBar();
  wireSortPopover();
  wireMetadataModal();
  await refresh();
  subscribeStatus();
  subscribeDeviceStatus();
  subscribeSendProgress();
  subscribePullProgress();
  subscribeLibraryRowUpdated();
});

async function loadPreferences() {
  const view = localStorage.getItem("view");
  if (view === "list") state.view = "list";
  const sort = localStorage.getItem("sort");
  if (sort) {
    try {
      state.sort = { ...state.sort, ...JSON.parse(sort) };
    } catch {}
  }
  const cols = localStorage.getItem("columnWidths");
  if (cols) {
    try {
      state.columnWidths = JSON.parse(cols) || {};
    } catch {}
  }
  const colCfg = localStorage.getItem("columnConfig");
  if (colCfg) {
    try {
      state.columnConfig = mergeColumnConfig(JSON.parse(colCfg));
    } catch {
      state.columnConfig = defaultColumnConfig();
    }
  } else {
    state.columnConfig = defaultColumnConfig();
  }
  const filters = localStorage.getItem("filters");
  if (filters) {
    try {
      const parsed = JSON.parse(filters) || {};
      for (const facet of FACETS) {
        if (Array.isArray(parsed[facet])) {
          state.filters[facet] = new Set(parsed[facet]);
        }
      }
    } catch {}
  }
  const search = localStorage.getItem("search");
  if (typeof search === "string") state.search = search;
  applyView();
}

function persistPreferences() {
  localStorage.setItem("view", state.view);
  localStorage.setItem("sort", JSON.stringify(state.sort));
  const filtersForStorage = {};
  for (const facet of FACETS) {
    filtersForStorage[facet] = [...state.filters[facet]];
  }
  localStorage.setItem("filters", JSON.stringify(filtersForStorage));
  localStorage.setItem("search", state.search);
  localStorage.setItem("columnConfig", JSON.stringify(state.columnConfig));
}

function saveColumnWidths() {
  localStorage.setItem("columnWidths", JSON.stringify(state.columnWidths));
}

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

function wireToolbar() {
  $("#btn-add").addEventListener("click", onAddClick);
  $("#view-gallery").addEventListener("click", () => setView("gallery"));
  $("#view-list").addEventListener("click", () => setView("list"));
}

async function onAddClick() {
  const paths = await window.api.openFileDialog();
  if (!paths || paths.length === 0) return;
  await importPaths(paths);
}

function setView(v) {
  state.view = v;
  applyView();
  persistPreferences();
  if (v === "list") {
    requestAnimationFrame(() => {
      ensureDefaultColumnWidths();
      applyColumnWidths();
    });
  }
}

function applyView() {
  $("#view-gallery").classList.toggle("active", state.view === "gallery");
  $("#view-list").classList.toggle("active", state.view === "list");
  $("#view-gallery").setAttribute("aria-selected", String(state.view === "gallery"));
  $("#view-list").setAttribute("aria-selected", String(state.view === "list"));
  $("#gallery").classList.toggle("active", state.view === "gallery");
  $("#list").classList.toggle("active", state.view === "list");
  $("#gallery").hidden = state.view !== "gallery";
  $("#list").hidden = state.view !== "list";
}

// ---------------------------------------------------------------------------
// Drag and drop
// ---------------------------------------------------------------------------

function wireDragDrop() {
  const veil = $("#drop-veil");
  window.api.onDragDrop((event) => {
    const t = event.payload?.type;
    if (t === "enter" || t === "over") {
      veil.hidden = false;
    } else if (t === "leave") {
      veil.hidden = true;
    } else if (t === "drop") {
      veil.hidden = true;
      const paths = event.payload.paths || [];
      const accepted = paths.filter((p) => {
        const lower = p.toLowerCase();
        return (
          lower.endsWith(".epub") ||
          lower.endsWith(".kfx") ||
          lower.endsWith(".kfx-zip")
        );
      });
      if (accepted.length === 0) {
        showToast("only .epub, .kfx, .kfx-zip are supported", true);
        return;
      }
      importPaths(accepted);
    }
  });
}

async function importPaths(paths) {
  let imported = 0;
  let dupes = 0;
  let failed = 0;
  try {
    const results = await window.api.invoke("library_import", { paths });
    for (const r of results) {
      if (r.kind === "imported") imported++;
      else if (r.kind === "duplicate") dupes++;
      else if (r.kind === "failed") {
        failed++;
        console.error("import failed:", r.path, r.error);
      }
    }
  } catch (e) {
    showToast(`import error: ${e}`, true);
    return;
  }

  await refresh();

  const parts = [];
  if (imported) parts.push(`${imported} imported`);
  if (dupes) parts.push(`${dupes} already in library`);
  if (failed) parts.push(`${failed} failed`);
  if (parts.length) showToast(parts.join(" · "), failed > 0);
}

// ---------------------------------------------------------------------------
// Loading + rendering
// ---------------------------------------------------------------------------

async function refresh() {
  try {
    state.books = await window.api.invoke("library_list");
  } catch (e) {
    showToast(`failed to load library: ${e}`, true);
    state.books = [];
  }
  if (state.device) {
    try {
      const rows = await window.api.invoke("device_list_ours");
      setSent(rows);
    } catch {
      setSent([]);
    }
  }
  render();
}

function setSent(rows) {
  state.sent = rows || [];
  state.sentSet = new Set(state.sent.map((r) => r.sha256));
}

function render() {
  // Prune selection of any books that no longer exist (e.g. after a refresh).
  if (state.selected.size > 0) {
    const live = new Set(state.books.map((b) => b.id));
    for (const id of [...state.selected]) {
      if (!live.has(id)) state.selected.delete(id);
    }
    if (state.lastClicked != null && !live.has(state.lastClicked)) {
      state.lastClicked = null;
    }
  }
  const books = sortedBooks(visibleBooks(state.books));
  renderGallery(books);
  renderList(books);
  renderQueue();
  renderSelectionBar();
  updateSendUnsentButton();
  // The empty-state messages are wired to whether the *visible* set is
  // empty. If the underlying library is non-empty but filters hide
  // everything, the empty state surfaces in the same slot — the user
  // can clear filters via the "All" pill.
  $("#gallery-empty").hidden = books.length > 0;
  $("#list-empty").hidden = books.length > 0;
  renderFilterBar();
  renderSortControl();
  if (state.view === "list") {
    requestAnimationFrame(() => {
      ensureDefaultColumnWidths();
      applyColumnWidths();
    });
  }
}

function sortedBooks(books) {
  const list = books || state.books;
  const { key, asc } = state.sort;
  const dir = asc ? 1 : -1;
  return [...list].sort((a, b) => {
    const av = sortValue(a, key);
    const bv = sortValue(b, key);
    if (av == null && bv == null) return 0;
    if (av == null) return 1;
    if (bv == null) return -1;
    if (typeof av === "number" && typeof bv === "number") return (av - bv) * dir;
    return String(av).localeCompare(String(bv)) * dir;
  });
}

function sortValue(b, key) {
  if (key === "on_kindle") return state.sentSet.has(b.sha256) ? 1 : 0;
  if (key === "series") return seriesSortKey(b);
  return b[key];
}

// Composite sort key for the Series column: primary by series_name,
// secondary by series_index. We pack both into a single string with a
// control-char separator ( sorts before every printable char) and
// a zero-padded index, so the existing localeCompare path in
// sortedBooks() handles the two-level ordering without any tuple
// machinery. Books without a series return null and sink to the bottom
// via the existing null-handling.
function seriesSortKey(b) {
  const name = b.series_name?.trim();
  if (!name) return null;
  // *10 so half-numbered series (1.5, 2.5) sort correctly; pad to 8
  // digits so even an unset index (99_999_999) compares cleanly.
  const rawIdx =
    b.series_index != null && Number.isFinite(b.series_index)
      ? Math.round(b.series_index * 10)
      : 99_999_999;
  return `${name}${String(rawIdx).padStart(8, "0")}`;
}

// Display string for the Series cell: "<Name> #<index>" or just "<Name>".
function seriesText(b) {
  const name = b.series_name?.trim();
  if (!name) return "";
  if (b.series_index != null && Number.isFinite(b.series_index)) {
    return `${name} #${b.series_index}`;
  }
  return name;
}

// ---------------------------------------------------------------------------
// Filter algorithm (cascading facets + free-text search)
//
// Composition: render() calls sortedBooks(visibleBooks(state.books)).
//   - visibleBooks applies the global search and AND-across-facets.
//   - facetOptions, used by the dropdown, applies a "leave-one-out" view
//     so selecting language=jp narrows the Author pill's options, but the
//     Language pill itself still shows every language in the library.
//
// CJK support: extractFacetValues for authors splits on /\s*[,、]\s*/ so a
// Japanese OPF that emits "村上春樹、夏目漱石" inside one <dc:creator> still
// yields two distinct authors in the facet. Everywhere else is just JS
// Unicode-native string ops (.includes, .toLowerCase no-op for CJK, etc.).
// ---------------------------------------------------------------------------

function extractFacetValues(book, facet) {
  switch (facet) {
    case "language":
      return [book.language?.trim() || "—"];
    case "author": {
      const trimmed = (book.author || "").trim();
      if (!trimmed) return ["—"];
      // ASCII comma OR CJK ideographic comma U+3001. Japanese EPUBs often
      // pack multiple creators into one <dc:creator> separated by 「、」.
      const parts = trimmed.split(/\s*[,、]\s*/).filter(Boolean);
      return parts.length ? [...new Set(parts)] : ["—"];
    }
    case "on_kindle":
      return [state.sentSet.has(book.sha256) ? "yes" : "no"];
    case "publisher":
      return [book.publisher?.trim() || "—"];
    case "series":
      return [book.series_name?.trim() || "—"];
    case "tags":
      return book.tags?.length ? book.tags : ["—"];
    default:
      return [];
  }
}

function activeFacetsExcept(skipFacet) {
  const out = {};
  for (const facet of FACETS) {
    const sel = state.filters[facet];
    if (facet === skipFacet || sel.size === 0) continue;
    out[facet] = sel;
  }
  return out;
}

function matchesFacets(book, facets) {
  for (const facet of Object.keys(facets)) {
    const vals = extractFacetValues(book, facet);
    if (!vals.some((v) => facets[facet].has(v))) return false;
  }
  return true;
}

function matchesSearch(book) {
  const q = state.search.trim().toLowerCase();
  if (!q) return true;
  const hay = [
    book.title,
    book.author,
    book.publisher,
    book.series_name,
    ...(book.tags || []),
  ]
    .filter(Boolean)
    .join(" ")
    .toLowerCase();
  return hay.includes(q);
}

function visibleBooks(books) {
  const facets = activeFacetsExcept(null);
  return books.filter((b) => matchesSearch(b) && matchesFacets(b, facets));
}

function facetOptions(facet) {
  const others = activeFacetsExcept(facet);
  const counts = new Map();
  for (const b of state.books) {
    if (!matchesSearch(b)) continue;
    if (!matchesFacets(b, others)) continue;
    for (const v of extractFacetValues(b, facet)) {
      counts.set(v, (counts.get(v) || 0) + 1);
    }
  }
  // Always include this pill's currently-selected values, even if the
  // cross-facet filter would exclude them. Without this, a user who
  // selected author=A then language=B (excluding A's books) would have
  // no way to unselect A.
  for (const v of state.filters[facet]) {
    if (!counts.has(v)) counts.set(v, 0);
  }
  return [...counts.entries()].sort((a, b) => {
    if (a[0] === "—") return 1;
    if (b[0] === "—") return -1;
    return a[0].localeCompare(b[0]);
  });
}

function hasAnyFilter() {
  if (state.search.trim()) return true;
  for (const facet of FACETS) {
    if (state.filters[facet].size > 0) return true;
  }
  return false;
}

function clearAllFilters() {
  for (const facet of FACETS) state.filters[facet] = new Set();
  state.search = "";
  $("#search-input").value = "";
  persistPreferences();
  render();
}

function toggleFilterValue(facet, value) {
  const sel = state.filters[facet];
  if (sel.has(value)) sel.delete(value);
  else sel.add(value);
  persistPreferences();
  render();
}

function clearFacet(facet) {
  state.filters[facet] = new Set();
  persistPreferences();
  render();
}

function renderGallery(books) {
  const grid = $("#gallery-grid");
  grid.innerHTML = "";
  for (const b of books) {
    grid.appendChild(galleryCard(b));
  }
}

function galleryCard(b) {
  const card = document.createElement("div");
  card.className = "book-card";
  if (state.selected.has(b.id)) card.classList.add("selected");
  card.dataset.bookId = b.id;
  card.title = `${b.title}\n${b.author}`;

  const coverUrl = coverUrlFor(b);
  const cover = document.createElement("div");
  cover.className = "cover";
  if (coverUrl) {
    cover.classList.add("has-image");
    const img = document.createElement("img");
    img.src = coverUrl;
    img.alt = "";
    img.loading = "lazy";
    cover.appendChild(img);
  } else {
    const ph = document.createElement("div");
    ph.className = "cover-placeholder";
    ph.textContent = b.title || "Untitled";
    cover.appendChild(ph);
  }
  card.appendChild(cover);

  const meta = document.createElement("div");
  meta.className = "meta";
  const t = document.createElement("div");
  t.className = "t";
  t.textContent = b.title || "Untitled";
  const a = document.createElement("div");
  a.className = "a";
  a.textContent = b.author || "Unknown author";
  meta.append(t, a, metaBadges(b));
  card.appendChild(meta);

  card.addEventListener("click", (e) => onItemClick(e, b));
  card.addEventListener("dblclick", () => openInFinder(b.id));
  card.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    onItemContext(e, b);
    openContextMenu(e.clientX, e.clientY, b);
  });
  return card;
}

function metaBadges(b) {
  const wrap = document.createElement("div");
  wrap.className = "meta-badges";

  // The side that the import wrote directly is always "done". The other
  // side (whichever direction the background job runs) carries the row's
  // status. `b.kind` tells us which side that is.
  wrap.appendChild(formatBadge("epub", b, /*compact=*/ true));
  wrap.appendChild(formatBadge("kfx", b, /*compact=*/ true));

  if (state.sentSet.has(b.sha256)) {
    const dot = document.createElement("span");
    dot.className = "meta-kindle-dot";
    dot.title = "On Kindle";
    wrap.appendChild(dot);
  }

  return wrap;
}

// Returns the conversion status as it applies to the given format side
// (`"epub"` or `"kfx"`). The format that the import wrote directly is
// always "done"; the format being produced by the queue follows b.status.
function formatStatusFor(format, b) {
  const producing = b.kind === "kfx_to_epub" ? "epub" : "kfx";
  return format === producing ? b.status : "done";
}

function formatLabel(format, status, compact) {
  const upper = format.toUpperCase();
  if (compact || status === "done") return upper;
  switch (status) {
    case "converting": return `${upper} · converting`;
    case "pending":    return `${upper} · queued`;
    case "error":      return `${upper} · failed`;
    default:           return upper;
  }
}

function formatTooltip(format, status, b) {
  const upper = format.toUpperCase();
  switch (status) {
    case "done":       return `${upper} ready`;
    case "converting": return `${upper} converting…`;
    case "pending":    return `${upper} queued`;
    case "error":      return `${upper} failed: ${b.error || ""}`;
    default:           return upper;
  }
}

function formatBadge(format, b, compact) {
  const status = formatStatusFor(format, b);
  const span = document.createElement("span");
  span.className = `fmt-badge ${format} ${status}`;
  span.textContent = formatLabel(format, status, compact);
  span.title = formatTooltip(format, status, b);
  if (status === "error") {
    span.addEventListener("click", (e) => {
      e.stopPropagation();
      retryConvert(b.id);
    });
    span.style.cursor = "pointer";
  }
  return span;
}

function renderList(books) {
  const visibleCols = state.columnConfig.filter((c) => c.visible);

  // Rebuild colgroup so the resize logic indexes correctly into it.
  const colGroup = $("#book-cols");
  colGroup.innerHTML = "";
  for (const col of visibleCols) {
    const c = document.createElement("col");
    c.dataset.col = col.key;
    colGroup.appendChild(c);
  }

  // Rebuild the header row.
  const head = $("#list-head");
  head.innerHTML = "";
  for (const col of visibleCols) {
    head.appendChild(buildHeaderCell(col.key));
  }

  // Body rows.
  const tbody = $("#list-body");
  tbody.innerHTML = "";
  for (const b of books) tbody.appendChild(listRow(b, visibleCols));

  // Sort indicator on the new header.
  $$("#list th[data-sort]").forEach((th) => {
    th.classList.toggle("sorted", th.dataset.sort === state.sort.key);
    th.classList.toggle("asc", state.sort.asc);
  });

  // Re-attach header interactions (the thead got recreated).
  wireListHeaders();
}

function listRow(b, visibleCols) {
  const tr = document.createElement("tr");
  if (state.selected.has(b.id)) tr.classList.add("selected");
  tr.dataset.bookId = b.id;

  for (const col of visibleCols) {
    tr.appendChild(buildBodyCell(col.key, b));
  }

  tr.addEventListener("click", (e) => onItemClick(e, b));
  tr.addEventListener("dblclick", () => openInFinder(b.id));
  tr.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    onItemContext(e, b);
    openContextMenu(e.clientX, e.clientY, b);
  });
  return tr;
}

function buildHeaderCell(key) {
  const def = COLUMN_DEFS[key];
  const th = document.createElement("th");
  th.dataset.col = key;
  if (def.sortable) th.dataset.sort = key;

  // .th-label is the drag handle. We do NOT use the HTML5 draggable=true
  // API because Tauri's webview (with dragDropEnabled=true, required for
  // file-drop import) intercepts native drags at the OS level, which
  // both blocks dragstart events in the page and triggers the file-drop
  // import overlay. Column-reorder is implemented with mousedown/move/up
  // in onLabelMouseDown — see wireListHeaders().
  const label = document.createElement("span");
  label.className = "th-label";
  label.textContent = def.label;
  th.appendChild(label);

  const resizer = document.createElement("span");
  resizer.className = "resizer";
  th.appendChild(resizer);

  return th;
}

function buildBodyCell(key, b) {
  switch (key) {
    case "title":        return cell(b.title || "Untitled");
    case "author":       return cell(b.author || "");
    case "series":       return cell(seriesText(b));
    case "publisher":    return cell(b.publisher || "");
    case "published_at": return cell(b.published_at || "");
    case "language":     return cell(b.language || "");
    case "tags":         return cell((b.tags || []).join(", "));
    case "imported_at":  return cell(formatDate(b.imported_at));
    case "file_size":    return cell(formatBytes(b.file_size));
    case "formats":      return formatsCell(b);
    case "on_kindle":    return onKindleCell(b);
    default:             return cell("");
  }
}

function formatsCell(b) {
  const td = document.createElement("td");
  const wrap = document.createElement("div");
  wrap.className = "formats";
  // Verbose badges in the list (`KFX · converting` etc.) — the format that
  // the queue is producing carries the row's status; the other side stays
  // "done". See `formatStatusFor` above.
  wrap.appendChild(formatBadge("epub", b, /*compact=*/ false));
  wrap.appendChild(formatBadge("kfx", b, /*compact=*/ false));
  td.appendChild(wrap);
  return td;
}

function onKindleCell(b) {
  const td = document.createElement("td");
  const span = document.createElement("span");
  if (state.sentSet.has(b.sha256)) {
    span.className = "on-kindle yes";
    span.textContent = "✓";
    td.title = "On Kindle";
  } else {
    span.className = "on-kindle no";
    span.textContent = "—";
  }
  td.appendChild(span);
  return td;
}

function cell(text) {
  const td = document.createElement("td");
  td.textContent = text;
  td.title = text;
  return td;
}

// Re-attaches every header interaction after renderList rebuilds the
// thead/colgroup: sort-click, resize, drag-to-reorder, and the
// right-click visibility menu. Old listeners die with the previous DOM,
// so there's no need to manually remove them.
function wireListHeaders() {
  // Sort on header click. Skip clicks that originated on the resizer
  // (mousedown there handles the resize; the click would still bubble).
  $$("#list th[data-sort]").forEach((th) => {
    th.addEventListener("click", (e) => {
      if (e.target.classList.contains("resizer")) return;
      if (e.target.classList.contains("th-label") && th.classList.contains("just-dragged")) {
        // Suppress the synthetic click that fires at the end of a drag.
        th.classList.remove("just-dragged");
        return;
      }
      const key = th.dataset.sort;
      if (state.sort.key === key) state.sort.asc = !state.sort.asc;
      else state.sort = { key, asc: true };
      persistPreferences();
      render();
    });
  });

  // Resize handles.
  $$("#list .resizer").forEach((resizer, i) => {
    resizer.addEventListener("mousedown", (e) => onResizerDown(e, resizer, i));
  });

  // Drag-to-reorder via mouse events (not HTML5 drag — see the comment
  // in buildHeaderCell). Drag handle is the .th-label span; the rest of
  // the th and the resizer don't participate.
  $$("#list .th-label").forEach((label) => {
    label.addEventListener("mousedown", onLabelMouseDown);
  });

  // Right-click anywhere in the header row → visibility menu.
  $("#list thead").addEventListener("contextmenu", onHeaderContextMenu);
}

// --- Drag-to-reorder columns (mouse-based) ---

function onLabelMouseDown(e) {
  if (e.button !== 0) return; // left click only
  const th = e.target.closest("th");
  if (!th) return;
  const fromKey = th.dataset.col;

  const startX = e.clientX;
  const startY = e.clientY;
  // Threshold so a plain click (which should sort) doesn't trigger a
  // drag. Once we cross this, the gesture becomes a drag and the
  // post-mouseup click is suppressed via .just-dragged.
  const THRESHOLD = 4;
  let dragging = false;
  let ghost = null;

  const onMove = (ev) => {
    if (!dragging) {
      if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < THRESHOLD) return;
      dragging = true;
      th.classList.add("dragging");
      ghost = document.createElement("div");
      ghost.className = "col-drag-ghost";
      ghost.textContent = COLUMN_DEFS[fromKey]?.label ?? fromKey;
      ghost.style.width = `${th.offsetWidth}px`;
      document.body.appendChild(ghost);
      document.body.style.cursor = "grabbing";
    }
    ghost.style.left = `${ev.clientX + 8}px`;
    ghost.style.top = `${ev.clientY + 8}px`;

    // Highlight the th under the cursor with a drop indicator.
    $$("#list thead th").forEach((t) => {
      t.classList.remove("drop-left", "drop-right");
    });
    const overTh = elementUnder(ev.clientX, ev.clientY)?.closest("#list thead th");
    if (overTh && overTh.dataset.col !== fromKey) {
      const r = overTh.getBoundingClientRect();
      const before = ev.clientX - r.left < r.width / 2;
      overTh.classList.toggle("drop-left", before);
      overTh.classList.toggle("drop-right", !before);
    }
  };

  const onUp = (ev) => {
    document.removeEventListener("mousemove", onMove);
    document.removeEventListener("mouseup", onUp);
    document.body.style.cursor = "";
    if (ghost) ghost.remove();
    $$("#list thead th").forEach((t) => {
      t.classList.remove("dragging", "drop-left", "drop-right");
    });

    if (!dragging) return; // plain click — let the sort handler run

    // Suppress the synthetic click that fires after this mouseup.
    th.classList.add("just-dragged");

    const overTh = elementUnder(ev.clientX, ev.clientY)?.closest("#list thead th");
    if (!overTh || overTh.dataset.col === fromKey) return;
    const r = overTh.getBoundingClientRect();
    const before = ev.clientX - r.left < r.width / 2;
    reorderColumn(fromKey, overTh.dataset.col, before);
  };

  document.addEventListener("mousemove", onMove);
  document.addEventListener("mouseup", onUp);
  // No preventDefault: we want the click event to still fire when there's
  // no drag (so sort works). The just-dragged flag guards against the
  // trailing click when there IS a drag.
}

function elementUnder(x, y) {
  return document.elementFromPoint(x, y);
}

function reorderColumn(fromKey, toKey, before) {
  const order = [...state.columnConfig];
  const fromIdx = order.findIndex((c) => c.key === fromKey);
  if (fromIdx === -1) return;
  const [dragged] = order.splice(fromIdx, 1);
  let toIdx = order.findIndex((c) => c.key === toKey);
  if (toIdx === -1) return;
  if (!before) toIdx++;
  order.splice(toIdx, 0, dragged);
  state.columnConfig = order;
  persistPreferences();
  render();
}

// --- Column visibility (right-click header) ---

function onHeaderContextMenu(e) {
  e.preventDefault();
  e.stopPropagation();
  const menu = $("#ctx-menu");
  menu.innerHTML = "";

  const visibleCount = state.columnConfig.filter((c) => c.visible).length;
  for (const col of state.columnConfig) {
    const def = COLUMN_DEFS[col.key];
    if (!def) continue;
    const li = document.createElement("li");
    li.textContent = (col.visible ? "✓  " : "    ") + def.label;
    const wouldHideLast = col.visible && visibleCount === 1;
    if (wouldHideLast) {
      li.style.opacity = "0.5";
      li.title = "At least one column must stay visible";
    }
    li.addEventListener("click", (ev) => {
      ev.stopPropagation();
      menu.hidden = true;
      if (wouldHideLast) return;
      col.visible = !col.visible;
      persistPreferences();
      render();
    });
    menu.appendChild(li);
  }

  menu.hidden = false;
  menu.style.left = `${e.clientX}px`;
  menu.style.top = `${e.clientY}px`;
  requestAnimationFrame(() => {
    const r = menu.getBoundingClientRect();
    if (r.right > window.innerWidth)
      menu.style.left = `${window.innerWidth - r.width - 4}px`;
    if (r.bottom > window.innerHeight)
      menu.style.top = `${window.innerHeight - r.height - 4}px`;
  });
}

function onResizerDown(e, resizer, idx) {
  e.preventDefault();
  e.stopPropagation();
  const cols = $$("#book-cols col");
  const col = cols[idx];
  if (!col) return;
  const key = col.dataset.col;
  const startX = e.clientX;
  // <col> is invisible to layout — getBoundingClientRect on it returns 0,
  // which used to make the column snap to the 48px minimum on the first
  // pixel of drag. Measure the actual rendered width via the th instead.
  const th = resizer.closest("th");
  const startWidth = th ? th.getBoundingClientRect().width : (state.columnWidths[key] || 100);
  resizer.classList.add("active");
  document.body.style.cursor = "col-resize";
  const onMove = (ev) => {
    const w = Math.max(48, startWidth + ev.clientX - startX);
    state.columnWidths[key] = w;
    col.style.width = `${w}px`;
  };
  const onUp = () => {
    document.removeEventListener("mousemove", onMove);
    document.removeEventListener("mouseup", onUp);
    resizer.classList.remove("active");
    document.body.style.cursor = "";
    saveColumnWidths();
  };
  document.addEventListener("mousemove", onMove);
  document.addEventListener("mouseup", onUp);
}

function applyColumnWidths() {
  $$("#book-cols col").forEach((col) => {
    const key = col.dataset.col;
    const w = state.columnWidths[key];
    col.style.width = w ? `${w}px` : "";
  });
}

/// Measure natural content widths and seed `state.columnWidths` once. We do
/// this only when (a) the list view is visible (so layout is meaningful) and
/// (b) we have books to measure. After that, widths are sticky unless the
/// user drags or clears storage.
function ensureDefaultColumnWidths() {
  if (state.view !== "list") return;
  if (state.books.length === 0) return;
  const cols = $$("#book-cols col");
  const missing = cols.filter((c) => !state.columnWidths[c.dataset.col]);
  if (missing.length === 0) return;

  const table = document.querySelector("#list .book-table");
  if (!table) return;

  // Switch to auto-layout briefly so the browser sizes columns to content.
  cols.forEach((c) => (c.style.width = ""));
  const prevLayout = table.style.tableLayout;
  table.style.tableLayout = "auto";
  void table.offsetWidth; // force reflow

  $$("#list thead th").forEach((th, i) => {
    const key = cols[i]?.dataset.col;
    if (!key) return;
    if (!state.columnWidths[key]) {
      const measured = Math.ceil(th.getBoundingClientRect().width) + 8;
      state.columnWidths[key] = measured;
    }
  });

  table.style.tableLayout = prevLayout || "fixed";
  applyColumnWidths();
  saveColumnWidths();
}

// ---------------------------------------------------------------------------
// Live status events
// ---------------------------------------------------------------------------

function subscribeStatus() {
  window.api.listen("conversion:status", (e) => {
    const { book_id, status, error } = e.payload || {};
    const idx = state.books.findIndex((b) => b.id === book_id);
    if (idx === -1) {
      // Imported elsewhere — full refresh.
      refresh();
      return;
    }
    state.books[idx] = { ...state.books[idx], status, error: error || null };
    // When a conversion finishes, fetch the row to pick up kfx_path. Bump
    // the cache buster too — the worker may have just overwritten the
    // grayscale cover.jpg with the color-fetch result, and without a fresh
    // URL the browser would keep showing the cached desaturated image.
    if (status === "done") {
      state.coverCacheBust += 1;
      refresh();
    } else {
      render();
    }
  });
}

// Build the URL we hand to <img src=…>. Returns null when the book has no
// cover on disk yet. The `?v=N` cache buster matches the file's "version"
// from sidle's perspective — incrementing it on every cover overwrite is
// the cheap way to force the webview to re-fetch.
function coverUrlFor(b) {
  if (!b || !b.cover_path) return null;
  const base = window.api.fileUrl(b.cover_path);
  if (!base) return null;
  return `${base}?v=${state.coverCacheBust}`;
}

// ---------------------------------------------------------------------------
// Device pill + popover
// ---------------------------------------------------------------------------

function wireDevice() {
  $("#device-pill").addEventListener("click", (e) => {
    e.stopPropagation();
    const pop = $("#device-popover");
    pop.hidden = !pop.hidden;
    if (!pop.hidden) refreshDeviceList();
  });
  $("#btn-send-unsent").addEventListener("click", () => sendUnsent());
  // Clicks INSIDE the popover shouldn't close it.
  $("#device-popover").addEventListener("click", (e) => e.stopPropagation());
  document.addEventListener("click", (e) => {
    const root = $("#device");
    if (root && !root.contains(e.target)) $("#device-popover").hidden = true;
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") $("#device-popover").hidden = true;
  });
}

function subscribeSendProgress() {
  window.api.listen("device:send-progress", (e) => {
    const r = e.payload;
    if (!r) return;
    const prog = $("#device-send-progress");
    let line;
    if (r.kind === "pushed") line = `sent: ${r.filename}`;
    else if (r.kind === "already_present") line = `already on device: ${r.filename}`;
    else if (r.kind === "skipped") line = `skipped (${r.reason})`;
    else line = `failed: ${r.error}`;
    prog.hidden = false;
    prog.textContent = line;
  });
}

async function sendBooks(bookIds) {
  if (!state.device) {
    showToast("no Kindle connected", true);
    return;
  }
  const btn = $("#btn-send-unsent");
  btn.disabled = true;
  let results = [];
  try {
    results = await window.api.invoke("device_send", { bookIds });
  } catch (e) {
    showToast(`send failed: ${e}`, true);
    btn.disabled = false;
    return;
  }
  const counts = { pushed: 0, already_present: 0, skipped: 0, failed: 0 };
  for (const r of results) counts[r.kind] = (counts[r.kind] || 0) + 1;
  const parts = [];
  if (counts.pushed) parts.push(`${counts.pushed} sent`);
  if (counts.already_present) parts.push(`${counts.already_present} already there`);
  if (counts.skipped) parts.push(`${counts.skipped} skipped`);
  if (counts.failed) parts.push(`${counts.failed} failed`);
  showToast(parts.join(" · ") || "nothing to do", counts.failed > 0);
  await refreshDeviceList();
  updateSendUnsentButton();
  setTimeout(() => {
    $("#device-send-progress").hidden = true;
  }, 2000);
}

async function sendUnsent() {
  const unsent = state.books.filter(
    (b) => b.status === "done" && !state.sentSet.has(b.sha256),
  );
  if (unsent.length === 0) {
    showToast("nothing to send");
    return;
  }
  await sendBooks(unsent.map((b) => b.id));
}

function updateSendUnsentButton() {
  const btn = $("#btn-send-unsent");
  if (!state.device) {
    btn.disabled = true;
    btn.textContent = "Send all unsent";
    return;
  }
  const count = state.books.filter(
    (b) => b.status === "done" && !state.sentSet.has(b.sha256),
  ).length;
  btn.disabled = count === 0;
  btn.textContent = count === 0 ? "Send all unsent" : `Send all unsent (${count})`;
}

function subscribeDeviceStatus() {
  window.api.listen("device:status", (e) => updateDeviceUI(e.payload));
  window.api.invoke("device_status").then(updateDeviceUI).catch(() => {});
}

function updateDeviceUI(info) {
  state.device = info || null;
  const dot = $("#device-pill .device-dot");
  const label = $("#device-pill-label");
  const status = $("#device-popover-status");
  const tip = $("#device-tip");
  if (info) {
    dot.className = "device-dot connected";
    const free = info.free_bytes ? `· ${formatBytes(info.free_bytes)} free` : "";
    label.textContent = `Kindle ${free}`.trim();
    status.className = "device-popover-status connected";
    status.textContent = "Connected";
    $("#device-model").textContent = info.model || "Kindle";
    $("#device-serial").textContent = info.serial || "—";
    $("#device-transport").textContent = transportLabel(info.transport);
    $("#device-free").textContent =
      info.free_bytes != null && info.total_bytes != null
        ? `${formatBytes(info.free_bytes)} of ${formatBytes(info.total_bytes)}`
        : "—";
    // MTP devices need exclusive USB session access, so any other app
    // currently talking to the Kindle (Image Capture, OpenMTP, Calibre)
    // will block sidle's push/delete with a "device busy" error. The tip
    // names them so the user knows what to quit. Mass-storage doesn't
    // have this contention.
    if (info.transport === "mtp") {
      tip.textContent =
        "MTP device. Quit Image Capture, OpenMTP, or Calibre if a push fails — only one app can hold the USB session at a time.";
      tip.hidden = false;
    } else {
      tip.hidden = true;
      tip.textContent = "";
    }
    // Always load sent state when device connects so the list-view "On Kindle"
    // column reflects reality without the user having to open the popover.
    refreshDeviceList();
  } else {
    dot.className = "device-dot disconnected";
    label.textContent = "No Kindle";
    status.className = "device-popover-status disconnected";
    status.textContent = "Disconnected";
    $("#device-model").textContent = "—";
    $("#device-serial").textContent = "—";
    $("#device-transport").textContent = "—";
    $("#device-free").textContent = "—";
    $("#device-count").textContent = "—";
    $("#device-sent-list").innerHTML = "";
    tip.hidden = true;
    tip.textContent = "";
    setSent([]);
    $("#device-empty").textContent = "Plug in a Kindle via USB.";
    $("#device-empty").hidden = false;
    render();
  }
}

function transportLabel(t) {
  if (t === "mass_storage") return "USB (mass storage)";
  if (t === "mtp") return "USB (MTP)";
  return t || "—";
}

async function refreshDeviceList() {
  if (!state.device) {
    setSent([]);
    renderDeviceList();
    render();
    return;
  }
  try {
    const rows = await window.api.invoke("device_list_ours");
    setSent(rows);
  } catch (e) {
    console.error("device_list_ours failed:", e);
    setSent([]);
  }
  renderDeviceList();
  render();
}

function renderDeviceList() {
  const list = $("#device-sent-list");
  list.innerHTML = "";
  const rows = state.sent || [];
  $("#device-count").textContent =
    rows.length === 0 ? "0 books" : `${rows.length} book${rows.length === 1 ? "" : "s"}`;
  const empty = $("#device-empty");
  if (rows.length === 0) {
    empty.textContent = "Nothing sent yet.";
    empty.hidden = false;
  } else {
    empty.hidden = true;
  }
  for (const r of rows) list.appendChild(deviceRow(r));
  updateSendUnsentButton();
}

function deviceRow(r) {
  const li = document.createElement("li");
  const top = document.createElement("div");
  top.className = "device-sent-top";

  const title = document.createElement("div");
  title.className = "device-sent-title";
  title.textContent = r.title || r.filename;
  title.title = r.filename;

  const del = document.createElement("button");
  del.type = "button";
  del.className = "device-sent-del";
  del.title = "Remove from Kindle";
  del.textContent = "×";
  del.addEventListener("click", (e) => {
    e.stopPropagation();
    deleteFromDevice([r.sha256], [r.title || r.filename]);
  });

  top.append(title, del);

  const meta = document.createElement("div");
  meta.className = "device-sent-meta";
  const parts = [];
  if (r.author) parts.push(r.author);
  parts.push(formatDate(r.sent_at));
  meta.textContent = parts.join(" · ");
  if (!r.file_present) {
    const missing = document.createElement("span");
    missing.className = "device-sent-missing";
    missing.textContent = " · missing on device";
    meta.appendChild(missing);
  }

  li.append(top, meta);
  return li;
}

async function deleteFromDevice(sha256s, titles) {
  if (!state.device) {
    showToast("no Kindle connected", true);
    return;
  }
  const label =
    titles.length === 1
      ? `Remove "${titles[0]}" from the Kindle?`
      : `Remove ${titles.length} books from the Kindle?`;
  if (!confirm(`${label}\n\nThe file is deleted from /documents; the local library is untouched.`)) {
    return;
  }
  let results = [];
  try {
    results = await window.api.invoke("device_delete", { sha256s });
  } catch (e) {
    showToast(`delete failed: ${e}`, true);
    return;
  }
  const counts = { removed: 0, not_ours: 0, failed: 0 };
  for (const r of results) counts[r.kind] = (counts[r.kind] || 0) + 1;
  const parts = [];
  if (counts.removed) parts.push(`${counts.removed} removed`);
  if (counts.not_ours) parts.push(`${counts.not_ours} not ours`);
  if (counts.failed) parts.push(`${counts.failed} failed`);
  showToast(parts.join(" · "), counts.failed > 0);
  await refreshDeviceList();
}

// ---------------------------------------------------------------------------
// Auto-pull from Kindle /dedrm
//
// The backend pulls and converts every new /dedrm file on Kindle connect —
// no user interaction needed. We just listen for the per-file progress (to
// keep the popover's status line current) and a final summary toast.
// ---------------------------------------------------------------------------

function subscribePullProgress() {
  // Per-file event from the autopull worker. Updates the device-popover
  // status line for users who have the popover open, and — for actually-
  // imported files — triggers a library refresh so the new row appears in
  // the gallery the moment it lands on disk, instead of waiting for the
  // whole batch to finish.
  window.api.listen("device:pull-progress", async (e) => {
    const r = e.payload;
    if (!r) return;
    const prog = $("#device-send-progress");
    let line;
    const name = (r.path || "").split("/").pop() || "";
    if (r.kind === "imported") line = `imported: ${name}`;
    else if (r.kind === "duplicate") line = `already in library: ${name}`;
    else line = `failed: ${r.error}`;
    prog.hidden = false;
    prog.textContent = line;
    if (r.kind === "imported") await refresh();
  });

  // Status-bar progress counter. The backend emits this once on autopull
  // start (`done: 0`) and once per book completed thereafter — covers the
  // gap that used to look like a freeze where nothing rendered until the
  // whole pull was done.
  window.api.listen("device:autopull-progress", (e) => {
    const p = e.payload;
    if (!p) return;
    state.autopull = { done: p.done, total: p.total };
    renderQueue();
  });

  window.api.listen("device:autopull-done", async (e) => {
    const s = e.payload || { imported: 0, duplicate: 0, failed: 0 };
    state.autopull = null;
    if (s.imported + s.duplicate + s.failed > 0) {
      const parts = [];
      if (s.imported) parts.push(`${s.imported} imported`);
      if (s.duplicate) parts.push(`${s.duplicate} already in library`);
      if (s.failed) parts.push(`${s.failed} failed`);
      showToast(`Kindle /dedrm: ${parts.join(" · ")}`, s.failed > 0);
      setTimeout(() => {
        $("#device-send-progress").hidden = true;
      }, 2000);
    }
    // One final refresh to pick up the last row's status + reset the
    // status-bar line to the queue summary.
    await refresh();
  });
}

// ---------------------------------------------------------------------------
// Queue drawer
// ---------------------------------------------------------------------------

function wireQueueDrawer() {
  $("#status-bar-toggle").addEventListener("click", () => {
    const drawer = $("#queue-drawer");
    drawer.hidden = !drawer.hidden;
  });
  $("#queue-drawer-close").addEventListener("click", () => {
    $("#queue-drawer").hidden = true;
  });
}

function renderQueue() {
  const active = state.books.filter(
    (b) => b.status === "pending" || b.status === "converting" || b.status === "error",
  );

  const counts = {
    converting: active.filter((b) => b.status === "converting").length,
    pending: active.filter((b) => b.status === "pending").length,
    error: active.filter((b) => b.status === "error").length,
    total: state.books.length,
  };

  const toggle = $("#status-bar-toggle");
  const summary = $("#status-bar-summary");
  toggle.classList.remove("active", "errors", "done");

  // When an autopull is in flight, its progress takes the status-bar line
  // (the queue counts will follow once the converting jobs kick in). Don't
  // overwrite it here.
  if (state.autopull) {
    summary.textContent =
      `Pulling ${state.autopull.done}/${state.autopull.total} from Kindle…`;
    toggle.classList.add("active");
  } else {
    const parts = [];
    if (counts.converting) parts.push(`${counts.converting} converting`);
    if (counts.pending) parts.push(`${counts.pending} queued`);
    if (counts.error) parts.push(`${counts.error} failed`);
    if (parts.length === 0) {
      summary.textContent = counts.total
        ? `Library: ${counts.total} book${counts.total === 1 ? "" : "s"}`
        : "No conversions running";
      toggle.classList.add("done");
    } else {
      summary.textContent = parts.join("  ·  ");
      if (counts.error) toggle.classList.add("errors");
      else if (counts.converting) toggle.classList.add("active");
    }
  }

  const ul = $("#queue-list");
  ul.innerHTML = "";
  for (const b of active) ul.appendChild(queueRow(b));
  $("#queue-empty").hidden = active.length > 0;
}

function queueRow(b) {
  const li = document.createElement("li");

  const main = document.createElement("div");
  main.className = "queue-row-main";

  const title = document.createElement("div");
  title.className = "queue-title";
  title.textContent = b.title || "Untitled";

  const status = document.createElement("div");
  status.className = `queue-status ${b.status}`;
  if (b.status === "error") {
    const label = document.createElement("span");
    label.textContent = "Failed";
    label.title = b.error || "";
    status.appendChild(label);
    const retry = document.createElement("button");
    retry.className = "queue-retry";
    retry.textContent = "Retry";
    retry.addEventListener("click", () => retryConvert(b.id));
    status.appendChild(retry);
  } else if (b.status === "converting") {
    status.textContent = "Converting…";
  } else {
    status.textContent = "Queued";
  }

  main.append(title, status);

  const meta = document.createElement("div");
  meta.className = "queue-meta";
  meta.textContent = b.author || "";

  const bar = document.createElement("div");
  bar.className = `queue-progress ${b.status}`;

  li.append(main, meta, bar);
  return li;
}

// ---------------------------------------------------------------------------
// Context menu
// ---------------------------------------------------------------------------

function wireContextMenu() {
  document.addEventListener("click", () => ($("#ctx-menu").hidden = true));
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") $("#ctx-menu").hidden = true;
  });
}

function openContextMenu(x, y, b) {
  const menu = $("#ctx-menu");
  menu.innerHTML = "";

  if (state.selected.size > 1) {
    // Multi-selection menu — bulk actions.
    const sel = selectedBooks();
    const send = sel.filter(
      (s) => s.status === "done" && !state.sentSet.has(s.sha256),
    );
    const unsend = sel.filter((s) => state.sentSet.has(s.sha256));
    if (state.device && send.length) {
      add(menu, `Send to Kindle (${send.length})`, () => bulkSend());
    }
    if (state.device && unsend.length) {
      add(menu, `Remove from Kindle (${unsend.length})`, () => bulkUnsend());
    }
    add(menu, `Remove ${sel.length} from library`, () => bulkRemove(), true);
  } else {
    // Single-item menu.
    if (state.device && b.status === "done") {
      if (state.sentSet.has(b.sha256)) {
        add(menu, "Remove from Kindle", () =>
          deleteFromDevice([b.sha256], [b.title]),
        );
      } else {
        add(menu, "Send to Kindle", () => sendBooks([b.id]));
      }
    }
    add(menu, "Edit metadata…", () => openMetadataModal(b));
    add(menu, "Open in Finder", () => openInFinder(b.id));
    add(menu, "Re-fetch cover", () => recrawlCover(b));
    add(menu, "Force re-convert", () => retryConvert(b.id));
    add(menu, "Remove from library", () => removeBook(b), true);
  }

  menu.hidden = false;
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;
  // Keep on-screen
  requestAnimationFrame(() => {
    const r = menu.getBoundingClientRect();
    if (r.right > window.innerWidth) menu.style.left = `${window.innerWidth - r.width - 4}px`;
    if (r.bottom > window.innerHeight) menu.style.top = `${window.innerHeight - r.height - 4}px`;
  });
}

function add(menu, label, fn, danger = false) {
  const li = document.createElement("li");
  li.textContent = label;
  if (danger) li.className = "danger";
  li.addEventListener("click", (ev) => {
    ev.stopPropagation();
    menu.hidden = true;
    fn();
  });
  menu.appendChild(li);
}

// ---------------------------------------------------------------------------
// Selection (multi-select + bulk actions)
// ---------------------------------------------------------------------------

function wireSelection() {
  $("#sel-send").addEventListener("click", bulkSend);
  $("#sel-unsend").addEventListener("click", bulkUnsend);
  $("#sel-delete").addEventListener("click", bulkRemove);
  $("#sel-clear").addEventListener("click", clearSelection);

  // Lasso + click-to-clear behavior on empty area of main. We use mousedown
  // (rather than click) so we can distinguish a drag (→ lasso) from a tap
  // (→ clear selection).
  $("#main").addEventListener("mousedown", onMainMouseDown);

  // Esc clears selection. Cmd/Ctrl-A selects all.
  document.addEventListener("keydown", (e) => {
    const t = e.target;
    const inField =
      t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable);
    if (e.key === "Escape" && state.selected.size > 0) {
      clearSelection();
      return;
    }
    if ((e.metaKey || e.ctrlKey) && e.key === "a" && !inField) {
      e.preventDefault();
      state.selected = new Set(state.books.map((b) => b.id));
      render();
    }
  });
}

const LASSO_THRESHOLD = 4; // px before a mousedown promotes to a drag

function onMainMouseDown(e) {
  if (e.button !== 0) return; // primary button only
  // Anything actionable: let the card/row/header/resizer handler take it.
  if (
    e.target.closest(
      ".book-card, .book-table tbody tr, .book-table thead, .resizer",
    )
  ) {
    return;
  }

  const startX = e.clientX;
  const startY = e.clientY;
  const additive = e.metaKey || e.ctrlKey || e.shiftKey;
  const baseSelection = additive ? new Set(state.selected) : new Set();
  let active = false;
  const lasso = $("#lasso");

  const onMove = (ev) => {
    if (!active) {
      const dx = ev.clientX - startX;
      const dy = ev.clientY - startY;
      if (Math.hypot(dx, dy) < LASSO_THRESHOLD) return;
      active = true;
      lasso.hidden = false;
    }
    positionLasso(lasso, startX, startY, ev.clientX, ev.clientY);
    const rect = makeRect(startX, startY, ev.clientX, ev.clientY);
    const hits = computeLassoHits(rect);
    state.selected = new Set([...baseSelection, ...hits]);
    applyLassoVisuals();
  };

  const onUp = () => {
    document.removeEventListener("mousemove", onMove);
    document.removeEventListener("mouseup", onUp);
    if (active) {
      lasso.hidden = true;
      state.lastClicked = null;
      render();
    } else {
      // No drag — treat as a click on empty area: clear unless additive.
      if (!additive) clearSelection();
    }
  };

  document.addEventListener("mousemove", onMove);
  document.addEventListener("mouseup", onUp);
  e.preventDefault();
}

function positionLasso(el, x1, y1, x2, y2) {
  const left = Math.min(x1, x2);
  const top = Math.min(y1, y2);
  el.style.left = `${left}px`;
  el.style.top = `${top}px`;
  el.style.width = `${Math.abs(x2 - x1)}px`;
  el.style.height = `${Math.abs(y2 - y1)}px`;
}

function makeRect(x1, y1, x2, y2) {
  return {
    left: Math.min(x1, x2),
    top: Math.min(y1, y2),
    right: Math.max(x1, x2),
    bottom: Math.max(y1, y2),
  };
}

function computeLassoHits(rect) {
  const items =
    state.view === "gallery"
      ? $$("#gallery-grid .book-card")
      : $$("#list-body tr");
  const hits = new Set();
  for (const el of items) {
    const r = el.getBoundingClientRect();
    if (rectsIntersect(rect, r)) {
      const id = Number(el.dataset.bookId);
      if (id) hits.add(id);
    }
  }
  return hits;
}

function rectsIntersect(a, b) {
  return !(
    a.right < b.left ||
    a.left > b.right ||
    a.bottom < b.top ||
    a.top > b.bottom
  );
}

/// Update only the .selected classes + the action bar during a drag — avoids
/// a full re-render (which would tear down + rebuild every card/row on every
/// mousemove frame).
function applyLassoVisuals() {
  const sel = state.selected;
  $$("#gallery-grid .book-card").forEach((el) => {
    const id = Number(el.dataset.bookId);
    el.classList.toggle("selected", sel.has(id));
  });
  $$("#list-body tr").forEach((el) => {
    const id = Number(el.dataset.bookId);
    el.classList.toggle("selected", sel.has(id));
  });
  renderSelectionBar();
}

function onItemClick(e, b) {
  e.stopPropagation();
  if (e.shiftKey && state.lastClicked != null) {
    selectRangeTo(b.id);
  } else if (e.metaKey || e.ctrlKey) {
    toggleSelected(b.id);
    state.lastClicked = b.id;
  } else {
    state.selected = new Set([b.id]);
    state.lastClicked = b.id;
  }
  render();
}

/// Context menu (right-click) behavior:
/// - if right-click hits an already-selected book, keep the selection as-is
///   so the menu can operate on the multi-selection
/// - if it hits an unselected book, reset selection to just that one
function onItemContext(_e, b) {
  if (state.selected.has(b.id)) return;
  state.selected = new Set([b.id]);
  state.lastClicked = b.id;
  render();
}

function selectRangeTo(toId) {
  const ordered = sortedBooks();
  const from = ordered.findIndex((b) => b.id === state.lastClicked);
  const to = ordered.findIndex((b) => b.id === toId);
  if (from === -1 || to === -1) {
    state.selected.add(toId);
    return;
  }
  const [lo, hi] = from < to ? [from, to] : [to, from];
  for (let i = lo; i <= hi; i++) state.selected.add(ordered[i].id);
}

function toggleSelected(id) {
  if (state.selected.has(id)) state.selected.delete(id);
  else state.selected.add(id);
}

function clearSelection() {
  if (state.selected.size === 0) return;
  state.selected.clear();
  state.lastClicked = null;
  render();
}

function selectedBooks() {
  return state.books.filter((b) => state.selected.has(b.id));
}

function renderSelectionBar() {
  const bar = $("#selection-bar");
  const n = state.selected.size;
  if (n === 0) {
    bar.hidden = true;
    return;
  }
  bar.hidden = false;
  $("#selection-count").textContent = `${n} selected`;

  const sel = selectedBooks();
  const eligibleSend = sel.filter(
    (b) => b.status === "done" && !state.sentSet.has(b.sha256),
  );
  const eligibleUnsend = sel.filter((b) => state.sentSet.has(b.sha256));

  const send = $("#sel-send");
  send.disabled = !state.device || eligibleSend.length === 0;
  send.textContent =
    eligibleSend.length && eligibleSend.length !== n
      ? `Send to Kindle (${eligibleSend.length})`
      : "Send to Kindle";

  const unsend = $("#sel-unsend");
  unsend.disabled = !state.device || eligibleUnsend.length === 0;
  unsend.textContent =
    eligibleUnsend.length && eligibleUnsend.length !== n
      ? `Remove from Kindle (${eligibleUnsend.length})`
      : "Remove from Kindle";
}

async function bulkSend() {
  const eligible = selectedBooks().filter(
    (b) => b.status === "done" && !state.sentSet.has(b.sha256),
  );
  if (eligible.length === 0) {
    showToast("nothing to send");
    return;
  }
  await sendBooks(eligible.map((b) => b.id));
}

async function bulkUnsend() {
  const eligible = selectedBooks().filter((b) => state.sentSet.has(b.sha256));
  if (eligible.length === 0) return;
  await deleteFromDevice(
    eligible.map((b) => b.sha256),
    eligible.map((b) => b.title || b.sha256.slice(0, 8)),
  );
}

async function bulkRemove() {
  const sel = selectedBooks();
  if (sel.length === 0) return;
  const msg =
    sel.length === 1
      ? `Remove "${sel[0].title}" from the library?`
      : `Remove ${sel.length} books from the library?`;
  if (
    !confirm(
      `${msg}\n\nThis deletes the cached EPUB and KFX. The Kindle is untouched.`,
    )
  ) {
    return;
  }
  for (const b of sel) {
    try {
      await window.api.invoke("library_remove", { bookId: b.id });
    } catch (e) {
      console.error("remove failed:", b.id, e);
    }
  }
  state.selected.clear();
  state.lastClicked = null;
  await refresh();
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async function openInFinder(bookId) {
  try {
    await window.api.invoke("library_open_in_finder", { bookId });
  } catch (e) {
    showToast(`open failed: ${e}`, true);
  }
}

async function retryConvert(bookId) {
  try {
    await window.api.invoke("conversion_retry", { bookId });
  } catch (e) {
    showToast(`retry failed: ${e}`, true);
  }
}

async function recrawlCover(b) {
  let result;
  try {
    result = await window.api.invoke("library_recrawl_cover", { bookId: b.id });
  } catch (e) {
    showToast(`cover fetch error: ${e}`, true);
    return;
  }
  if (result.kind === "no_asin") {
    showToast("no ASIN — can't fetch", true);
    return;
  }
  if (result.kind === "failed") {
    showToast(`cover fetch failed: ${result.error}`, true);
    return;
  }
  // kind === "updated"
  state.coverCacheBust += 1;
  showToast(`cover updated: ${b.title}`);
  await refresh();
}

async function removeBook(b) {
  if (!confirm(`Remove "${b.title}" from the library?\n\nThis deletes the cached EPUB and KFX.`)) {
    return;
  }
  try {
    await window.api.invoke("library_remove", { bookId: b.id });
    await refresh();
  } catch (e) {
    showToast(`remove failed: ${e}`, true);
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function formatBytes(n) {
  if (!Number.isFinite(n)) return "";
  const u = ["B", "KB", "MB", "GB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 10 || i === 0 ? 0 : 1)} ${u[i]}`;
}

function formatDate(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

let toastTimer = null;
function showToast(msg, isError = false) {
  const t = $("#toast");
  t.textContent = msg;
  t.className = `toast${isError ? " error" : ""}`;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 4000);
}

// ---------------------------------------------------------------------------
// Filter bar + sort UI
//
// One filter bar serves both gallery and list views — it lives between the
// toolbar and <main>, outside either view section. render() composes
// sortedBooks(visibleBooks(state.books)) so both views consume the same
// filtered+sorted set.
// ---------------------------------------------------------------------------

// Currently-open dropdown facet name (e.g. "language") or null.
let openDropdownFacet = null;
// Current dropdown internal search input value.
let dropdownSearch = "";
// Debounce timer for the global free-text search input.
let searchDebounceTimer = null;

function wireFilterBar() {
  // "All" pill — clears every facet + search.
  document
    .querySelector('.pill[data-pill="all"]')
    .addEventListener("click", clearAllFilters);

  // Facet pills.
  document.querySelectorAll(".pill[data-facet]").forEach((pill) => {
    pill.addEventListener("click", (e) => {
      // The × inline button (created lazily in renderFilterBar) handles
      // its own click via stopPropagation; this only fires when the pill
      // body is clicked.
      const facet = pill.dataset.facet;
      if (openDropdownFacet === facet) closeFilterDropdown();
      else openFilterDropdown(facet, pill);
    });
  });

  // Global free-text search.
  const search = $("#search-input");
  search.value = state.search;
  search.addEventListener("input", () => {
    clearTimeout(searchDebounceTimer);
    searchDebounceTimer = setTimeout(() => {
      state.search = search.value;
      persistPreferences();
      render();
    }, 100);
  });
  search.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      search.value = "";
      state.search = "";
      persistPreferences();
      render();
    }
  });

  // Sort button.
  $("#sort-button").addEventListener("click", () => {
    if ($("#sort-popover").hidden) openSortPopover();
    else closeSortPopover();
  });

  // Dropdown internal search.
  $(".filter-dropdown-search").addEventListener("input", (e) => {
    dropdownSearch = e.target.value;
    if (openDropdownFacet) renderDropdownOptions(openDropdownFacet);
  });

  // Dropdown clear.
  $(".filter-dropdown-clear").addEventListener("click", () => {
    if (openDropdownFacet) clearFacet(openDropdownFacet);
  });

  // Dismiss popovers on outside click + Escape.
  document.addEventListener("click", (e) => {
    if (
      openDropdownFacet &&
      !$("#filter-dropdown").contains(e.target) &&
      !e.target.closest(`.pill[data-facet="${openDropdownFacet}"]`)
    ) {
      closeFilterDropdown();
    }
    if (
      !$("#sort-popover").hidden &&
      !$("#sort-popover").contains(e.target) &&
      !e.target.closest("#sort-button")
    ) {
      closeSortPopover();
    }
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      closeFilterDropdown();
      closeSortPopover();
    }
  });
}

function renderFilterBar() {
  // "All" pill is active iff no filter is active.
  const allPill = document.querySelector('.pill[data-pill="all"]');
  allPill.classList.toggle("active", !hasAnyFilter());

  // Facet pills: visual state reflects the filter Set.
  document.querySelectorAll(".pill[data-facet]").forEach((pill) => {
    const facet = pill.dataset.facet;
    const sel = state.filters[facet];
    const baseLabel = facetDisplayLabel(facet);

    // Rebuild the pill's interior: <span class="pill-label">…</span>
    // optionally followed by an × clear button.
    pill.innerHTML = "";
    const label = document.createElement("span");
    label.className = "pill-label";
    if (sel.size === 0) {
      label.textContent = baseLabel;
      pill.classList.remove("active");
    } else if (sel.size === 1) {
      const [value] = sel;
      label.textContent = `${baseLabel}: ${value}`;
      pill.classList.add("active");
    } else {
      label.textContent = `${baseLabel}: ${sel.size}`;
      pill.classList.add("active");
    }
    pill.appendChild(label);

    if (sel.size > 0) {
      const clear = document.createElement("span");
      clear.className = "pill-clear";
      clear.textContent = "×";
      clear.title = `Clear ${baseLabel}`;
      clear.addEventListener("click", (e) => {
        e.stopPropagation();
        clearFacet(facet);
      });
      pill.appendChild(clear);
    }
  });
}

function facetDisplayLabel(facet) {
  switch (facet) {
    case "language": return "Language";
    case "author":   return "Author";
    case "on_kindle": return "On Kindle";
    case "publisher": return "Publisher";
    case "series":   return "Series";
    case "tags":     return "Tags";
    default:         return facet;
  }
}

function openFilterDropdown(facet, anchorPill) {
  openDropdownFacet = facet;
  dropdownSearch = "";
  const dd = $("#filter-dropdown");
  dd.hidden = false;
  dd.querySelector(".filter-dropdown-search").value = "";
  positionPopover(dd, anchorPill);
  renderDropdownOptions(facet);
  // Focus the inner search for immediate filtering.
  dd.querySelector(".filter-dropdown-search").focus();
}

function closeFilterDropdown() {
  if (!openDropdownFacet) return;
  openDropdownFacet = null;
  dropdownSearch = "";
  $("#filter-dropdown").hidden = true;
}

function renderDropdownOptions(facet) {
  const dd = $("#filter-dropdown");
  const list = dd.querySelector(".filter-dropdown-list");
  const empty = dd.querySelector(".filter-dropdown-empty");
  list.innerHTML = "";

  const needle = dropdownSearch.trim().toLowerCase();
  const all = facetOptions(facet);
  const filtered = needle
    ? all.filter(([v]) => v.toLowerCase().includes(needle))
    : all;

  if (filtered.length === 0) {
    empty.hidden = false;
    return;
  }
  empty.hidden = true;

  const selected = state.filters[facet];
  for (const [value, count] of filtered) {
    const li = document.createElement("li");
    if (count === 0) li.classList.add("zero");

    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = selected.has(value);
    cb.addEventListener("click", (e) => e.stopPropagation());
    cb.addEventListener("change", () => toggleFilterValue(facet, value));

    const lbl = document.createElement("span");
    lbl.className = "opt-label";
    lbl.textContent = value;
    lbl.title = value;

    const cnt = document.createElement("span");
    cnt.className = "opt-count";
    cnt.textContent = count;

    li.append(cb, lbl, cnt);
    li.addEventListener("click", () => {
      cb.checked = !cb.checked;
      toggleFilterValue(facet, value);
    });
    list.appendChild(li);
  }
}

function positionPopover(popover, anchor) {
  // Render off-screen first to measure intrinsic size, then place under
  // the anchor. Clamps to viewport horizontally.
  popover.style.visibility = "hidden";
  popover.style.left = "0px";
  popover.style.top = "0px";
  const aRect = anchor.getBoundingClientRect();
  const pRect = popover.getBoundingClientRect();
  let left = aRect.left;
  const maxLeft = window.innerWidth - pRect.width - 8;
  if (left > maxLeft) left = Math.max(8, maxLeft);
  popover.style.left = `${left}px`;
  popover.style.top = `${aRect.bottom + 4}px`;
  popover.style.visibility = "";
}

// --- Sort UI ---

function renderSortControl() {
  // Button label reflects current sort.
  const label = SORT_KEYS.find(([k]) => k === state.sort.key)?.[1] ?? "—";
  $("#sort-button .sort-label").textContent = `Sort: ${label}`;
  $("#sort-button .sort-dir").textContent = state.sort.asc ? "↑" : "↓";

  // Populate the popover key list. Rebuild each time so .active is fresh.
  const list = $("#sort-key-list");
  list.innerHTML = "";
  for (const [key, name] of SORT_KEYS) {
    const li = document.createElement("li");
    li.dataset.key = key;
    if (state.sort.key === key) li.classList.add("active");
    const radio = document.createElement("span");
    radio.className = "sort-radio";
    const text = document.createElement("span");
    text.textContent = name;
    li.append(radio, text);
    li.addEventListener("click", () => {
      state.sort = { key, asc: state.sort.asc };
      persistPreferences();
      render();
    });
    list.appendChild(li);
  }

  // Direction buttons.
  $$("#sort-popover .sort-dir-toggle button").forEach((btn) => {
    btn.classList.toggle(
      "active",
      btn.dataset.dir === (state.sort.asc ? "asc" : "desc"),
    );
  });
}

function openSortPopover() {
  const pop = $("#sort-popover");
  pop.hidden = false;
  positionPopover(pop, $("#sort-button"));
}

function closeSortPopover() {
  $("#sort-popover").hidden = true;
}

function wireSortPopover() {
  $$("#sort-popover .sort-dir-toggle button").forEach((btn) => {
    btn.addEventListener("click", () => {
      state.sort = { ...state.sort, asc: btn.dataset.dir === "asc" };
      persistPreferences();
      render();
    });
  });
}

// ---------------------------------------------------------------------------
// Metadata editor modal
//
// Right-click → "Edit metadata…" opens this modal prefilled from the row.
// All text fields commit together on Save (the form always submits the
// full set — matches the Rust MetadataPatch shape). Cover changes commit
// immediately when the user picks a file via library_set_cover; Cancel
// does NOT revert the cover. See library-navigation.md Phase 4 for the
// "immediate-apply" rationale.
// ---------------------------------------------------------------------------

let metadataBook = null;

function wireMetadataModal() {
  $("#metadata-cancel").addEventListener("click", closeMetadataModal);
  $("#metadata-form").addEventListener("submit", (e) => {
    e.preventDefault();
    submitMetadataForm();
  });
  $("#metadata-cover-change").addEventListener("click", onCoverChangeClick);

  // Esc closes; Cmd/Ctrl+Enter submits. Scoped to the modal element so
  // it doesn't fight the global selection/context-menu shortcuts.
  $("#metadata-modal").addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      closeMetadataModal();
    } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submitMetadataForm();
    }
  });

  // Backdrop click closes (but not inside the panel).
  $("#metadata-modal .modal-backdrop").addEventListener("click", closeMetadataModal);
}

function openMetadataModal(book) {
  metadataBook = book;
  const form = $("#metadata-form");
  form.title.value = book.title || "";
  form.author.value = book.author || "";
  form.language.value = book.language || "";
  form.publisher.value = book.publisher || "";
  form.published_at.value = book.published_at || "";
  form.series_name.value = book.series_name || "";
  form.series_index.value =
    book.series_index != null && Number.isFinite(book.series_index)
      ? String(book.series_index)
      : "";
  // Tags display as comma-joined; canonicalization happens on the
  // backend so case + duplicates clean themselves up on save.
  form.tags.value = (book.tags || []).join(", ");

  renderCoverPreview(book);

  $("#metadata-modal").hidden = false;
  // Focus the title input for keyboard-first flow.
  setTimeout(() => form.title.focus(), 0);
}

function closeMetadataModal() {
  $("#metadata-modal").hidden = true;
  metadataBook = null;
}

function renderCoverPreview(book) {
  const box = $("#metadata-cover-preview");
  box.innerHTML = "";
  const url = coverUrlFor(book);
  if (url) {
    const img = document.createElement("img");
    img.src = url;
    img.alt = "";
    box.appendChild(img);
  } else {
    box.textContent = book.title || "No cover";
  }
}

async function submitMetadataForm() {
  if (!metadataBook) return;
  const form = $("#metadata-form");
  const tagsRaw = form.tags.value.trim();
  const patch = {
    title: form.title.value.trim(),
    author: form.author.value.trim(),
    language: form.language.value.trim(),
    publisher:
      form.publisher.value.trim() === "" ? null : form.publisher.value.trim(),
    published_at:
      form.published_at.value.trim() === ""
        ? null
        : form.published_at.value.trim(),
    series_name:
      form.series_name.value.trim() === "" ? null : form.series_name.value.trim(),
    series_index:
      form.series_index.value === "" ? null : Number(form.series_index.value),
    // Accept ASCII or CJK comma — same as the author facet split. The
    // backend lowercases + dedupes + drops empties.
    tags:
      tagsRaw === ""
        ? []
        : tagsRaw.split(/[,、]/).map((s) => s.trim()).filter(Boolean),
  };

  if (!patch.title) {
    showToast("Title cannot be empty.", true);
    return;
  }
  if (
    patch.series_index != null &&
    (!Number.isFinite(patch.series_index) || patch.series_index < 0)
  ) {
    showToast("Series # must be a non-negative number.", true);
    return;
  }

  try {
    const updated = await window.api.invoke("library_update_metadata", {
      bookId: metadataBook.id,
      patch,
    });
    mergeBookRow(updated);
    closeMetadataModal();
    render();
  } catch (e) {
    showToast(`Save failed: ${e}`, true);
  }
}

async function onCoverChangeClick() {
  if (!metadataBook) return;
  let src;
  try {
    src = await window.api.invoke("library_pick_image");
  } catch (e) {
    showToast(`Image picker failed: ${e}`, true);
    return;
  }
  if (!src) return; // user cancelled

  try {
    const result = await window.api.invoke("library_set_cover", {
      bookId: metadataBook.id,
      srcPath: src,
    });
    if (result.kind === "updated") {
      // Bump the cache-buster so the gallery thumbnail and the modal
      // preview both reload from disk instead of the cached image.
      state.coverCacheBust += 1;
      // Update the in-memory row directly; library:row-updated will
      // also fire and re-apply, but doing it locally avoids a render
      // gap.
      const idx = state.books.findIndex((b) => b.id === metadataBook.id);
      if (idx !== -1) {
        state.books[idx] = { ...state.books[idx], cover_path: result.cover_path };
        metadataBook = state.books[idx];
      }
      renderCoverPreview(metadataBook);
      render();
    } else if (result.kind === "failed") {
      showToast(`Cover change failed: ${result.error}`, true);
    }
  } catch (e) {
    showToast(`Cover change failed: ${e}`, true);
  }
}

// Replace one book in state.books by id. Used by the metadata save path
// and by the library:row-updated event subscriber.
function mergeBookRow(row) {
  const idx = state.books.findIndex((b) => b.id === row.id);
  if (idx !== -1) state.books[idx] = row;
}

function subscribeLibraryRowUpdated() {
  window.api.listen("library:row-updated", (e) => {
    if (!e?.payload) return;
    mergeBookRow(e.payload);
    // If the modal is open on this book, refresh the cover preview so a
    // backend-driven update (e.g. a future bulk recrawl) keeps the UI in
    // sync.
    if (metadataBook && metadataBook.id === e.payload.id) {
      metadataBook = e.payload;
      renderCoverPreview(metadataBook);
    }
    render();
  });
}
