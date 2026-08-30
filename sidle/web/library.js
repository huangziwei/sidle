// Library view: gallery + list, drag-drop, right-click menu, live status.

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));

const state = {
  books: [],
  view: "gallery", // 'gallery' | 'list'
  // Grouping axis, orthogonal to view. 'none' = flat (every book); 'series' =
  // collapse same-series books into a collection you drill into. Persisted.
  group: "series", // 'none' | 'series'
  // When grouped, the series whose contents are being browsed, or null at the
  // top level. Ephemeral navigation — never persisted.
  seriesView: null, // string | null
  // Top-level tab. 'device' is the Kindle page, reached from the upper-right
  // pill, outside the tab strip.
  section: "books", // 'books' | 'notes' | 'misc' (the Files tab) | 'reading' | 'device'
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
    formats: new Set(), // "EPUB" | "PDF" (+ "KFX" in source mode) — see formatFacetMode
  },
  // How the Format facet classifies each book: "companion" is the non-KFX file it has,
  // "source" the format it was imported from.
  formatFacetMode: "companion", // "companion" | "source"
  search: "", // global free-text search across title, author, series, tags
  device: null, // DeviceInfo | null
  sent: [],     // Vec<DeviceBookRow>
  sentSet: new Set(), // sha256s currently on device, derived from `sent`
  // Column order, visibility and widths for the list view live in the shared
  // TableView (`booksTable`), under "columnConfig"/"columnWidths".
  convProgress: {},
  // Book ids whose conversion failed since the failure report was last dismissed, a
  // batch of failures reading as one list. Cleared by `hideErrorReport`; the books keep
  // their `error`.
  convFailures: [],
  // When non-null, an autopull from /dedrm is in progress. `{ done, total }`
  // — surfaced in the status bar and used by renderQueue to know it shouldn't
  // clobber the autopull line with the queue summary.
  autopull: null,
  // When non-null, a foreground import (a drop or an Add) is running, and carries its
  // label.
  importing: null,
  // The in-flight send-to-Kindle batch: one task per book, surfaced in the queue
  // drawer.
  sendQueue: [],
  // When non-null, a long library file op (backup / restore / merge) is running.
  fileop: null,
  // When non-null, books are being removed from the Kindle. `{ done, total,
  deleting: null,
  // True while a Kindle annotation sync (auto-on-connect or manual) is running.
  // Surfaced in the status bar below importing and autopull, which keep a book pull's
  // progress in view.
  annotationSync: false,
  // When non-null, the title of a book currently being opened in the reader.
  // Top priority in the status bar — it's the most immediate user action — and
  // cleared by openReader once the reader is up (or the open fails).
  opening: null,
  // Keyboard focus cursor (Books section) — a stable key for the currently
  focusKey: null,
  // Anchor for Shift+arrow range extension — the book key where the current
  // range began. Cleared by any plain (non-shift) cursor move or selection edit.
  focusAnchorKey: null,
};

// The Books section's multi-select. The Notes section owns a second SelectionController
// of its own.
const booksSelection = new window.SelectionController({
  idAttr: "bookId",
  // The selectable books on screen, in display order: shift-range and select-all scope
  // to what shows. Flat lists every sorted book; the grouped top level lists standalone
  // books alone, leaving a collection unselectable.
  orderedIds: () => displayedSelectableBookIds(),
  // The selectable elements on the visible surface. In List view the grouped
  // top level is the series index, where only standalone (`.book-row`) rows are
  // selectable — series rows are navigation.
  containers: () => {
    if (state.view === "gallery") return $$("#gallery-grid .book-card");
    if (displayMode() === "grouped") return $$("#series-list tbody tr.book-row");
    return $$("#list tbody tr");
  },
  // Paint `.selected` across whichever surface is populated. render() builds
  paintContainers: () => [
    ...$$("#gallery-grid .book-card"),
    ...$$("#list tbody tr"),
    ...$$("#series-list tbody tr.book-row"),
  ],
  lassoEl: () => $("#lasso"),
  skipSelector:
    ".book-card, .book-table tbody tr, .series-table tbody tr, .series-card, .book-table thead, .resizer",
  onChange: () => {
    renderSelectionBar();
    paintSeriesSelection(); // collections reflect "all members selected"
  },
});

// The selection controller for the active section. The lasso and keyboard handlers in
// wireSelection() are written against this, and stay section-agnostic.
function activeController() {
  // The Kindle page, the Files tab and the Reading Log have no selectable items and
  // return no controller. The lasso and the Esc/Cmd-A handlers bail on a null
  // controller, staying inert there.
  if (state.section === "device" || state.section === "misc" || state.section === "reading") {
    return null;
  }
  if (state.section === "notes") {
    return window.Notebooks ? window.Notebooks.selection() : null;
  }
  return booksSelection;
}

// Sort keys exposed in the gallery-visible sort popover and as
// data-sort attrs on the list-view column headers. Order here is the
// order shown in the popover.
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

const FACETS = ["language", "author", "on_kindle", "publisher", "series", "tags", "formats"];

// Display names for the canonical language codes the backend harmonizes to.
const LANGUAGE_NAMES = {
  en: "English",
  ja: "日本語",
  "zh-Hans": "简体中文",
  "zh-Hant": "繁體中文",
  zh: "中文",
  ko: "한국어",
  fr: "Français",
  de: "Deutsch",
  es: "Español",
  it: "Italiano",
  pt: "Português",
  ru: "Русский",
  nl: "Nederlands",
  ar: "العربية",
  he: "עברית",
  hi: "हिन्दी",
  th: "ไทย",
  vi: "Tiếng Việt",
  id: "Bahasa Indonesia",
  ms: "Bahasa Melayu",
  pl: "Polski",
  sv: "Svenska",
  da: "Dansk",
  no: "Norsk",
  fi: "Suomi",
  el: "Ελληνικά",
  cs: "Čeština",
  tr: "Türkçe",
  uk: "Українська",
  ro: "Română",
  hu: "Magyar",
};

// Human-readable name for a language code; "—"/blank stays the em-dash
// placeholder, unknown codes fall back to themselves.
function languageName(code) {
  const c = (code || "").trim();
  if (!c || c === "—") return "—";
  return LANGUAGE_NAMES[c] || c;
}

// The label shown for a facet *value* in the filter dropdown and pill. The stored value
// stays the canonical code that filtering matches on; the language facet alone maps its
// codes to display names.
function facetOptionLabel(facet, value) {
  return facet === "language" ? languageName(value) : value;
}

// The Books list-view column schema. The shared TableView (table.js) renders
const textEdit = (get) => ({ type: "text", get });
const BOOK_COLUMNS = [
  { key: "title",        label: "Title",      sortable: true,  render: (b) => b.title || "Untitled",    edit: textEdit((b) => b.title || "") },
  { key: "author",       label: "Author",     sortable: true,  render: (b) => b.author || "",           edit: textEdit((b) => b.author || "") },
  { key: "series",       label: "Series",     sortable: true,  render: (b) => seriesText(b),            edit: textEdit((b) => seriesText(b)) },
  { key: "publisher",    label: "Publisher",  sortable: true,  render: (b) => b.publisher || "",        edit: textEdit((b) => b.publisher || "") },
  { key: "published_at", label: "Published",  sortable: true,  render: (b) => b.published_at || "",     edit: textEdit((b) => b.published_at || "") },
  { key: "language",     label: "Lang",       sortable: true,  render: (b) => languageName(b.language), edit: { type: "select", get: (b) => b.language || "", options: () => languageOptions() } },
  { key: "tags",         label: "Tags",       sortable: false, render: (b) => (b.tags || []).join(", "), edit: textEdit((b) => (b.tags || []).join(", ")) },
  { key: "imported_at",  label: "Date added", sortable: true,  render: (b) => formatDate(b.imported_at) },
  { key: "file_size",    label: "Size",       sortable: true,  render: (b) => formatBytes(b.file_size) },
  { key: "formats",      label: "Formats",    sortable: false, render: (b) => formatsContent(b) },
  { key: "on_kindle",    label: "On Kindle",  sortable: true,  render: (b) => onKindleContent(b) },
];

// Options for the Language cell's inline <select>: a blank ("—") plus the code→name set
// the cell renders. The editor adds a stored code the list omits, and opening it leaves
// the value alone (see _openEditor).
function languageOptions() {
  return [
    { value: "", label: "—" },
    ...Object.entries(LANGUAGE_NAMES).map(([code, name]) => ({ value: code, label: name })),
  ];
}

function formatsContent(b) {
  const wrap = document.createElement("div");
  wrap.className = "formats";
  // Verbose badges in the list (`KFX · converting` etc.) — see formatStatusFor.
  wrap.appendChild(formatBadge(nonKfxFormat(b), b, /*compact=*/ false));
  wrap.appendChild(formatBadge("kfx", b, /*compact=*/ false));
  return wrap;
}

function onKindleContent(b) {
  const span = document.createElement("span");
  if (state.sentSet.has(b.sha256)) {
    span.className = "on-kindle yes";
    span.textContent = "✓";
    span.title = "On Kindle";
  } else {
    span.className = "on-kindle no";
    span.textContent = "—";
  }
  return span;
}

// The Books list view. Sort lives in `state.sort`, shared with the gallery; the table
// renders the indicator and reports header clicks through onSort.
const booksTable = new window.TableView({
  table: document.querySelector("#list table"),
  columns: BOOK_COLUMNS,
  idOf: (b) => b.id,
  idAttr: "bookId",
  configKey: "columnConfig",
  widthsKey: "columnWidths",
  getSort: () => state.sort,
  onSort: (key) => {
    if (state.sort.key === key) state.sort.asc = !state.sort.asc;
    else state.sort = { key, asc: true };
    persistPreferences();
    render();
  },
  isSelected: (id) => booksSelection.has(id),
  onRowClick: (e, b) => onItemClick(e, b),
  onRowDblClick: (b) => openReader(b),
  onRowContext: (e, b) => {
    onItemContext(e, b);
    openContextMenu(e.clientX, e.clientY, b);
  },
  onChange: () => render(),
  ctxMenu: document.querySelector("#ctx-menu"),
  // Click-to-edit: only when this row is the sole selection (a click on an
  // unselected/multi-selected row selects first; a second click then edits).
  canEditNow: (id) => booksSelection.count() === 1 && booksSelection.has(id),
  onCellEdit: (b, key, value) => commitInlineEdit(b, key, value),
});

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
  wireServer();
  // Header interactions (sort, resize, drag-to-reorder, visibility menu)
  // are wired by renderList() itself since the thead is rebuilt on every
  // render — no separate boot call needed.
  wireSelection();
  wireGalleryDelegation();
  wireFilterBar();
  wireSortPopover();
  wireMetadataModal();
  wireSplitModal();
  wireLibraryShortcuts();
  await refresh();
  subscribeStatus();
  subscribeImportProgress();
  subscribeDeviceStatus();
  refreshServerStatus();
  subscribeSendProgress();
  subscribeSendActive();
  subscribeFileopProgress();
  subscribePullProgress();
  subscribeAnnotationSync();
  subscribeLibraryRowUpdated();
  subscribeRecrawlProgress();
  // Populate the Notes tab where boot left off in it.
  if (state.section === "notes" && window.Notebooks) {
    window.Notebooks.setView(state.view);
    window.Notebooks.show();
  }
  // Likewise for the Files tab (what a Sync backs up off the Kindle).
  if (state.section === "misc") window.Misc?.show();
  // And the Reading Log, which loads its overview on first show.
  if (state.section === "reading") window.ReadingLog?.show();
  // And Apps, which reads the composed fleet on first show.
  if (state.section === "apps") window.Apps?.show();
});

function loadPreferences() {
  const view = localStorage.getItem("view");
  if (view === "list") state.view = "list";
  // Grouping defaults to "series" (2026-06). `groupMode` is the key read; the line
  // below drops the abandoned `group` key.
  const group = localStorage.getItem("groupMode");
  if (group === "none" || group === "series") state.group = group;
  localStorage.removeItem("group"); // drop the abandoned pre-flip key
  const sort = localStorage.getItem("sort");
  if (sort) {
    try {
      state.sort = { ...state.sort, ...JSON.parse(sort) };
    } catch {
      // malformed JSON in localStorage — keep the default
    }
  }
  // Column order/visibility + widths are loaded by the booksTable TableView from
  // the same "columnConfig"/"columnWidths" keys (no separate state here).
  const filters = localStorage.getItem("filters");
  if (filters) {
    try {
      const parsed = JSON.parse(filters) || {};
      for (const facet of FACETS) {
        if (Array.isArray(parsed[facet])) {
          state.filters[facet] = new Set(parsed[facet]);
        }
      }
    } catch {
      // malformed JSON in localStorage — keep the default
    }
  }
  const fmtMode = localStorage.getItem("formatFacetMode");
  if (fmtMode === "source" || fmtMode === "companion") state.formatFacetMode = fmtMode;
  const search = localStorage.getItem("search");
  if (typeof search === "string") state.search = search;
  // Every section setSection persists (i.e. all but the transient Kindle page)
  const section = localStorage.getItem("section");
  if (
    section === "notes" ||
    section === "misc" ||
    section === "reading" ||
    section === "apps"
  ) {
    state.section = section;
  }
  applySection();
}

function persistPreferences() {
  localStorage.setItem("view", state.view);
  localStorage.setItem("groupMode", state.group);
  localStorage.setItem("sort", JSON.stringify(state.sort));
  const filtersForStorage = {};
  for (const facet of FACETS) {
    filtersForStorage[facet] = [...state.filters[facet]];
  }
  localStorage.setItem("filters", JSON.stringify(filtersForStorage));
  localStorage.setItem("formatFacetMode", state.formatFacetMode);
  localStorage.setItem("search", state.search);
  // booksTable persists "columnConfig"/"columnWidths" itself on reorder/resize.
}

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

function wireToolbar() {
  $("#btn-add").addEventListener("click", onAddClick);
  $("#view-gallery").addEventListener("click", () => setView("gallery"));
  $("#view-list").addEventListener("click", () => setView("list"));
  $("#group-none").addEventListener("click", () => setGroup("none"));
  $("#group-series").addEventListener("click", () => setGroup("series"));
  $("#series-back").addEventListener("click", () => exitSeries());
  $("#section-books").addEventListener("click", () => setSection("books"));
  $("#section-notes").addEventListener("click", () => setSection("notes"));
  $("#section-misc").addEventListener("click", () => setSection("misc"));
  $("#section-apps").addEventListener("click", () => setSection("apps"));
  $("#section-reading").addEventListener("click", () => setSection("reading"));
  $("#btn-notes-import").addEventListener("click", () => {
    if (window.Notebooks) window.Notebooks.importDevice();
  });
  $("#error-report-close").addEventListener("click", hideErrorReport);

  $("#btn-settings").addEventListener("click", openSettings);
  $("#settings-close").addEventListener("click", closeSettings);
  $("#settings-modal .modal-backdrop").addEventListener("click", closeSettings);
  $("#settings-move").addEventListener("click", () => pickRelocate("move"));
  $("#settings-use").addEventListener("click", () => pickRelocate("use"));
  $("#settings-backup").addEventListener("click", doBackup);
  $("#settings-restore").addEventListener("click", pickRestore);
  $("#settings-merge").addEventListener("click", doMerge);
  $("#settings-confirm-cancel").addEventListener("click", resetRelocateConfirm);
  // Arrow-wrapped, past a reference: the listener's first argument is the click
  // event, which reads as a truthy "keep a copy".
  $("#settings-confirm-ok").addEventListener("click", () => confirmRelocate(false));
  $("#settings-confirm-keep").addEventListener("click", () => confirmRelocate(true));
}

async function onAddClick() {
  const paths = await window.api.openFileDialog();
  if (!paths || paths.length === 0) return;
  await importPaths(paths);
}

function setView(v) {
  state.view = v;
  // render() builds the active surface; the other view's DOM is cleared on each pass.
  render();
  persistPreferences();
}

function applyView() {
  const books = state.section === "books";
  const mode = displayMode(); // 'flat' | 'grouped' | 'series' (books-only)
  // Content visibility is driven by `.view.active { display: block }`, an author rule
  // that OVERRIDES the `hidden` attribute (`[hidden]` is a UA rule). The `active`
  // class, never `hidden`, shows and hides a section.
  const galleryActive = books && state.view === "gallery";
  // In List view the grouped TOP level shows the lightweight series index
  // (#series-list); flat and drilled-into-a-series both use the book table.
  const seriesIndexActive = books && state.view === "list" && mode === "grouped";
  const listActive = books && state.view === "list" && mode !== "grouped";
  // Toolbar toggle buttons reflect the chosen view regardless of section.
  $("#view-gallery").classList.toggle("active", state.view === "gallery");
  $("#view-list").classList.toggle("active", state.view === "list");
  $("#view-gallery").setAttribute("aria-selected", String(state.view === "gallery"));
  $("#view-list").setAttribute("aria-selected", String(state.view === "list"));
  // Grouping toggle reflects state.group. It lives in the filter bar, which
  // applySection() hides wholesale on the Notes tab — no per-control hiding here.
  $("#group-none").classList.toggle("active", state.group === "none");
  $("#group-series").classList.toggle("active", state.group === "series");
  $("#group-none").setAttribute("aria-selected", String(state.group === "none"));
  $("#group-series").setAttribute("aria-selected", String(state.group === "series"));
  $("#gallery").classList.toggle("active", galleryActive);
  $("#list").classList.toggle("active", listActive);
  $("#series-list").classList.toggle("active", seriesIndexActive);
  $("#gallery").hidden = !galleryActive;
  $("#list").hidden = !listActive;
  $("#series-list").hidden = !seriesIndexActive;
  // The Gallery/List toggle drives the Notes section too, handing the choice to
  // notebooks.js for its grid/table swap. A no-op while Notes is hidden.
  if (window.Notebooks) window.Notebooks.setView(state.view);
  if (window.Apps) window.Apps.setView(state.view);
  // Re-apply the keyboard focus ring to its tile. applyView runs on every render and on
  // a bare view switch that rebuilds no DOM, keeping the cursor visible across both,
  // and across the gallery and list DOM.
  paintFocus();
}

// Switch the grouping axis (flat ⇄ by-series). Resets any drill-in and the
// selection (it's scoped to the on-screen set, which is about to change).
function setGroup(g) {
  if (state.group === g) return;
  state.group = g;
  state.seriesView = null;
  clearSelection();
  persistPreferences();
  render();
}

// The library section to fall back to when leaving the Kindle page; the pill toggles
// device ⇄ here. Any section but 'device', the one place the pill is pressed from and
// never the way back.
let lastLibrarySection = "books";

// One scroll container (#main) holds every section, and swapping which one is shown
// carries its offset here, keyed by page.
const scrollMarks = new Map();

// Which page the scroller is showing. Inside Books a series drill-in is its own
// page with its own offset; everywhere else the section is the page.
function scrollKey() {
  return state.section === "books" && state.seriesView != null
    ? `series:${state.seriesView}`
    : state.section;
}

function parkScroll(key) {
  scrollMarks.set(key, $("#main").scrollTop);
}

function restoreScroll(key) {
  $("#main").scrollTop = scrollMarks.get(key) || 0;
}

// Top-level Books / Notes / Files / Kindle split. Books and Notes share the gallery and
// list surfaces; the others bring their own.
function setSection(s) {
  // Re-picking the open section is no navigation, and leaves the scroller where
  // the reader put it.
  const moving = state.section !== s;
  if (moving) parkScroll(scrollKey());
  state.section = s;
  // Drill-in is a Books-only navigation; don't carry it across the tab switch.
  state.seriesView = null;
  if (s !== "device") {
    lastLibrarySection = s;
    localStorage.setItem("section", s);
  }
  // Leaving Books, for Notes or the Kindle page, drops the book selection, and the
  // Books selection bar clears off the other surface.
  if (s !== "books") clearSelection();
  applySection();
  if (window.Notebooks) {
    if (s === "notes") window.Notebooks.show();
    else window.Notebooks.hide();
  }
  if (window.Misc) {
    if (s === "misc") window.Misc.show();
    else window.Misc.hide();
  }
  if (window.ReadingLog) {
    if (s === "reading") window.ReadingLog.show();
    else window.ReadingLog.hide();
  }
  if (window.Apps) {
    if (s === "apps") window.Apps.show();
    else window.Apps.hide();
  }
  if (s === "device") {
    // Entering the Kindle page: re-pull device / deploy / LAN state. It lives here,
    // outside the pill handler, and the `\` shortcut refreshes too.
    refreshDevicePage();
  } else if (s === "books") {
    // Re-render the book views on return: a device refresh reaches rows the Kindle page
    // left stale.
    render();
  }
  // Last: the arriving section's content, and its height, are in place before the
  // offset applies.
  if (moving) restoreScroll(scrollKey());
}

function applySection() {
  const notes = state.section === "notes";
  const misc = state.section === "misc";
  const reading = state.section === "reading";
  const device = state.section === "device";
  const books = state.section === "books";
  const apps = state.section === "apps";
  // The Kindle page, Files and the Reading Log collapse the toolbar to just the
  // section tabs. Apps keeps the Gallery/List toggle and drops the filter with
  // the rest.
  const bare = device || misc || reading;
  const unfiltered = notes || apps || bare;
  // Section tabs light up for their own section. The Kindle page activates none
  // of Books/Notes/Files; the upper-right pill carries that state.
  $("#section-books").classList.toggle("active", books);
  $("#section-notes").classList.toggle("active", notes);
  $("#section-misc").classList.toggle("active", misc);
  $("#section-apps").classList.toggle("active", apps);
  $("#section-books").setAttribute("aria-selected", String(books));
  $("#section-notes").setAttribute("aria-selected", String(notes));
  $("#section-misc").setAttribute("aria-selected", String(misc));
  $("#section-apps").setAttribute("aria-selected", String(apps));
  // Reading Log is a plain toggle button outside that list, and carries `aria-pressed`;
  // `aria-selected` is meaningful on a `role="tab"` alone.
  $("#section-reading").classList.toggle("active", reading);
  $("#section-reading").setAttribute("aria-pressed", String(reading));
  // The pill doubles as the Kindle-page tab — mark it active there.
  $("#device-pill").classList.toggle("active", device);
  // Add… (Books only) and Import (Notes only); both gone elsewhere.
  $("#btn-add").hidden = !books;
  $("#btn-notes-import").hidden = !notes;
  // Notes keeps the Gallery/List toggle and separators. The Kindle page and Files hide
  // the view toggle and both toolbar separators (the section↔view one and #view-sep),
  // collapsing the toolbar-group to the section tabs.
  $("#filter-bar").hidden = unfiltered;
  const search = document.querySelector(".filter-search");
  if (search) search.hidden = unfiltered;
  $("#view-seg").hidden = bare;
  $("#view-sep").hidden = bare;
  // Addressed by id: a second .toolbar-sep sits ahead of this one, and DOM order
  // identifies neither.
  const sectionSep = $("#section-view-sep");
  if (sectionSep) sectionSep.hidden = bare;
  // `#notes` / `#misc` / `#device-page` use the same `.view`/`.view.active`
  // system: the `active` class (not `hidden`) is what `display: block`s them.
  // applyView() owns the book views, which stay inactive when section isn't Books.
  $("#notes").classList.toggle("active", notes);
  $("#notes").hidden = !notes;
  $("#misc").classList.toggle("active", misc);
  $("#misc").hidden = !misc;
  $("#reading-log").classList.toggle("active", reading);
  $("#reading-log").hidden = !reading;
  $("#device-page").classList.toggle("active", device);
  $("#device-page").hidden = !device;
  $("#apps").classList.toggle("active", apps);
  $("#apps").hidden = !apps;
  applyView();
}

// ---------------------------------------------------------------------------
// Drag and drop
// ---------------------------------------------------------------------------

// True while an in-flight OS drag carries at least one image file. Latched on `enter`,
// the one phase before `drop` that reports paths, and `over` lights the cover drop
// target without re-sniffing.
let dragHasImage = false;

// The cover preview element while it accepts a dropped image, with the
// single-book metadata editor open. `null` for a drop falling through to the
// normal library import.
function coverDropTarget() {
  const modal = $("#metadata-modal");
  if (!modal || modal.hidden || !metadataBook || metadataBulk) return null;
  return $("#metadata-cover-preview");
}

function isImagePath(p) {
  return /\.(jpe?g|png|webp)$/i.test(p || "");
}

function wireDragDrop() {
  const veil = $("#drop-veil");
  window.api.onDragDrop((event) => {
    const payload = event.payload || {};
    const t = payload.type;

    // Cover drop target: while the single-book editor is open, an image dropped
    const coverBox = coverDropTarget();
    if (coverBox) {
      if (t === "enter") dragHasImage = (payload.paths || []).some(isImagePath);
      if ((t === "enter" || t === "over") && dragHasImage) {
        veil.hidden = true; // keep the editor focused; no full-screen veil
        // Arm the preview at any cursor position: the drop is accepted anywhere on the
        // open editor, and the lit target says "this image becomes the cover", dodging
        // window-vs-webview coord skew.
        coverBox.classList.add("drag-over");
        return;
      }
      if (t === "leave") {
        coverBox.classList.remove("drag-over");
        dragHasImage = false;
      }
      if (t === "drop") {
        coverBox.classList.remove("drag-over");
        dragHasImage = false;
        const img = (payload.paths || []).find(isImagePath);
        if (img) {
          veil.hidden = true;
          applyCoverFromPath(img);
          return;
        }
        // no image in the drop → fall through to import handling
      }
    }

    if (t === "enter" || t === "over") {
      veil.hidden = false;
    } else if (t === "leave") {
      veil.hidden = true;
    } else if (t === "drop") {
      veil.hidden = true;
      const paths = payload.paths || [];
      const accepted = paths.filter((p) => {
        const lower = p.toLowerCase();
        return (
          lower.endsWith(".epub") ||
          lower.endsWith(".kfx") ||
          lower.endsWith(".kfx-zip") ||
          lower.endsWith(".azw3") ||
          lower.endsWith(".mobi") ||
          // PDF → wrapped into a fixed-layout PDOC KFX for the Scribe (the
          // device renders the PDF; the pen draws over it).
          lower.endsWith(".pdf") ||
          // Plain .zip is accepted: an Aozora Bunko archive arrives under that
          // extension.
          lower.endsWith(".zip")
        );
      });
      if (accepted.length === 0) {
        showToast("only .epub, .kfx, .kfx-zip, .azw3, .mobi, .pdf are supported", true);
        return;
      }
      importPaths(accepted);
    }
  });
}

// ---------------------------------------------------------------------------
// Drag a book onto a series tile to file it there
// ---------------------------------------------------------------------------

const BOOK_DRAG_THRESHOLD = 4; // px a mousedown must travel before it's a drag

// The series tile lit as the drop target, cleared as the cursor moves off. Reset to
// null with no drag in flight.
let seriesDropTargetEl = null;

// Wire a book tile/row as a drag source. `book` is its library row.
function wireBookDragSource(el, book) {
  el.addEventListener("mousedown", (e) => onBookDragDown(e, el, book));
}

function onBookDragDown(e, el, book) {
  if (e.button !== 0) return; // primary button only
  // Modifier = select/toggle/range; let the click handler own it, no drag.
  if (e.metaKey || e.ctrlKey || e.shiftKey) return;
  e.preventDefault(); // suppress text selection; the click still fires for select

  const startX = e.clientX;
  const startY = e.clientY;
  // Grab the whole selection where the book is part of it, and this one alone
  // where the selection is unrelated.
  const ids = booksSelection.has(book.id) ? booksSelection.ids() : [book.id];

  let dragging = false;
  let ghost = null;

  const onMove = (ev) => {
    if (!dragging) {
      if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < BOOK_DRAG_THRESHOLD) return;
      dragging = true;
      el.classList.add("drag-source");
      ghost = document.createElement("div");
      ghost.className = "book-drag-ghost";
      ghost.textContent = `${ids.length} book${ids.length === 1 ? "" : "s"}`;
      document.body.appendChild(ghost);
      document.body.style.cursor = "grabbing";
    }
    ghost.style.left = `${ev.clientX + 10}px`;
    ghost.style.top = `${ev.clientY + 10}px`;
    paintSeriesDropTarget(seriesTileAt(ev.clientX, ev.clientY));
  };

  const onUp = (ev) => {
    document.removeEventListener("mousemove", onMove);
    document.removeEventListener("mouseup", onUp);
    document.body.style.cursor = "";
    if (ghost) ghost.remove();
    el.classList.remove("drag-source");
    paintSeriesDropTarget(null);
    if (!dragging) return; // never crossed the threshold — a plain click

    el.classList.add("just-dragged"); // swallow the trailing synthetic click
    // Drop it again past that click's moment, holding an aborted drag —
    // released over nothing, with no re-render — off the next real click.
    setTimeout(() => el.classList.remove("just-dragged"), 0);
    const target = seriesTileAt(ev.clientX, ev.clientY);
    if (target) assignBooksToSeries(ids, target.dataset.series);
  };

  document.addEventListener("mousemove", onMove);
  document.addEventListener("mouseup", onUp);
}

// The series drop target under a point: a gallery `.series-card` or a grouped
// list series row (`#series-list tr[data-series]`). null when over neither.
function seriesTileAt(x, y) {
  const el = document.elementFromPoint(x, y);
  return el ? el.closest(".series-card[data-series], #series-list tbody tr[data-series]") : null;
}

// Light exactly one series tile as the live drop target, clearing the previous.
function paintSeriesDropTarget(el) {
  if (seriesDropTargetEl === el) return;
  if (seriesDropTargetEl) seriesDropTargetEl.classList.remove("series-drop-target");
  seriesDropTargetEl = el;
  if (el) el.classList.add("series-drop-target");
}

// Set `series_name` on the dropped books through the bulk-metadata command the
// editor uses, then merge the returned rows and re-render once. A book in the
// series is skipped, and the toast count states what moved.
async function assignBooksToSeries(bookIds, seriesName) {
  const name = (seriesName || "").trim();
  if (!name || !bookIds || bookIds.length === 0) return;
  const targets = bookIds.filter((id) => {
    const b = state.books.find((x) => x.id === id);
    return b && seriesNameOf(b) !== name;
  });
  if (targets.length === 0) {
    showToast(`Already in "${name}".`);
    return;
  }
  try {
    const rows = await window.api.invoke("library_bulk_update_metadata", {
      bookIds: targets,
      patch: { series_name: name },
    });
    for (const r of rows) mergeBookRow(r);
    // The books collapse into the series tile, showing individually nowhere,
    // and drop out of the selection.
    if (targets.some((id) => booksSelection.has(id))) booksSelection.clear();
    render();
    showToast(`Added ${rows.length} book${rows.length === 1 ? "" : "s"} to "${name}".`);
  } catch (e) {
    showToast(`Couldn't add to series: ${e}`, true);
  }
}

async function importPaths(paths) {
  startImportStatus(paths);

  let imported = 0;
  let dupes = 0;
  const failures = [];
  try {
    const results = await window.api.invoke("library_import", { paths });
    for (const r of results) {
      if (r.kind === "imported") imported++;
      else if (r.kind === "duplicate") dupes++;
      else if (r.kind === "failed") {
        failures.push({ path: r.path, error: r.error });
        console.error("import failed:", r.path, r.error);
      }
    }
  } catch (e) {
    showToast(`import error: ${e}`, true);
    showImportFailure();
    return;
  }

  const failed = failures.length;
  // Status-bar failure mode is the all-failed case (status bar is the
  // single-line summary; the detail panel below carries the per-file reason).
  if (failed > 0 && imported === 0 && dupes === 0) {
    showImportFailure();
  } else {
    clearImportStatus();
  }

  await refresh();

  // The toast is only a count; show a persistent, dismissable report with the
  // actual reason each file failed (anything imported successfully clears any
  // stale report). Failures from a previous run shouldn't linger over a clean one.
  if (failed > 0) {
    showImportErrorReport(failures);
  } else {
    hideErrorReport();
  }

  const parts = [];
  if (imported) parts.push(`${imported} imported`);
  if (dupes) parts.push(`${dupes} already in library`);
  if (failed) parts.push(`${failed} failed`);
  if (parts.length) showToast(parts.join(" · "), failed > 0);
}

// Persistent failure report. The detailed `error` from Rust (the full anyhow
function showErrorReport(title, entries) {
  const panel = $("#error-report");
  const list = $("#error-report-list");
  const heading = $("#error-report-title");
  if (!panel || !list) return;

  heading.textContent = title;
  list.innerHTML = "";
  for (const entry of entries) {
    const li = document.createElement("li");
    const name = document.createElement("div");
    name.className = "error-report-item";
    name.textContent = entry.name || "(unknown)";
    const reason = document.createElement("pre");
    reason.className = "error-report-reason";
    reason.textContent = entry.reason || "Unknown error";
    li.append(name, reason);
    if (entry.onRetry) {
      const retry = document.createElement("button");
      retry.className = "error-report-retry";
      retry.textContent = "Retry";
      retry.addEventListener("click", () => {
        entry.onRetry();
        hideErrorReport();
      });
      li.appendChild(retry);
    }
    list.appendChild(li);
  }
  panel.hidden = false;
}

function hideErrorReport() {
  const panel = $("#error-report");
  if (panel) panel.hidden = true;
  state.convFailures = [];
}

function showImportErrorReport(failures) {
  showErrorReport(
    failures.length === 1 ? "Import failed" : `${failures.length} imports failed`,
    failures.map((f) => ({
      name: (f.path || "").split(/[\\/]/).pop() || f.path || "(unknown file)",
      reason: f.error,
    })),
  );
}

// Why a conversion failed, with its Retry one click away. Reached from the persistent
// failure report.
function showConversionErrorReport(bookIds) {
  const books = bookIds
    .map((id) => state.books.find((b) => b.id === id))
    .filter(Boolean);
  if (!books.length) return;
  showErrorReport(
    books.length === 1 ? "Conversion failed" : `${books.length} conversions failed`,
    books.map((b) => ({
      name: b.title || "Untitled",
      reason: b.error,
      onRetry: () => retryConvert(b.id),
    })),
  );
}

function startImportStatus(paths) {
  state.importing = { message: importInitialMessage(paths), failed: false };
  render();
}

function clearImportStatus() {
  state.importing = null;
  render();
}

function showImportFailure() {
  state.importing = { message: "Import failed", failed: true };
  render();
  // Lingers a beat, then falls back to the queue summary.
  setTimeout(() => {
    if (state.importing && state.importing.failed) {
      state.importing = null;
      render();
    }
  }, 3000);
}

function importInitialMessage(paths) {
  if (paths.length === 1) return `Importing ${importFormatName(paths[0])}…`;
  return `Importing ${paths.length} file${paths.length === 1 ? "" : "s"}…`;
}

// What to call the format at `path` while it imports. The extension is the whole of the
// evidence, and the backend dispatches on it too.
function importFormatName(path) {
  const lower = (path || "").toLowerCase();
  if (lower.endsWith(".zip")) return "Aozora zip";
  if (lower.endsWith(".epub")) return "EPUB";
  if (lower.endsWith(".kfx-zip")) return "KFX bundle";
  if (lower.endsWith(".kfx")) return "KFX";
  if (lower.endsWith(".azw3")) return "AZW3";
  if (lower.endsWith(".mobi")) return "MOBI";
  if (lower.endsWith(".pdf")) return "PDF";
  return "book";
}

// Live progress for the file currently being imported. Only the formats
// converted during the import report these — see the `importing` state comment.
function subscribeImportProgress() {
  window.api.listen("library:import-progress", (e) => {
    const { path, index, total, fraction, label } = e.payload || {};
    // A tick outliving its import, past the command's resolve, leaves the status
    // line down.
    if (!state.importing || state.importing.failed) return;
    const batch = total > 1 ? ` (${(index ?? 0) + 1}/${total})` : "";
    state.importing = {
      ...state.importing,
      message: `Importing ${importFormatName(path)}${batch}`,
      name: fileNameOf(path),
      fraction: fraction ?? 0,
      label: label || "",
    };
    if (!updateImportRowProgress()) renderQueue();
  });
}

// The last path segment, for naming an import row after the file it came from.
function fileNameOf(path) {
  return (path || "").split(/[\\/]/).pop() || path || "";
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
  // Only `sent` rows carry a full sha256 to intersect with the local library.
  state.sentSet = new Set(
    state.sent.filter((r) => r.kind === "sent").map((r) => r.sha256),
  );
}

function render() {
  // Prune the selection of books a refresh has dropped.
  booksSelection.prune(new Set(state.books.map((b) => b.id)));
  const visible = sortedBooks(visibleBooks(state.books));

  // Drop a drill-in whose series holds no visible book — filtered out, removed,
  // or its series_name edited away — leaving no empty view to strand on.
  if (
    state.seriesView != null &&
    !visible.some((b) => seriesNameOf(b) === state.seriesView)
  ) {
    state.seriesView = null;
  }

  // Grouping is a presentation layer on the filtered, sorted
  const mode = displayMode();
  const galleryView = state.view === "gallery";
  let count;
  if (mode === "grouped") {
    const entries = groupBySeries(visible);
    count = entries.length;
    if (galleryView) {
      renderGalleryGrouped(entries); // #gallery-grid (active)
      renderSeriesList([]); // #series-list (inactive → clear)
    } else {
      renderSeriesList(entries); // #series-list = the grouped-list series index
      renderGalleryGrouped([]); // #gallery-grid (inactive → clear)
    }
    renderList([]); // #list unused at the grouped top level
  } else {
    const books =
      mode === "series" ? membersOfSeries(visible, state.seriesView) : visible;
    count = books.length;
    if (galleryView) {
      renderGallery(books); // #gallery-grid (active)
      renderList([]); // #list (inactive → clear)
    } else {
      renderList(books); // #list (active)
      renderGallery([]); // #gallery-grid (inactive → clear)
    }
    renderSeriesList([]); // #series-list unused when flat / drilled-in
  }

  // Breadcrumb: only while drilled into a series.
  $("#series-crumb").hidden = mode !== "series";
  if (mode === "series") {
    $("#series-crumb-name").textContent = `${state.seriesView} (${count})`;
  }

  applyView(); // sync which container is visible to the current mode
  renderQueue();
  renderSelectionBar();
  paintSeriesSelection(); // re-mark selected collections on the rebuilt tiles
  updateSendUnsentButton();
  // `#gallery-empty`, `#list-empty` and `#series-list-empty` hide at a `count` above
  // zero.
  $("#gallery-empty").hidden = count > 0;
  $("#list-empty").hidden = count > 0;
  $("#series-list-empty").hidden = count > 0;
  renderFilterBar();
  renderSortControl();
  if (state.view === "list") {
    requestAnimationFrame(() => {
      booksTable.ensureWidths();
    });
  }
}

// Natural-order string collation: a digit run compares as a number, ordering "Vol 2"
// ahead of "Vol 10".
const naturalCollator = new Intl.Collator(undefined, { numeric: true });
function naturalCompare(a, b) {
  return naturalCollator.compare(a, b);
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
    return naturalCompare(String(av), String(bv)) * dir;
  });
}

function sortValue(b, key) {
  if (key === "on_kindle") return state.sentSet.has(b.sha256) ? 1 : 0;
  if (key === "series") return seriesSortKey(b);
  return b[key];
}

// Composite sort key for the Series column: primary by series_name,
function seriesSortKey(b) {
  const name = b.series_name?.trim();
  if (!name) return null;
  // Scaled by 10000: a sub-volume down to four decimals (5.1, 5.25, …) keeps its place
  // in the padded key.
  const rawIdx =
    b.series_index != null && Number.isFinite(b.series_index)
      ? Math.round(b.series_index * 10000)
      : 9_999_999_999_999;
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
// Series grouping (flat ⇄ collections). Pure presentation over the filtered,
// sorted `visible` list — see render().

// A book's series identity, or null when it has none (→ stays standalone).
function seriesNameOf(b) {
  return b.series_name?.trim() || null;
}

// What the Books section shows.
//   'flat'    — every book individually (also the Notes section)
//   'grouped' — series collections + standalone books
function displayMode() {
  if (state.section !== "books" || state.group !== "series") return "flat";
  return state.seriesView != null ? "series" : "grouped";
}

// Canonical within-series order: by series_index ascending (half-numbers like
// 1.5 sort correctly), books without an index after those with one, then title.
function bySeriesIndex(a, b) {
  const ai = a.series_index;
  const bi = b.series_index;
  const an = ai != null && Number.isFinite(ai);
  const bn = bi != null && Number.isFinite(bi);
  if (an && bn && ai !== bi) return ai - bi;
  if (an !== bn) return an ? -1 : 1;
  // No series index, or an equal one, falls back to the title in natural order, lining
  // "Vol 1 … Vol 9, Vol 10, Vol 11" up without a hand-entered index.
  return naturalCompare(a.title || "", b.title || "");
}

function membersOfSeries(books, name) {
  return books.filter((b) => seriesNameOf(b) === name).sort(bySeriesIndex);
}

// Fold the sorted list into entries: a series collection appears at its
// first-seen member's position, and the active sort drives tile/row order
// (Title → first book alphabetically; Date added → newest member;
function groupBySeries(sortedVisible) {
  const out = [];
  const seen = new Map();
  for (const b of sortedVisible) {
    const s = seriesNameOf(b);
    if (!s) {
      out.push({ type: "book", book: b });
      continue;
    }
    let g = seen.get(s);
    if (!g) {
      g = { type: "series", name: s, books: [] };
      seen.set(s, g);
      out.push(g);
    }
    g.books.push(b);
  }
  return out;
}

// The selectable books on screen, in display order. A collection is never selectable,
// and the grouped top level yields standalone books alone.
function displayedSelectableBookIds() {
  const visible = sortedBooks(visibleBooks(state.books));
  const mode = displayMode();
  if (mode === "series") {
    return membersOfSeries(visible, state.seriesView).map((b) => b.id);
  }
  if (mode === "grouped") {
    return groupBySeries(visible)
      .filter((e) => e.type === "book")
      .map((e) => e.book.id);
  }
  return visible.map((b) => b.id);
}

// Subtitle under a collection: the shared author if every member agrees, else a
// book count.
function seriesSubtitle(entry) {
  const authors = new Set(
    entry.books.map((b) => (b.author || "").trim()).filter(Boolean),
  );
  if (authors.size === 1) return [...authors][0];
  const n = entry.books.length;
  return `${n} book${n === 1 ? "" : "s"}`;
}

// Drill into a series' contents (from a collection tile or series-index row).
function enterSeries(name) {
  parkScroll(scrollKey());
  state.seriesView = name;
  clearSelection(); // selection is scoped to the on-screen set, which changes
  render();
  restoreScroll(scrollKey());
}

// Back out to the grouped top level.
function exitSeries() {
  parkScroll(scrollKey());
  state.seriesView = null;
  clearSelection();
  render();
  restoreScroll(scrollKey());
}

// ---------------------------------------------------------------------------
// Filter algorithm (cascading facets + free-text search)

function extractFacetValues(book, facet) {
  switch (facet) {
    case "language":
      return [book.language?.trim() || "—"];
    case "author": {
      const trimmed = (book.author || "").trim();
      if (!trimmed) return ["—"];
      // " & " (multi-author join) OR CJK ideographic comma 「、」. A plain comma
      // stays inside the name — it's the "Surname, Given" separator, not a join.
      const parts = trimmed.split(/\s*[&、]\s*/).filter(Boolean);
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
    case "formats":
      // `state.formatFacetMode` chooses the classification: "source" is the format the
      // book was imported from, "companion" the non-KFX file it has (EPUB|PDF). KFX is
      // universal and is no companion option.
      return [
        (state.formatFacetMode === "source" ? sourceFormat(book) : nonKfxFormat(book)).toUpperCase(),
      ];
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

// Canonical match form, mirroring sidle-core's `romaji::canon` (the fold behind
function canonSearch(s) {
  const folded = (s || "")
    .replace(/[ßẞ]/g, "ss")
    .replace(/[œŒøØ]/g, "oe")
    .replace(/[æÆ]/g, "ae")
    .normalize("NFKD")
    .toLowerCase();
  if (/(?![a-z0-9])[\p{L}\p{N}]/u.test(folded)) return "";
  return folded.replace(/[^a-z0-9]/g, "");
}

// The query-side folds, computed ONCE per filter pass and reused for every book
function searchTerms() {
  const raw = state.search.normalize("NFKC").trim().toLowerCase();
  return { raw, q: raw ? canonSearch(state.search) : "" };
}

function matchesSearch(book, terms) {
  const { raw, q } = terms;
  if (!raw) return true;
  // (1) Romaji-aware match against `search_key`, which the backend folded the same way
  // `canon` folds the query.
  if (q && book.search_key && book.search_key.includes(q)) return true;
  // (2) Raw native-text match. `canon` strips CJK to nothing and matches no CJK title;
  // the raw fields below carry it.
  const hay = [
    book.title,
    book.author,
    book.publisher,
    book.series_name,
    book.title_romaji,
    book.author_romaji,
    ...(book.tags || []),
  ]
    .filter(Boolean)
    .join(" ")
    .normalize("NFKC")
    .toLowerCase();
  return hay.includes(raw);
}

function visibleBooks(books) {
  const facets = activeFacetsExcept(null);
  const terms = searchTerms();
  return books.filter((b) => matchesSearch(b, terms) && matchesFacets(b, facets));
}

function facetOptions(facet) {
  const others = activeFacetsExcept(facet);
  const terms = searchTerms();
  const counts = new Map();
  for (const b of state.books) {
    if (!matchesSearch(b, terms)) continue;
    if (!matchesFacets(b, others)) continue;
    for (const v of extractFacetValues(b, facet)) {
      counts.set(v, (counts.get(v) || 0) + 1);
    }
  }
  // Always include this pill's selected values, at a count of 0, and a selection stays
  // visible in its own dropdown.
  for (const v of state.filters[facet]) {
    if (!counts.has(v)) counts.set(v, 0);
  }
  return [...counts.entries()].sort((a, b) => {
    if (a[0] === "—") return 1;
    if (b[0] === "—") return -1;
    return naturalCompare(a[0], b[0]);
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

// Switch the Format facet between "companion" (non-KFX file present) and
// "source" (imported-from format). KFX is valid in source mode alone, and
// leaving source prunes a selected "KFX", which filters to zero.
function setFormatFacetMode(mode) {
  if (mode === state.formatFacetMode) return;
  state.formatFacetMode = mode;
  if (mode !== "source") state.filters.formats.delete("KFX");
  persistPreferences();
  if (openDropdownFacet === "formats") renderDropdownOptions("formats");
  render();
}

// One set of listeners on the gallery grid, past ~4 per card × N books:
function wireGalleryDelegation() {
  const grid = $("#gallery-grid");

  const hit = (e) => {
    const card = e.target.closest(".book-card");
    if (!card || !grid.contains(card)) return null;
    const book = state.books.find((b) => b.id === Number(card.dataset.bookId));
    return book ? { card, book } : null;
  };

  grid.addEventListener("click", (e) => {
    const h = hit(e);
    if (h) onItemClick(e, h.book);
  });
  grid.addEventListener("dblclick", (e) => {
    const h = hit(e);
    if (h) openReader(h.book);
  });
  grid.addEventListener("contextmenu", (e) => {
    const h = hit(e);
    if (!h) return;
    e.preventDefault();
    onItemContext(e, h.book);
    openContextMenu(e.clientX, e.clientY, h.book);
  });
  grid.addEventListener("mousedown", (e) => {
    const h = hit(e);
    if (h) onBookDragDown(e, h.card, h.book); // drag onto a series tile to file it
  });
}

function renderGallery(books) {
  const grid = $("#gallery-grid");
  grid.innerHTML = "";
  for (const b of books) {
    grid.appendChild(galleryCard(b));
  }
}

// Grouped gallery top level: a collection tile per series, a normal card per
// standalone book.
function renderGalleryGrouped(entries) {
  const grid = $("#gallery-grid");
  grid.innerHTML = "";
  for (const e of entries) {
    grid.appendChild(e.type === "series" ? seriesCard(e) : galleryCard(e.book));
  }
}

// A series collection tile; a click drills in. It is a `.series-card`, NOT a
// `.book-card`, and the SelectionController skips it.
function seriesCard(entry) {
  const ordered = [...entry.books].sort(bySeriesIndex);
  const lead = ordered.find((b) => coverUrlFor(b)) || ordered[0];
  const n = entry.books.length;

  const card = document.createElement("div");
  card.className = "series-card";
  card.dataset.series = entry.name;
  card.title = `${entry.name}\n${n} book${n === 1 ? "" : "s"}`;

  const stack = document.createElement("div");
  stack.className = "series-stack";
  const cover = document.createElement("div");
  cover.className = "cover";
  const coverUrl = coverUrlFor(lead, { thumb: true });
  if (coverUrl) {
    cover.classList.add("has-image");
    const img = document.createElement("img");
    img.src = coverUrl;
    img.alt = "";
    img.loading = "lazy";
    img.draggable = false; // don't let the lead cover drag out as a bare image
    cover.appendChild(img);
  } else {
    const ph = document.createElement("div");
    ph.className = "cover-placeholder";
    ph.textContent = entry.name;
    cover.appendChild(ph);
  }
  stack.appendChild(cover);
  const badge = document.createElement("span");
  badge.className = "series-count";
  badge.textContent = String(n);
  stack.appendChild(badge);

  // Overflow menu (2×3 dots, bottom-right) — opens the series context menu a
  // right-click gives. stopPropagation keeps the dots from also drilling in through the
  // card's click handler.
  const menuBtn = document.createElement("button");
  menuBtn.type = "button";
  menuBtn.className = "series-menu";
  menuBtn.title = "Series options";
  menuBtn.setAttribute("aria-label", "Series options");
  for (let i = 0; i < 6; i++) menuBtn.appendChild(document.createElement("span"));
  menuBtn.addEventListener("click", (ev) => {
    ev.stopPropagation();
    const r = menuBtn.getBoundingClientRect();
    openSeriesContextMenu(r.left, r.bottom, entry);
  });
  stack.appendChild(menuBtn);

  card.appendChild(stack);

  const meta = document.createElement("div");
  meta.className = "meta";
  const t = document.createElement("div");
  t.className = "t";
  t.textContent = entry.name;
  const a = document.createElement("div");
  a.className = "a";
  a.textContent = seriesSubtitle(entry);
  meta.append(t, a, seriesMetaBadges(entry));
  card.appendChild(meta);

  card.addEventListener("click", (e) => onSeriesClick(e, entry.name));
  card.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    openSeriesContextMenu(e.clientX, e.clientY, entry);
  });
  return card;
}

// Grouped list top level: a navigation index. Series rows drill in; standalone
function renderSeriesList(entries) {
  const tbody = $("#series-list tbody");
  tbody.innerHTML = "";
  for (const e of entries) {
    const tr = document.createElement("tr");
    const name = document.createElement("td");
    name.className = "col-series";
    const author = document.createElement("td");
    author.className = "col-author";
    const count = document.createElement("td");
    count.className = "col-count";

    if (e.type === "series") {
      tr.dataset.series = e.name; // keyboard focus cursor keys off this
      name.textContent = e.name;
      author.textContent = seriesSubtitle(e);
      count.textContent = String(e.books.length);
      tr.append(name, author, count);
      tr.addEventListener("click", (ev) => onSeriesClick(ev, e.name));
      tr.addEventListener("contextmenu", (ev) => {
        ev.preventDefault();
        openSeriesContextMenu(ev.clientX, ev.clientY, e);
      });
    } else {
      const b = e.book;
      tr.className = "book-row";
      if (booksSelection.has(b.id)) tr.classList.add("selected");
      tr.dataset.bookId = b.id;
      name.textContent = b.title || "Untitled";
      author.textContent = b.author || "";
      count.textContent = ""; // a standalone book isn't a collection
      tr.append(name, author, count);
      tr.addEventListener("click", (ev) => onItemClick(ev, b));
      tr.addEventListener("dblclick", () => openReader(b));
      tr.addEventListener("contextmenu", (ev) => {
        ev.preventDefault();
        onItemContext(ev, b);
        openContextMenu(ev.clientX, ev.clientY, b);
      });
      wireBookDragSource(tr, b); // drag onto a series row to file it there
    }
    tbody.appendChild(tr);
  }
}

function galleryCard(b) {
  const card = document.createElement("div");
  card.className = "book-card";
  if (booksSelection.has(b.id)) card.classList.add("selected");
  card.dataset.bookId = b.id;
  card.title = `${b.title}\n${b.author}`;

  const coverUrl = coverUrlFor(b, { thumb: true });
  const cover = document.createElement("div");
  cover.className = "cover";
  if (coverUrl) {
    cover.classList.add("has-image");
    const img = document.createElement("img");
    img.src = coverUrl;
    img.alt = "";
    img.loading = "lazy";
    img.draggable = false; // the card owns the drag (file into a series), not the cover image
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

  // No per-card listeners: click / dblclick / contextmenu / drag are delegated
  // once on `#gallery-grid` (see wireGalleryDelegation), resolved back to this
  // book via `dataset.bookId`. Keeps each render's card build pure DOM.
  return card;
}

function metaBadges(b) {
  const wrap = document.createElement("div");
  wrap.className = "meta-badges";

  // The side the import wrote directly is always "done". The other side, whichever
  // direction the background job runs, carries the row's status. `b.kind` names that
  // side.
  wrap.appendChild(formatBadge(nonKfxFormat(b), b, /*compact=*/ true));
  wrap.appendChild(formatBadge("kfx", b, /*compact=*/ true));

  if (state.sentSet.has(b.sha256)) {
    const dot = document.createElement("span");
    dot.className = "meta-kindle-dot";
    dot.title = "On Kindle";
    wrap.appendChild(dot);
  }

  return wrap;
}

// Format badges for a series collection tile (gallery view): the distinct
// non-KFX sides present across its members — usually one, since a series is
// typically uniform, but a mixed series shows both — then the universal KFX.
function seriesMetaBadges(entry) {
  const wrap = document.createElement("div");
  wrap.className = "meta-badges";
  const present = new Set(entry.books.map(nonKfxFormat));
  for (const fmt of ["epub", "pdf"]) {
    if (present.has(fmt)) {
      wrap.appendChild(formatBadge(fmt, worstForFormat(entry.books, fmt), /*compact=*/ true));
    }
  }
  wrap.appendChild(formatBadge("kfx", worstForFormat(entry.books, "kfx"), /*compact=*/ true));
  return wrap;
}

// The member whose conversion status for `format` is the most urgent
// (error > converting > pending > done), surfacing a converting or failed
// volume on the collection's badge. Defaults to the first member.
const STATUS_RANK = { error: 3, converting: 2, pending: 1, done: 0 };
function worstForFormat(books, format) {
  let rep = books[0];
  let rank = -1;
  for (const b of books) {
    const r = STATUS_RANK[formatStatusFor(format, b)] ?? 0;
    if (r > rank) {
      rank = r;
      rep = b;
    }
  }
  return rep;
}

// A book pairs KFX with exactly one non-KFX side: EPUB for reflowable books,
// PDF for PDF-backed (container) books. Derived from the conversion `kind`.
function nonKfxFormat(b) {
  return b.kind === "pdf_to_kfx" || b.kind === "kfx_to_pdf" ? "pdf" : "epub";
}

// The format a book was imported *from*: the first token of `b.kind`
function sourceFormat(b) {
  return (b.kind || "epub_to_kfx").split("_to_")[0];
}

// Returns the conversion status as it applies to the given format side
function formatStatusFor(format, b) {
  const target = (b.kind || "epub_to_kfx").split("_to_")[1] || "kfx";
  return format === target ? b.status : "done";
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
    // Show why before offering the retry: a click that silently re-ran the conversion
    // left the reason unreadable and repeated the same failure.
    span.addEventListener("click", (e) => {
      e.stopPropagation();
      showConversionErrorReport([b.id]);
    });
    span.style.cursor = "pointer";
  }
  return span;
}

// The Books list view is rendered by the shared TableView (see `booksTable`),
// which owns the colgroup/thead/tbody build, sort indicator, and header wiring.
function renderList(books) {
  booksTable.render(books);
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
    // Seed the progress bar at 0 when a job starts; drop it on any terminal
    // state (done/error) or when it falls back to pending. The per-image
    // `conversion:progress` ticks fill it in between.
    if (status === "converting") {
      state.convProgress[book_id] = { fraction: 0, label: "Starting…" };
    } else {
      delete state.convProgress[book_id];
    }
    // A failure surfaces its reason on its own, past a book sitting at "failed"
    // with the cause behind a badge hover.
    if (status === "error") {
      if (!state.convFailures.includes(book_id)) state.convFailures.push(book_id);
      showConversionErrorReport(state.convFailures);
    }
    // A finished conversion re-pulls rows for `kfx_path`; every other status repaints
    // what is in hand.
    if (status === "done") {
      refresh();
    } else {
      render();
    }
  });

  // Live per-book conversion progress. These fire often (once per chapter /
  window.api.listen("conversion:progress", (e) => {
    const { book_id, fraction, label } = e.payload || {};
    if (book_id == null) return;
    state.convProgress[book_id] = { fraction: fraction ?? 0, label: label || "" };
    if (!updateQueueRowProgress(book_id)) renderQueue();
  });
}

// Build the URL handed to <img src=…>. Returns null for a book with no cover on disk.
function coverUrlFor(b, { thumb = false } = {}) {
  if (!b) return null;
  const path = (thumb && b.cover_thumb_path) || b.cover_path;
  if (!path) return null;
  const base = window.api.fileUrl(path);
  if (!base) return null;
  // Per-book cache token: the served image's mtime, `cover_rev`, appended as `?v=`.
  return `${base}?v=${b.cover_rev || 0}`;
}

// ---------------------------------------------------------------------------
// Device pill + Kindle page
// ---------------------------------------------------------------------------

// Re-pull everything the Kindle page shows. Fired on entering the device section
function refreshDevicePage() {
  refreshDeviceList();
  refreshServerStatus();
}

function wireDevice() {
  // The pill toggles the full-screen Kindle page: into it, or back to the last
  // library section from inside it, which is the popover's click-to-dismiss.
  // setSection's "device" branch fires the on-enter refreshes.
  $("#device-pill").addEventListener("click", () => {
    setSection(state.section === "device" ? lastLibrarySection : "device");
  });
  $("#btn-send-unsent").addEventListener("click", () => sendUnsent());
  $("#btn-sync-annotations").addEventListener("click", () => syncAnnotations());
  $("#btn-restore-annotations")?.addEventListener("click", () => restoreFromDevice());
  $("#btn-import-all-orphans").addEventListener("click", () => importAllOrphans());
  $("#btn-device-eject").addEventListener("click", () => ejectDevice());
}

// Verb shown in the footer / settings status for each long library file op.
const FILEOP_VERB = { backup: "Backing up", restore: "Restoring", merge: "Merging" };
// Guards the fileop footer line against a final progress event arriving after
// the triggering handler cleared it (same race as sendInFlight).
let fileopInFlight = false;

// Live progress for a long library file op (backup / restore / merge), in the
// footer status line and in the settings modal's own line, which is where the
// op is triggered.
function subscribeFileopProgress() {
  window.api.listen("library:fileop-progress", (e) => {
    const p = e?.payload;
    if (!p) return;
    if (!fileopInFlight) return; // ignore a late tick after the handler resolved
    state.fileop = { op: p.op, done: p.done, total: p.total };
    renderQueue();
    const pct = p.total > 0 ? Math.round((p.done / p.total) * 100) : null;
    const el = $("#settings-status");
    if (pct != null && el && !el.hidden) {
      el.textContent = `${FILEOP_VERB[p.op] || "Working"}… ${pct}%`;
    }
  });
}

// Live byte-progress for the file currently being sent to the Kindle, shown in
// the footer status line (see renderQueue). Distinct from `device:send-progress`
// below, which paints the per-file terminal result into the device popover.
function subscribeSendActive() {
  window.api.listen("device:send-active", (e) => {
    const p = e?.payload;
    if (!p) return;
    if (!sendInFlight) return; // ignore a late tick after sendBooks resolved
    const task = state.sendQueue.find((t) => t.id === p.book_id);
    if (task) {
      task.status = "sending";
      task.done = p.done;
      task.total = p.total;
      if (p.title) task.title = p.title;
    }
    renderQueue();
  });
}

function subscribeSendProgress() {
  window.api.listen("device:send-progress", (e) => {
    const r = e.payload;
    if (!r) return;
    // Move the matching queue task to its terminal state (drawer + summary).
    // find() on a cleared queue is a safe no-op for a late event.
    const task = state.sendQueue.find((t) => t.id === r.book_id);
    if (task) {
      task.status =
        r.kind === "pushed" || r.kind === "already_present"
          ? "sent"
          : r.kind === "skipped"
            ? "skipped"
            : "failed";
      if (task.status === "sent" && task.total > 0) task.done = task.total;
      task.error = r.error;
      renderQueue();
    }
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

let sendInFlight = false;
async function sendBooks(bookIds) {
  if (!state.device) {
    showToast("no Kindle connected", true);
    return;
  }
  const btn = $("#btn-send-unsent");
  btn.disabled = true;
  sendInFlight = true;
  // Seed one queue task per book up front, and they all appear in the queue drawer at
  // once, like conversions, transitioning as events land.
  state.sendQueue = bookIds.map((id) => {
    const b = state.books.find((x) => x.id === id);
    return {
      id,
      title: b?.title || "Untitled",
      author: b?.author || "",
      status: "queued",
      done: 0,
      total: 0,
    };
  });
  renderQueue();
  let results = [];
  try {
    results = await window.api.invoke("device_send", { bookIds });
  } catch (e) {
    showToast(`send failed: ${e}`, true);
    btn.disabled = false;
    return;
  } finally {
    // Clear the send queue once the batch ends. The flag guards against a final
    // send-active/-progress event landing after this (Tauri events and the
    // command response aren't mutually ordered) and re-populating it.
    sendInFlight = false;
    state.sendQueue = [];
    renderQueue();
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

// ----- Kindle page: apps live in the Apps tab ---------------------------------

// The Apps tab is the only place an app is installed, updated, removed or added. The
// Kindle page keeps the device itself. A connect or disconnect changes every app row's
// device state, reaching the tab here.
function invalidateApps() {
  window.Apps?.invalidate();
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
    // Eject is mass-storage-only. An MTP device closes its USB session on
    // unplug and has no eject, which hides the button.
    const ejectBtn = $("#btn-device-eject");
    if (ejectBtn) {
      ejectBtn.hidden = info.transport !== "mass_storage";
      ejectBtn.disabled = false;
    }
    // Annotation sync works on both transports: mass-storage reads the volume,
    // MTP (Scribe) pulls the .yjr over USB. Enabled whenever a Kindle is
    // connected. (MTP yields records only if the device exposes .sdr/.yjr.)
    const syncBtn = $("#btn-sync-annotations");
    if (syncBtn) syncBtn.disabled = false;
    const restoreBtn = $("#btn-restore-annotations");
    if (restoreBtn) restoreBtn.disabled = false;
    $("#device-model").textContent = info.model || "Kindle";
    // MTP fills firmware in through the on-connect session refresh, showing "—" for the
    // first beat and updating after, as free space does.
    $("#device-firmware").textContent = info.firmware || "—";
    $("#device-serial").textContent = info.serial || "—";
    $("#device-transport").textContent = transportLabel(info.transport);
    $("#device-free").textContent =
      info.free_bytes != null && info.total_bytes != null
        ? `${formatBytes(info.free_bytes)} of ${formatBytes(info.total_bytes)}`
        : "—";
    // MTP devices need exclusive USB session access, and any other app talking to the
    // Kindle (Image Capture, OpenMTP, Calibre) holds it.
    if (info.transport === "mtp") {
      tip.textContent =
        "MTP device. Quit Image Capture, OpenMTP, or Calibre if a push fails — only one app can hold the USB session at a time.";
      tip.hidden = false;
    } else {
      tip.hidden = true;
      tip.textContent = "";
    }
    // Always load sent state when a device connects, and the list-view "On Kindle"
    // column reflects the device without a visit to the Kindle page.
    refreshDeviceList();
    invalidateApps();
  } else {
    dot.className = "device-dot disconnected";
    label.textContent = "No Kindle";
    status.className = "device-popover-status disconnected";
    status.textContent = "Disconnected";
    $("#device-model").textContent = "—";
    $("#device-firmware").textContent = "—";
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
    // The apps card stays up with no Kindle, listing what a push carries and
    // taking a registration before the cable is in.
    invalidateApps();
    // Hide eject button on disconnect — nothing to eject.
    const ejectBtn = $("#btn-device-eject");
    if (ejectBtn) ejectBtn.hidden = true;
    const syncBtn = $("#btn-sync-annotations");
    if (syncBtn) syncBtn.disabled = true;
    const restoreBtn = $("#btn-restore-annotations");
    if (restoreBtn) restoreBtn.disabled = true;
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

  // Bulk "Import all orphans" — count + show/hide. The button drives
  // `device_import_orphan` per row, which enqueues the KFX→EPUB background job.
  const orphanCount = rows.filter((r) => r.kind === "orphan").length;
  const btn = $("#btn-import-all-orphans");
  if (btn) {
    btn.hidden = orphanCount === 0;
    btn.textContent =
      orphanCount === 1 ? "Import 1 orphan" : `Import all ${orphanCount} orphans`;
    btn.disabled = false;
  }
}

function deviceRow(r) {
  const li = document.createElement("li");
  const top = document.createElement("div");
  top.className = "device-sent-top";

  const title = document.createElement("div");
  title.className = "device-sent-title";
  if (r.kind === "sent") {
    title.textContent = r.title || r.filename;
    title.title = r.filename;
  } else {
    // orphan — no local library entry. Shows the filename minus the sha8 infix, leaving
    // something recognizable on screen.
    const display = r.filename.replace(/\.[0-9a-f]{8}\.kfx$/i, ".kfx");
    title.textContent = display;
    title.title = r.filename;
  }

  const del = document.createElement("button");
  del.type = "button";
  del.className = "device-sent-del";
  del.title = "Remove from Kindle";
  del.textContent = "×";
  del.addEventListener("click", (e) => {
    e.stopPropagation();
    // The popup row carries the exact filename — delete by name, with no sha
    // translation. The backend verifies the `.<sha8>.kfx` shape before touching
    // anything, and a stale UI drives no arbitrary delete.
    const label = r.kind === "sent"
      ? r.title || r.filename
      : r.filename;
    deleteFromDevice([r.filename], [label]);
  });

  top.append(title, del);

  const meta = document.createElement("div");
  meta.className = "device-sent-meta";
  if (r.kind === "sent") {
    if (r.author) meta.textContent = r.author;
  } else {
    const badge = document.createElement("span");
    badge.className = "device-sent-orphan";
    badge.textContent = "not in library";
    meta.appendChild(badge);

    const reimport = document.createElement("button");
    reimport.type = "button";
    reimport.className = "device-sent-reimport";
    reimport.textContent = "Import to library";
    reimport.addEventListener("click", (e) => {
      e.stopPropagation();
      importOrphan(r.filename);
    });
    meta.appendChild(reimport);
  }

  li.append(top, meta);
  return li;
}

async function deleteFromDevice(filenames, titles) {
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
  // Light the status bar before the first round trip: the backend deletes one file at a
  // time over MTP, a large batch spends minutes in here, and the grid re-reads once it
  // ends.
  state.deleting = { done: 0, total: filenames.length, title: titles[0] || "" };
  renderQueue();
  try {
    results = await window.api.invoke("device_delete", { filenames });
  } catch (e) {
    showToast(`delete failed: ${e}`, true);
    return;
  } finally {
    state.deleting = null;
    renderQueue();
  }
  const counts = { removed: 0, not_ours: 0, failed: 0 };
  for (const r of results) counts[r.kind] = (counts[r.kind] || 0) + 1;
  const parts = [];
  if (counts.removed) parts.push(`${counts.removed} removed`);
  if (counts.not_ours) parts.push(`${counts.not_ours} skipped (not ours)`);
  if (counts.failed) parts.push(`${counts.failed} failed`);
  showToast(parts.join(" · "), counts.failed > 0);
  await refreshDeviceList();
}

async function ejectDevice() {
  if (!state.device) return;
  const btn = $("#btn-device-eject");
  btn.disabled = true;
  try {
    await window.api.invoke("device_eject");
    // Don't manually clear UI state — the device monitor will fire
    // `device:status` with null shortly, and `updateDeviceUI` will
    // do the right thing.
    showToast("Kindle ejected");
  } catch (e) {
    btn.disabled = false;
    showToast(`eject failed: ${e}`, true);
  }
}

// Status-bar feedback for a device orphan import. `importBase` holds the label its
// byte-progress ticks append to.
let importBase = null;

function setImportStatus(label) {
  importBase = label;
  state.importing = label ? { message: label, failed: false } : null;
  renderQueue();
}

// Display name for the status line: drop the extension; the device filename's
// `_<sha8>` infix is harmless to keep and helps disambiguate.
function importDisplayName(filename) {
  return String(filename || "").replace(/\.[^./\\]+$/, "");
}

async function importAllOrphans() {
  if (!state.device) {
    showToast("no Kindle connected", true);
    return;
  }
  const rows = state.sent || [];
  const orphans = rows.filter((r) => r.kind === "orphan");
  if (orphans.length === 0) return;
  const btn = $("#btn-import-all-orphans");
  btn.disabled = true;
  let imported = 0;
  let duplicate = 0;
  let failed = 0;
  try {
    for (let i = 0; i < orphans.length; i++) {
      const r = orphans[i];
      setImportStatus(
        `Importing ${i + 1}/${orphans.length}: ${importDisplayName(r.filename)}…`,
      );
      try {
        const result = await window.api.invoke("device_import_orphan", {
          filename: r.filename,
        });
        if (result.kind === "imported") imported++;
        else if (result.kind === "duplicate") duplicate++;
        else failed++;
      } catch (e) {
        failed++;
        console.error("import_orphan failed:", r.filename, e);
      }
      btn.textContent = `Importing… ${imported + duplicate + failed}/${orphans.length}`;
    }
  } finally {
    setImportStatus(null);
  }
  const parts = [];
  if (imported) parts.push(`${imported} imported`);
  if (duplicate) parts.push(`${duplicate} already in library`);
  if (failed) parts.push(`${failed} failed`);
  showToast(parts.join(", ") || "done", failed > 0);
  await refresh();
  await refreshDeviceList();
}

async function importOrphan(filename) {
  if (!state.device) {
    showToast("no Kindle connected", true);
    return;
  }
  setImportStatus(`Importing ${importDisplayName(filename)}…`);
  let result;
  try {
    result = await window.api.invoke("device_import_orphan", { filename });
  } catch (e) {
    setImportStatus(null);
    showToast(`import failed: ${e}`, true);
    return;
  }
  setImportStatus(null);
  if (result.kind === "imported") {
    showToast(`imported: ${filename}`);
  } else if (result.kind === "duplicate") {
    showToast(`already in library: ${filename}`);
  } else {
    showToast(`import failed: ${result.error}`, true);
  }
  await refresh();
  await refreshDeviceList();
}

// ---------------------------------------------------------------------------

function subscribePullProgress() {
  // Per-file event from the autopull worker. Updates the Kindle-page status
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

  // Live byte-progress for a manual orphan import (the per-book "Import" /
  window.api.listen("device:import-progress", (e) => {
    const p = e.payload;
    if (!p || importBase === null) return;
    const mib = (n) => (n / 1048576).toFixed(1);
    const suffix = p.total ? `  ·  ${mib(p.done)} / ${mib(p.total)} MiB` : "";
    state.importing = { message: importBase + suffix, failed: false };
    renderQueue();
  });

  // One per file removed from the Kindle. Gated on `state.deleting`, which holds a
  // stray event from resurrecting the line after the batch resolves — the idiom
  // `importBase` above uses.
  window.api.listen("device:delete-progress", (e) => {
    const r = e.payload;
    if (!r || !state.deleting) return;
    state.deleting = {
      ...state.deleting,
      done: state.deleting.done + 1,
      title: r.filename || state.deleting.title,
    };
    renderQueue();
  });

  // Status-bar progress counter. The backend emits this once on autopull start (`done:
  // 0`) and once per book completed after, and the pull renders throughout.
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
// Kindle annotation sync (highlights / notes / bookmarks)

// Turn a DeviceImportReport into a one-line summary for the toast. Sidle is an
// additive backup: a sync adds and deletes nothing, and this reports new
// annotations and handwritten pages. A no-op sync reads "up to date".
function annotationSyncSummary(report) {
  const added = report?.annotations?.inserted ?? 0;
  const inkPages = report?.ink_pages ?? 0;
  const files = report?.misc_new ?? 0;

  const parts = [];
  if (added > 0) {
    const books = report?.matched ?? 0;
    const from = books > 0 ? ` across ${books} book${books === 1 ? "" : "s"}` : "";
    parts.push(`${added} annotation${added === 1 ? "" : "s"}${from}`);
  }
  if (inkPages > 0) parts.push(`${inkPages} handwritten page${inkPages === 1 ? "" : "s"}`);
  if (files > 0) parts.push(`${files} file${files === 1 ? "" : "s"}`);
  if (parts.length > 0) return `Synced ${parts.join("; ")}`;

  // Nothing new to sync. Logs refresh on every sync, growing by appending, and
  // surface as the fallback message alone.
  if (logs > 0) return `Backed up ${logs} log${logs === 1 ? "" : "s"}`;
  return "Annotations already up to date";
}

// Manual re-sync from the device popover button.
async function syncAnnotations() {
  const btn = $("#btn-sync-annotations");
  const prog = $("#device-send-progress");
  if (btn.disabled) return;
  btn.disabled = true;
  const prevLabel = btn.textContent;
  btn.textContent = "Syncing…";
  if (prog) {
    prog.hidden = false;
    prog.textContent = "syncing annotations…";
  }
  // Drive the status bar via the same progress state the auto-sync uses; the
  // command emits `annotations:sync-progress` while it runs.
  state.annotationSync = { stage: "annotations", current: 0, total: 0, label: "" };
  renderQueue();
  try {
    const report = await window.api.invoke("annotations_import_from_device");
    showToast(annotationSyncSummary(report));
    window.sidleReader?.reloadAnnotations?.();
    window.Misc?.invalidate(); // new files may have landed
  } catch (e) {
    showToast(`annotation sync failed: ${e}`, true);
  } finally {
    state.annotationSync = false;
    renderQueue();
    btn.textContent = prevLabel;
    // Re-enable while a Kindle on either transport is connected.
    btn.disabled = !state.device;
    if (prog) setTimeout(() => (prog.hidden = true), 2000);
  }
}

// "Restore from device" — re-import everything the Kindle holds and UNDO Sidle-side
// deletions, recovering an accidental delete. Two-click confirm, reversing deletions:
// the first click arms, the second runs.
async function restoreFromDevice() {
  const btn = $("#btn-restore-annotations");
  if (!btn || btn.disabled) return;
  if (btn.dataset.armed !== "1") {
    btn.dataset.armed = "1";
    btn.dataset.label = btn.textContent;
    btn.textContent = "Click again to restore";
    setTimeout(() => {
      if (btn.dataset.armed === "1") {
        btn.dataset.armed = "";
        btn.textContent = btn.dataset.label || "Restore from device";
      }
    }, 3000);
    return;
  }
  btn.dataset.armed = "";
  const label = btn.dataset.label || "Restore from device";
  btn.disabled = true;
  btn.textContent = "Restoring…";
  const prog = $("#device-send-progress");
  if (prog) {
    prog.hidden = false;
    prog.textContent = "restoring from device…";
  }
  state.annotationSync = { stage: "annotations", current: 0, total: 0, label: "" };
  renderQueue();
  try {
    const report = await window.api.invoke("device_restore");
    const added = report?.annotations?.inserted ?? 0;
    const ink = report?.ink_pages ?? 0;
    if (added > 0 || ink > 0) {
      const parts = [];
      if (added > 0) parts.push(`${added} annotation${added === 1 ? "" : "s"}`);
      if (ink > 0) parts.push(`${ink} handwritten page${ink === 1 ? "" : "s"}`);
      showToast(`Restored ${parts.join(" + ")}`);
    } else {
      showToast("Nothing to restore — backup already matches the device");
    }
    window.sidleReader?.reloadAnnotations?.();
    window.Misc?.invalidate(); // restore re-pulls everything, misc included
  } catch (e) {
    showToast(`restore failed: ${e}`, true);
  } finally {
    state.annotationSync = false;
    renderQueue();
    btn.textContent = label;
    btn.disabled = !state.device;
    if (prog) setTimeout(() => (prog.hidden = true), 2000);
  }
}

// Background sync driven by the device monitor on connect.
function subscribeAnnotationSync() {
  window.api.listen("annotations:sync-start", () => {
    state.annotationSync = { stage: "annotations", current: 0, total: 0, label: "" };
    renderQueue();
  });
  // Per-item progress: which book/notebook is syncing (count + label).
  window.api.listen("annotations:sync-progress", (e) => {
    if (state.annotationSync) {
      state.annotationSync = e.payload;
      renderQueue();
    }
  });
  window.api.listen("annotations:sync-done", (e) => {
    state.annotationSync = false;
    renderQueue();
    const report = e.payload;
    const added = report?.annotations?.inserted ?? 0;
    const inkPages = report?.ink_pages ?? 0;
    const files = report?.misc_new ?? 0;
    // Toast where the sync pulled something new, leaving a no-op reconnect
    // quiet. Files the library had never seen count; a refreshed one does not,
    // a log updating on every sync toasting on every connect.
    if (added > 0 || inkPages > 0 || files > 0) showToast(annotationSyncSummary(report));
    // If the user is reading one of the synced books, repaint in place.
    window.sidleReader?.reloadAnnotations?.();
    // New files may have landed — refresh the Files tab if it's open.
    window.Misc?.invalidate();
    // A USB sync carries reading sessions too.
    window.ReadingLog?.invalidate?.();
  });
  // `POST /sync/reading-log` over the LAN, reaching the app through
  // `sync_pulse`. The Kindle pushes reading with no annotation attached to it.
  window.api.listen("reading-log:changed", () => {
    window.ReadingLog?.invalidate?.();
  });
  window.api.listen("annotations:sync-error", (e) => {
    state.annotationSync = false;
    renderQueue();
    showToast(`Kindle annotation sync failed: ${e.payload}`, true);
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

  renderStatusSummary(active);

  const ul = $("#queue-list");
  ul.innerHTML = "";
  // The running import sits at the top of the queue drawer.
  const importRow = state.importing?.label ? importQueueRow() : null;
  if (importRow) ul.appendChild(importRow);
  // A delete batch outranks the conversion queue for the same reason an import
  // does: the user is standing in front of it.
  const deleteRow = state.deleting ? deleteQueueRow() : null;
  if (deleteRow) ul.appendChild(deleteRow);
  for (const t of state.sendQueue) ul.appendChild(sendQueueRow(t));
  for (const b of active) ul.appendChild(queueRow(b));
  $("#queue-empty").hidden =
    active.length > 0 ||
    state.sendQueue.length > 0 ||
    importRow != null ||
    deleteRow != null;
}

// The single status-bar line, apart from `renderQueue`: the frequent in-place
// progress patches repaint it and leave the drawer list built.
function renderStatusSummary(active) {
  const counts = {
    converting: active.filter((b) => b.status === "converting").length,
    pending: active.filter((b) => b.status === "pending").length,
    error: active.filter((b) => b.status === "error").length,
    total: state.books.length,
  };

  const toggle = $("#status-bar-toggle");
  const summary = $("#status-bar-summary");
  toggle.classList.remove("active", "errors", "done");

  // Priority order in the status bar:
  //   1. foreground import (state.importing) — user-initiated drop / add
  //   2. autopull from Kindle's /dedrm folder (state.autopull)
  if (state.opening) {
    // A double-clicked book: a large KFX takes a beat to load, and this names the one
    // opening. openReader clears it once the reader is up.
    summary.textContent = `Opening ${state.opening}…`;
    toggle.classList.add("active");
  } else if (state.importing) {
    // A converting import (azw3 / mobi / zip) reports its step and how far in
    // it is; the ones stored as they arrive have only the opening message.
    const imp = state.importing;
    summary.textContent = imp.label
      ? `${imp.message} — ${imp.label} · ${convFillPct(imp)}%`
      : imp.message;
    if (imp.failed) toggle.classList.add("errors");
    else toggle.classList.add("active");
  } else if (state.sendQueue.length) {
    const q = state.sendQueue;
    const sending = q.find((t) => t.status === "sending");
    const finished = q.filter(
      (t) => t.status !== "queued" && t.status !== "sending",
    ).length;
    const batch = q.length > 1 ? ` (${Math.min(finished + 1, q.length)}/${q.length})` : "";
    if (sending) {
      const size =
        sending.total > 0
          ? ` — ${formatBytes(sending.done)} / ${formatBytes(sending.total)}`
          : "…";
      summary.textContent = `Sending ${sending.title}${size}${batch}`;
    } else {
      summary.textContent = `Sending to Kindle…${batch}`;
    }
    toggle.classList.add("active");
  } else if (state.deleting) {
    const d = state.deleting;
    const pct = d.total > 0 ? Math.round((d.done / d.total) * 100) : 0;
    summary.textContent =
      d.total > 1
        ? `Removing from Kindle — ${d.done}/${d.total} (${pct}%)`
        : "Removing from Kindle…";
    toggle.classList.add("active");
  } else if (state.fileop) {
    const f = state.fileop;
    const pct = f.total > 0 ? Math.round((f.done / f.total) * 100) : null;
    const verb = FILEOP_VERB[f.op] || "Working on";
    summary.textContent =
      pct != null ? `${verb} library — ${pct}%` : `${verb} library…`;
    toggle.classList.add("active");
  } else if (state.autopull) {
    summary.textContent =
      `Pulling ${state.autopull.done}/${state.autopull.total} from Kindle…`;
    toggle.classList.add("active");
  } else if (state.annotationSync) {
    const p = state.annotationSync;
    const noun = p && p.stage === "ink" ? "handwriting" : "annotations";
    summary.textContent =
      p && p.total > 0 && p.label
        ? `Syncing ${noun}: ${p.label} (${p.current}/${p.total})`
        : "Syncing Kindle annotations…";
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
}

// A queue-drawer row for the import in flight. Same shape as a conversion row —
function deleteQueueRow() {
  const d = state.deleting;
  const pct = d.total > 0 ? Math.round((d.done / d.total) * 100) : 0;
  const li = document.createElement("li");
  li.dataset.deleteRow = "1";

  const main = document.createElement("div");
  main.className = "queue-row-main";
  const title = document.createElement("div");
  title.className = "queue-title";
  title.textContent = d.title || "Removing from Kindle";
  const status = document.createElement("div");
  status.className = "queue-status converting";
  status.textContent = `${d.done}/${d.total} removed`;
  main.append(title, status);

  const meta = document.createElement("div");
  meta.className = "queue-meta";
  meta.textContent = "Removing";

  const bar = document.createElement("div");
  bar.className = "queue-progress converting";
  const fill = document.createElement("div");
  fill.className = "queue-progress-fill";
  fill.style.width = `${pct}%`;
  bar.appendChild(fill);

  li.append(main, meta, bar);
  return li;
}

function importQueueRow() {
  const imp = state.importing;
  const li = document.createElement("li");
  li.dataset.importRow = "1";

  const main = document.createElement("div");
  main.className = "queue-row-main";
  const title = document.createElement("div");
  title.className = "queue-title";
  title.textContent = imp.name || imp.message;
  const status = document.createElement("div");
  status.className = "queue-status converting";
  status.textContent = importStatusText(imp);
  main.append(title, status);

  const meta = document.createElement("div");
  meta.className = "queue-meta";
  meta.textContent = "Importing";

  const bar = document.createElement("div");
  bar.className = "queue-progress converting";
  const fill = document.createElement("div");
  fill.className = "queue-progress-fill";
  fill.style.width = `${convFillPct(imp)}%`;
  bar.appendChild(fill);

  li.append(main, meta, bar);
  return li;
}

// "Step · NN%" for an import that reports phases; a plain word for one that
// finishes too fast to have any.
function importStatusText(imp) {
  return imp.label ? `${imp.label} · ${convFillPct(imp)}%` : "Importing…";
}

// Patch the import row in place for a progress tick — same reason as
// `updateQueueRowProgress`: these fire per image. Returns false when the row
// isn't mounted (the drawer hasn't rendered since the import started).
function updateImportRowProgress() {
  const li = document.querySelector("#queue-list li[data-import-row]");
  if (!li) return false;
  const imp = state.importing;
  const fill = li.querySelector(".queue-progress-fill");
  if (fill) fill.style.width = `${convFillPct(imp)}%`;
  const status = li.querySelector(".queue-status.converting");
  if (status) status.textContent = importStatusText(imp);
  const title = li.querySelector(".queue-title");
  if (title) title.textContent = imp.name || imp.message;
  // The status bar carries the same numbers and is always visible, unlike the
  // drawer.
  renderStatusSummary(
    state.books.filter(
      (b) => b.status === "pending" || b.status === "converting" || b.status === "error",
    ),
  );
  return true;
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
    const label = document.createElement("button");
    label.className = "queue-why";
    label.textContent = "Failed";
    label.title = b.error || "";
    label.addEventListener("click", () => showConversionErrorReport([b.id]));
    status.appendChild(label);
    const retry = document.createElement("button");
    retry.className = "queue-retry";
    retry.textContent = "Retry";
    retry.addEventListener("click", () => retryConvert(b.id));
    status.appendChild(retry);
  } else if (b.status === "converting") {
    status.textContent = convStatusText(state.convProgress[b.id]);
  } else {
    status.textContent = "Queued";
  }

  main.append(title, status);

  const meta = document.createElement("div");
  meta.className = "queue-meta";
  meta.textContent = b.author || "";

  const bar = document.createElement("div");
  bar.className = `queue-progress ${b.status}`;
  // Determinate fill for an in-flight conversion (pending/error keep their CSS
  // `::after` treatment — amber stub / full accent). Width tracks the live
  // progress fraction; `updateQueueRowProgress` nudges it in place per tick.
  if (b.status === "converting") {
    const fill = document.createElement("div");
    fill.className = "queue-progress-fill";
    fill.style.width = `${convFillPct(state.convProgress[b.id])}%`;
    bar.appendChild(fill);
  }

  li.dataset.bookId = b.id;
  li.append(main, meta, bar);
  return li;
}

// A queue-drawer row for a book in the send-to-Kindle batch — mirrors queueRow
function sendQueueRow(t) {
  const cls =
    t.status === "failed" ? "error" : t.status === "queued" ? "pending" : "converting";
  const li = document.createElement("li");

  const main = document.createElement("div");
  main.className = "queue-row-main";
  const title = document.createElement("div");
  title.className = "queue-title";
  title.textContent = t.title || "Untitled";
  const status = document.createElement("div");
  status.className = `queue-status ${cls}`;
  status.textContent = sendStatusText(t);
  if (t.status === "failed" && t.error) status.title = t.error;
  main.append(title, status);

  const meta = document.createElement("div");
  meta.className = "queue-meta";
  meta.textContent = t.author || "";

  const bar = document.createElement("div");
  bar.className = `queue-progress ${cls}`;
  if (t.status !== "queued" && t.status !== "failed") {
    const fill = document.createElement("div");
    fill.className = "queue-progress-fill";
    const pct =
      t.status === "sending"
        ? t.total > 0
          ? Math.round((t.done / t.total) * 100)
          : 0
        : 100; // sent / skipped → full
    fill.style.width = `${pct}%`;
    bar.appendChild(fill);
  }

  li.append(main, meta, bar);
  return li;
}

// Status text for a send task — live bytes while sending, else a terminal word.
function sendStatusText(t) {
  switch (t.status) {
    case "sending":
      return t.total > 0 ? `${formatBytes(t.done)} / ${formatBytes(t.total)}` : "Sending…";
    case "sent":
      return "Sent";
    case "skipped":
      return "Skipped";
    case "failed":
      return "Failed";
    default:
      return "Queued";
  }
}

// "Step · NN%" text for an in-flight conversion (falls back to a generic label
// before the first progress tick lands).
function convStatusText(prog) {
  if (!prog || !prog.label) return "Converting…";
  return `${prog.label} · ${convFillPct(prog)}%`;
}

// Clamp a progress fraction to an integer percent for bar width / status text.
function convFillPct(prog) {
  return Math.round(Math.min(1, Math.max(0, prog?.fraction ?? 0)) * 100);
}

// Patch one queue row's fill and status text in place for a progress tick.
function updateQueueRowProgress(bookId) {
  const li = document.querySelector(`#queue-list li[data-book-id="${bookId}"]`);
  if (!li) return false;
  const prog = state.convProgress[bookId];
  const fill = li.querySelector(".queue-progress-fill");
  if (fill) fill.style.width = `${convFillPct(prog)}%`;
  const status = li.querySelector(".queue-status.converting");
  if (status) status.textContent = convStatusText(prog);
  return true;
}

// ---------------------------------------------------------------------------
// Book actions
// ---------------------------------------------------------------------------

const BOOK_ACTIONS = [
  {
    id: "read",
    group: "open",
    scopes: ["book"],
    label: () => "Read",
    run: (c) => openReader(c.book),
  },
  {
    id: "open-series",
    group: "open",
    scopes: ["series"],
    label: () => "Open series",
    run: (c) => enterSeries(c.series),
  },
  {
    id: "edit-metadata",
    group: "modify",
    scopes: ["book", "books", "series"],
    bar: true,
    label: (c) => ActionMenu.counted("Edit metadata…", c),
    run: (c) =>
      c.eligible.length === 1
        ? openMetadataModal(c.eligible[0])
        : openMetadataModal(c.eligible, { bulk: true }),
  },
  {
    // The full editor writes the source artifact (metadata/cover/TOC), not just
    // the DB row. All three source formats; the editor's rail gates the panels
    // each one can actually back.
    id: "edit-book",
    group: "modify",
    scopes: ["book"],
    when: (c) =>
      ["kfx", "epub", "pdf"].includes(sourceFormat(c.book)) && c.book.status === "done",
    label: () => "Edit book…",
    run: (c) => openEditor(c.book),
  },
  {
    // Splitting reads the EPUB side, and is offered once that side is ready.
    id: "split",
    group: "modify",
    scopes: ["book"],
    when: (c) => Boolean(c.book.epub_path) && formatStatusFor("epub", c.book) === "done",
    label: () => "Split into a series…",
    run: (c) => openSplitModal(c.book),
  },
  {
    id: "send",
    group: "transfer",
    scopes: ["book", "books"],
    bar: true,
    when: () => Boolean(state.device),
    eligible: (b) => b.status === "done" && !state.sentSet.has(b.sha256),
    label: (c) => ActionMenu.counted("Send to Kindle", c),
    run: (c) => sendBooks(c.eligible.map((b) => b.id)),
  },
  {
    id: "unsend",
    group: "transfer",
    scopes: ["book", "books"],
    bar: true,
    when: () => Boolean(state.device),
    eligible: (b) => state.sentSet.has(b.sha256),
    label: (c) => ActionMenu.counted("Remove from Kindle", c),
    run: (c) => unsendBooks(c.eligible),
  },
  {
    id: "export",
    group: "transfer",
    scopes: ["book", "books", "series"],
    bar: true,
    label: (c) => (c.surface === "bar" ? "Export…" : "Export to folder"),
    submenu: (c) => exportMenuItems(c.items),
  },
  {
    id: "finder",
    group: "transfer",
    scopes: ["book"],
    label: () => "Open in Finder",
    run: (c) => openInFinder(c.book.id),
  },
  {
    id: "recrawl",
    group: "rebuild",
    scopes: ["book", "books", "series"],
    // The color cover is fetched by catalogue id, and a book without one has nothing to
    // fetch with — `asin` is the file's own identity and names no catalogue item.
    eligible: (b) => Boolean(b.amazon_asin) && looksLikeRealAsin(b.amazon_asin),
    label: (c) =>
      ActionMenu.counted(c.items.length > 1 ? "Re-fetch covers" : "Re-fetch cover", c),
    // One book and many are two different backend calls, not one called twice:
    // the single fetch reports per-book why it failed, the batch streams
    // progress and reports a summary.
    run: (c) =>
      c.eligible.length === 1 ? recrawlCover(c.eligible[0]) : recrawlCovers(c.eligible),
  },
  {
    // Always full color. The JXR encoder auto-collapses a genuinely-grayscale page to
    // `8bppGray`, making color a strict superset at no size cost on B&W books. There is
    // no grayscale/color choice to offer.
    id: "reconvert",
    group: "rebuild",
    scopes: ["book", "books", "series"],
    label: (c) => ActionMenu.counted("Force re-convert", c),
    run: (c) => retryConvertAll(c.eligible),
  },
  {
    id: "remove",
    group: "destroy",
    scopes: ["book", "books"],
    bar: true,
    danger: true,
    label: (c) => ActionMenu.counted("Remove from library", c),
    run: (c) => removeBooks(c.eligible),
  },
];

function wireContextMenu() {
  // Suppress the WebView's native right-click menu (Reload, Open Frame in New
  document.addEventListener("contextmenu", (e) => {
    const t = e.target;
    const inField =
      t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable);
    if (!inField) e.preventDefault();
  });
  document.addEventListener("click", () => ActionMenu.close());
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") ActionMenu.close();
  });
}

// What a right-click acts on: the whole selection where the clicked book is
// part of a multi-selection, and the book alone where it is not, which
// `onItemContext` has reset the selection to.
function bookContext(b) {
  if (booksSelection.count() > 1) return { kind: "books", items: selectedBooks() };
  return { kind: "book", items: [b], book: b };
}

function openContextMenu(x, y, b) {
  ActionMenu.open(x, y, BOOK_ACTIONS, bookContext(b));
}

// Right-click on a series collection tile/row (grouped top level). A collection
function openSeriesContextMenu(x, y, entry) {
  ActionMenu.open(x, y, BOOK_ACTIONS, {
    kind: "series",
    series: entry.name,
    // EVERY book in the series from the full library, past the post-filter members this
    // tile shows, and a series rename or author fix reaches the whole series.
    items: membersOfSeries(state.books, entry.name),
  });
}

// ---------------------------------------------------------------------------
// Export (copy a chosen format out to an external folder)
// ---------------------------------------------------------------------------

// The export formats, in menu order. KFX is universal (every book has it); EPUB
const EXPORT_FORMATS = [
  ["epub", "EPUB"],
  ["pdf", "PDF"],
  ["kfx", "KFX"],
  ["txt", "TXT"],
];

// Whether `b`'s `fmt` file is present and converted, ready for an action to offer it.
function hasFormatReady(b, fmt) {
  if (fmt === "kfx") return formatStatusFor("kfx", b) === "done";
  // TXT is generated from whichever content side is ready — KFX (universal) or EPUB
  // (preferred when present). It is a stored companion file nowhere, and lands with the
  // imported side.
  if (fmt === "txt") {
    return (
      formatStatusFor("kfx", b) === "done" ||
      formatStatusFor("epub", b) === "done"
    );
  }
  if (nonKfxFormat(b) !== fmt) return false;
  return formatStatusFor(fmt, b) === "done";
}

// How many books in `books` can export each format (drives the menu labels).
function exportFormatCounts(books) {
  const counts = Object.fromEntries(EXPORT_FORMATS.map(([fmt]) => [fmt, 0]));
  for (const b of books) {
    for (const [fmt] of EXPORT_FORMATS) {
      if (hasFormatReady(b, fmt)) counts[fmt] += 1;
    }
  }
  return counts;
}

// `[label, fn]` pairs for the formats at least one book in `books` can export —
// e.g. `["EPUB (12)", …]`. Empty when nothing in the selection is ready.
function exportMenuItems(books) {
  const counts = exportFormatCounts(books);
  return EXPORT_FORMATS.filter(([fmt]) => counts[fmt] > 0).map(([fmt, label]) => [
    `${label} (${counts[fmt]})`,
    () => doExport(books, fmt),
  ]);
}

// Pick a destination folder, then copy each eligible book's `fmt` file into it
// (the backend groups them into per-author subfolders). Reports a one-line
// summary; per-book skips are logged to the console.
async function doExport(books, fmt) {
  const eligible = books.filter((b) => hasFormatReady(b, fmt));
  if (eligible.length === 0) {
    showToast(`no ${fmt.toUpperCase()} file ready to export`, true);
    return;
  }
  let dir;
  try {
    dir = await window.api.invoke("library_pick_folder");
  } catch (e) {
    showToast(`export failed: ${e}`, true);
    return;
  }
  if (!dir) return; // user cancelled the folder picker

  let summary;
  try {
    summary = await window.api.invoke("library_export_books", {
      bookIds: eligible.map((b) => b.id),
      format: fmt,
      destDir: dir,
    });
  } catch (e) {
    showToast(`export failed: ${e}`, true);
    return;
  }

  if (summary.errors?.length) console.warn("export skips:", summary.errors);
  const parts = [`exported ${summary.exported} ${fmt.toUpperCase()}`];
  if (summary.skipped) parts.push(`${summary.skipped} skipped`);
  showToast(`${parts.join(" · ")} → ${dir}`, summary.exported === 0);
}

// ---------------------------------------------------------------------------
// Series-as-selection (cmd/shift-click a collection to select its books)
// ---------------------------------------------------------------------------

// Click on a collection tile/row: a modifier (cmd/ctrl/shift) selects its
// member books; a plain click drills into the series.
function onSeriesClick(e, name) {
  if (e.metaKey || e.ctrlKey || e.shiftKey) {
    e.stopPropagation();
    toggleSeriesSelection(name);
  } else {
    enterSeries(name);
  }
}

// Add a collection's visible members to the book selection, or remove them from
// a selection holding all of them. One Export, or any bulk action, spans whole
// series and standalone books together.
function toggleSeriesSelection(name) {
  const visible = sortedBooks(visibleBooks(state.books));
  const members = membersOfSeries(visible, name);
  if (!members.length) return;
  const allSel = members.every((b) => booksSelection.has(b.id));
  for (const b of members) {
    if (allSel) booksSelection.selected.delete(b.id);
    else booksSelection.selected.add(b.id);
  }
  booksSelection.lastClicked = null;
  // Repaints book containers + fires onChange → renderSelectionBar +
  // paintSeriesSelection (which marks this collection selected).
  booksSelection.applyVisuals();
}

// Mark a collection tile or index-row `.selected` when every visible member is in the
// selection, and a cmd-selected series reads as a unit, like a book card. Called on
// every selection change and after each render.
function paintSeriesSelection() {
  const els = $$("#gallery-grid .series-card, #series-list tbody tr[data-series]");
  if (!els.length) return;
  const visible = sortedBooks(visibleBooks(state.books));
  for (const el of els) {
    const members = membersOfSeries(visible, el.dataset.series);
    const all = members.length > 0 && members.every((b) => booksSelection.has(b.id));
    el.classList.toggle("selected", all);
  }
}

// ---------------------------------------------------------------------------
// Selection (multi-select + bulk actions)
// ---------------------------------------------------------------------------

function wireSelection() {
  // The bar's action buttons are rendered, not wired — see renderSelectionBar.
  // Clear isn't an action on the books, it's an action on the bar itself.
  $("#sel-clear").addEventListener("click", clearSelection);
  // How many actions fit on the bar follows the window width, and a resize re-renders
  // it. What overflows moves under More…, and disappears nowhere.
  window.addEventListener("resize", renderSelectionBar);

  // Lasso and click-to-clear on the empty area of main. mousedown separates a
  // drag (→ lasso) from a tap (→ clear selection).
  $("#main").addEventListener("mousedown", onMainMouseDown);

  // Esc clears selection; Cmd/Ctrl-A selects all — dispatched to whichever
  // section (Books or Notes) is active via its SelectionController.
  document.addEventListener("keydown", (e) => {
    const ctrl = activeController();
    if (!ctrl) return;
    if (state.section === "notes" && window.Notebooks.viewerOpen()) return;
    const t = e.target;
    const inField =
      t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable);
    if (e.key === "Escape" && ctrl.count() > 0) {
      ctrl.clear();
      return;
    }
    if ((e.metaKey || e.ctrlKey) && e.key === "a" && !inField) {
      e.preventDefault();
      ctrl.selectAll();
    }
  });
}

// Lasso / empty-click for whichever section is active. The mechanics live in the
// SelectionController; this just routes the mousedown to the active one and skips
// when the target is actionable (a card/row/header handles its own click).
function onMainMouseDown(e) {
  if (e.button !== 0) return; // primary button only
  const ctrl = activeController();
  if (!ctrl) return;
  // Don't start a lasso over the open notebook viewer overlay.
  if (state.section === "notes" && window.Notebooks.viewerOpen()) return;
  if (e.target.closest(ctrl.cfg.skipSelector)) return;
  ctrl.beginLasso(e);
}

// Thin wrappers for the book card and row handlers; the logic is the shared
// controller's. Notes wires its cards straight to its own controller.
function onItemClick(e, b) {
  // A drag that releases back on its origin tile fires a trailing click; swallow
  const dragged = e.target.closest(".just-dragged");
  if (dragged) {
    dragged.classList.remove("just-dragged");
    return;
  }
  booksSelection.click(e, b.id);
  setFocusKey(`book:${b.id}`); // keyboard cursor follows the clicked book
}

/// Context menu (right-click): an existing multi-selection stays for the menu
/// to act on, and anything else resets to the clicked book.
function onItemContext(_e, b) {
  booksSelection.context(b.id);
}

function clearSelection() {
  booksSelection.clear();
}

function selectedBooks() {
  return state.books.filter((b) => booksSelection.has(b.id));
}

// ---------------------------------------------------------------------------
// Keyboard shortcuts + focus cursor (Books section)
// ---------------------------------------------------------------------------

// Focusable tiles in the ACTIVE view, in display order (for cursor movement):
// gallery cards (book + series) or list / series-index rows.
function focusableEls() {
  if (state.view === "gallery") {
    return $$("#gallery-grid .book-card, #gallery-grid .series-card");
  }
  if (displayMode() === "grouped") return $$("#series-list tbody tr");
  return $$("#list tbody tr");
}

// Every focusable element across BOTH the gallery and list DOM, which both persist,
// keeping the focus ring correct after a view switch. Used for painting alone.
function allFocusableEls() {
  return [
    ...$$("#gallery-grid .book-card, #gallery-grid .series-card"),
    ...$$("#list tbody tr"),
    ...$$("#series-list tbody tr"),
  ];
}

// Stable key for a focusable element, surviving re-render: "book:<id>" or
// "series:<name>" (book cards/rows carry data-book-id; series tiles data-series).
function focusKeyOf(el) {
  if (!el) return null;
  if (el.dataset.bookId) return `book:${el.dataset.bookId}`;
  if (el.dataset.series != null) return `series:${el.dataset.series}`;
  return null;
}
function keyBookId(key) {
  return key && key.startsWith("book:") ? Number(key.slice(5)) : null;
}

// Re-apply `.focused` to the tile matching state.focusKey (both DOMs); drop a
// stale focus whose tile is gone (filtered/regrouped away). Called from applyView.
function paintFocus() {
  let found = false;
  for (const el of allFocusableEls()) {
    const on = state.focusKey != null && focusKeyOf(el) === state.focusKey;
    el.classList.toggle("focused", on);
    if (on) found = true;
  }
  if (!found) state.focusKey = null;
}

// Point the cursor at `key` (e.g. after a click) and repaint, without scrolling.
function setFocusKey(key) {
  state.focusKey = key;
  state.focusAnchorKey = null;
  paintFocus();
}

// Columns in the gallery grid, read from its computed track list (robust to the
// responsive auto-fill). 1 in list view (linear).
function galleryColumns() {
  if (state.view !== "gallery") return 1;
  const grid = $("#gallery-grid");
  if (!grid) return 1;
  const tracks = getComputedStyle(grid).gridTemplateColumns;
  const n = tracks ? tracks.split(" ").filter(Boolean).length : 1;
  return Math.max(1, n);
}

// Move the cursor one step in `dir`, scroll it into view, repaint. With `extend`
// (Shift) it grows a selection range over the book tiles it passes.
function moveFocus(dir, extend) {
  const els = focusableEls();
  if (els.length === 0) return;
  let idx = els.findIndex((el) => focusKeyOf(el) === state.focusKey);
  if (idx === -1) {
    idx = 0; // first cursor move just focuses the first tile
  } else {
    const cols = galleryColumns();
    switch (dir) {
      case "left": idx -= 1; break;
      case "right": idx += 1; break;
      case "up": idx -= cols; break;
      case "down": idx += cols; break;
      case "first": idx = 0; break;
      case "last": idx = els.length - 1; break;
      case "pageup": idx -= cols * 3; break;
      case "pagedown": idx += cols * 3; break;
    }
  }
  idx = Math.max(0, Math.min(els.length - 1, idx));
  const el = els[idx];
  state.focusKey = focusKeyOf(el);
  els.forEach((e) => e.classList.toggle("focused", e === el));
  el.scrollIntoView({ block: "nearest", inline: "nearest" });
  if (extend) extendSelectionTo(el);
  else state.focusAnchorKey = null; // a plain move ends a Shift-range
}

// Shift+arrow: extend the selection from the range anchor to the focused tile.
// A series tile (not selectable) just moves the cursor — no selection change.
function extendSelectionTo(el) {
  const bookId = el.dataset.bookId ? Number(el.dataset.bookId) : null;
  if (bookId == null) return;
  if (keyBookId(state.focusAnchorKey) == null) state.focusAnchorKey = `book:${bookId}`;
  booksSelection.selectRangeFromAnchor(keyBookId(state.focusAnchorKey), bookId);
}

// Space: toggle selection of the focused book (no-op on a series tile).
function toggleFocusSelection() {
  const id = keyBookId(state.focusKey);
  if (id == null) return;
  booksSelection.toggle(id);
  booksSelection.lastClicked = id;
  booksSelection.applyVisuals();
  state.focusAnchorKey = null;
}

// Enter: read the focused book, or drill into the focused series.
function activateFocus() {
  const key = state.focusKey;
  if (!key) return;
  if (key.startsWith("book:")) {
    const b = state.books.find((x) => x.id === keyBookId(key));
    if (b) openReader(b);
  } else if (key.startsWith("series:")) {
    enterSeries(key.slice("series:".length));
  }
}

function focusSearch() {
  const s = $("#search-input");
  if (s) {
    s.focus();
    s.select();
  }
}

function openShortcuts() {
  $("#shortcuts-modal").hidden = false;
}
function closeShortcuts() {
  $("#shortcuts-modal").hidden = true;
}

function wireLibraryShortcuts() {
  document.addEventListener("keydown", onLibraryKeydown);
  $("#shortcuts-close").addEventListener("click", closeShortcuts);
  $("#shortcuts-modal .modal-backdrop").addEventListener("click", closeShortcuts);
}

// True when the library — not the reader or a modal — should receive shortcuts.
function libraryKeysReady() {
  return (
    $("#reader-view").hidden &&
    $("#metadata-modal").hidden &&
    $("#settings-modal").hidden
  );
}

function onLibraryKeydown(e) {
  // The cheat sheet, while open, swallows keys (Esc closes it).
  if (!$("#shortcuts-modal").hidden) {
    if (e.key === "Escape") closeShortcuts();
    return;
  }
  if (!libraryKeysReady()) return; // reader / metadata / settings own the keyboard

  const t = e.target;
  const inField =
    t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable);
  const mod = e.metaKey || e.ctrlKey;

  // Modifier combos that work even from a text field.
  if (mod && !e.altKey && (e.key === "f" || e.key === "F")) {
    e.preventDefault();
    focusSearch();
    return;
  }
  if (mod && (e.key === "o" || e.key === "O")) {
    e.preventDefault();
    onAddClick();
    return;
  }
  if (mod && e.key === ",") {
    e.preventDefault();
    openSettings();
    return;
  }

  if (inField || mod) return; // `inField` and `mod` hold the cases below to bare keys

  // `window.ReadingLog.handleKey` takes the reading log's bare keys before the section
  // and view keys below.
  if (state.section === "reading" && window.ReadingLog?.handleKey(e)) {
    e.preventDefault();
    return;
  }

  const books = state.section === "books";
  switch (e.key) {
    // Cursor navigation + activation — Books section only. preventDefault on all
    // of these (incl. Backspace) also blocks the WebView's stray scroll / back-nav.
    case "ArrowLeft":
    case "ArrowRight":
    case "ArrowUp":
    case "ArrowDown":
    case "Home":
    case "End":
    case "PageUp":
    case "PageDown":
    case "Enter":
    case " ":
    case "Backspace":
      if (!books) return;
      e.preventDefault();
      handleBooksNavKey(e);
      return;
    // App-level keys (any Books/Notes context).
    case "/":
      e.preventDefault();
      focusSearch();
      return;
    case "1":
      setView("gallery");
      return;
    case "2":
      setView("list");
      return;
    case "g":
    case "G":
      if (books) setGroup(state.group === "series" ? "none" : "series");
      return;
    case "[":
      setSection("books");
      return;
    case "]":
      setSection("notes");
      return;
    case "\\":
      setSection("device");
      return;
    case "?":
      e.preventDefault();
      openShortcuts();
      return;
  }
}

function handleBooksNavKey(e) {
  switch (e.key) {
    case "ArrowLeft": moveFocus("left", e.shiftKey); break;
    case "ArrowRight": moveFocus("right", e.shiftKey); break;
    case "ArrowUp": moveFocus("up", e.shiftKey); break;
    case "ArrowDown": moveFocus("down", e.shiftKey); break;
    case "Home": moveFocus("first", e.shiftKey); break;
    case "End": moveFocus("last", e.shiftKey); break;
    case "PageUp": moveFocus("pageup", e.shiftKey); break;
    case "PageDown": moveFocus("pagedown", e.shiftKey); break;
    case "Enter": activateFocus(); break;
    case " ": toggleFocusSelection(); break;
    case "Backspace": if (state.seriesView != null) exitSeries(); break;
  }
}

// The selection bar is a view of BOOK_ACTIONS over the current selection.
function renderSelectionBar() {
  const bar = $("#selection-bar");
  const n = booksSelection.count();
  if (n === 0) {
    bar.hidden = true;
    return;
  }
  bar.hidden = false;
  $("#selection-count").textContent = `${n} selected`;
  const items = selectedBooks();
  // One selected book is a book, past a one-book batch: it takes the
  // single-book actions (Read, Split, Open in Finder) a right-click gives.
  const ctx =
    items.length === 1
      ? { kind: "book", items, book: items[0] }
      : { kind: "books", items };
  ActionMenu.renderBar($("#selection-actions"), BOOK_ACTIONS, ctx);
}

// Delete every one of `books` from the attached Kindle. The device is addressed by
// FILENAME, and a book whose `sent` row names no file is skipped.
async function unsendBooks(books) {
  const filenameOf = new Map(
    state.sent.filter((r) => r.kind === "sent").map((r) => [r.sha256, r.filename]),
  );
  const pairs = books
    .map((b) => ({ filename: filenameOf.get(b.sha256), title: b.title }))
    .filter((p) => p.filename);
  if (pairs.length === 0) return;
  await deleteFromDevice(
    pairs.map((p) => p.filename),
    pairs.map((p) => p.title),
  );
}

// Queue a forced re-convert for every book in `books`, always full colour. It
// invokes the command directly, past `retryConvert`, and the batch shows one
// summary toast.
async function retryConvertAll(books) {
  if (!books.length) return;
  let failed = 0;
  for (const b of books) {
    try {
      await window.api.invoke("conversion_retry", { bookId: b.id });
    } catch (e) {
      failed += 1;
      if (failed === 1) showToast(`re-convert failed for "${b.title}": ${e}`, true);
      console.error("re-convert failed:", b.id, e);
    }
  }
  const ok = books.length - failed;
  if (ok) showToast(`re-converting ${ok} book${ok === 1 ? "" : "s"}…`);
}

// Re-fetch color covers for every book in `books`. The backend runs them one at a time
// — an Amazon round-trip plus an EPUB/KFX cover rewrite per book — and streams
// `library:recrawl-progress`; the gallery refreshes at the end.
let recrawlInFlight = false;
async function recrawlCovers(books) {
  if (recrawlInFlight) return;
  const n = books.length;
  if (n === 0) return;
  if (
    !confirm(
      `Re-fetch ${n === 1 ? "this cover" : `${n} covers`} from Amazon?\n\n` +
        "Replaces the current cover with the latest high-resolution art.",
    )
  ) {
    return;
  }

  recrawlInFlight = true;
  const prog = $("#sel-recrawl-progress");
  if (prog) {
    prog.hidden = false;
    prog.textContent = `Fetching covers 0/${n}…`;
  }
  let summary;
  try {
    summary = await window.api.invoke("library_recrawl_covers", {
      bookIds: books.map((b) => b.id),
    });
  } catch (e) {
    showToast(`cover fetch error: ${e}`, true);
    return;
  } finally {
    recrawlInFlight = false;
    if (prog) prog.hidden = true;
  }

  // Covers land at the same sidecar paths, leaving each <img> src unchanged. Re-pulling
  // rows brings the fresh per-book `cover_rev` (new mtimes), whose `?v=` change reloads
  // the recrawled tiles alone.
  await refresh();
  const parts = [`${summary.updated} updated`];
  if (summary.failed) parts.push(`${summary.failed} no cover`);
  if (summary.no_asin) parts.push(`${summary.no_asin} skipped`);
  showToast(parts.join(" · "), summary.updated === 0);
}

// Progress for a bulk cover re-fetch, inline in the selection bar, where a multi-minute
// run reads as live.
function subscribeRecrawlProgress() {
  window.api.listen("library:recrawl-progress", (e) => {
    const r = e?.payload;
    if (!r) return;
    const prog = $("#sel-recrawl-progress");
    if (!prog) return;
    prog.hidden = false;
    prog.textContent = `Fetching covers ${r.done}/${r.total}…`;
  });
}

async function removeBooks(books) {
  if (books.length === 0) return;
  const msg =
    books.length === 1
      ? `Remove "${books[0].title}" from the library?`
      : `Remove ${books.length} books from the library?`;
  if (
    !confirm(
      `${msg}\n\nThis deletes the cached EPUB and KFX. The Kindle is untouched.`,
    )
  ) {
    return;
  }
  let failed = 0;
  for (const b of books) {
    try {
      await window.api.invoke("library_remove", { bookId: b.id });
    } catch (e) {
      // library_remove surfaces IO errors (e.g. Spotlight/Books.app
      failed += 1;
      if (failed === 1) showToast(`remove failed for "${b.title}": ${e}`, true);
      console.error("remove failed:", b.id, e);
    }
  }
  // One VACUUM for the whole batch (not one per book), and only if something
  // was actually removed.
  if (failed < books.length) await compactLibrary();
  booksSelection.selected.clear();
  booksSelection.lastClicked = null;
  await refresh();
}

// Reclaim DB file space freed by deletions (VACUUM). Best-effort: a transient failure
// logs and surfaces as no delete error. Called once per delete operation — see
// removeBooks.
async function compactLibrary() {
  try {
    await window.api.invoke("library_compact");
  } catch (e) {
    console.error("library compact failed:", e);
  }
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

// Open a book in the built-in reader. `reader.js`, an ES module, installs
// `window.sidleReader`; it loads after this classic script and is present by the time a
// card is clicked.
let openReqSeq = 0;
// Open the Calibre-style book editor (window.sidleEditor) for KFX- and EPUB-source
// books. The caller gates the menu item, and this is reached for those alone.
function openEditor(b) {
  if (!window.sidleEditor) {
    showToast("editor not ready", true);
    return;
  }
  window.sidleEditor.open(b.id);
}

// Open a book in the reader, reporting the load on the status line. `b` needs
async function openReader(b) {
  if (!window.sidleReader) {
    showToast("reader not ready", true);
    return;
  }
  const seq = ++openReqSeq;
  state.opening = b.title || "book";
  renderQueue(); // paint "Opening …" before the (possibly slow) KFX load
  try {
    await window.sidleReader.open(b.id);
  } finally {
    if (seq === openReqSeq) {
      state.opening = null;
      renderQueue();
    }
  }
}

// Queue a re-convert through `conversion_retry`: a forced pass over a `done` book, or a
// second run at a failed one.
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
  // kind === "updated" — re-pull brings this book's new `cover_rev`, busting
  // just its tile.
  showToast(`cover updated: ${b.title}`);
  await refresh();
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
// Library settings (the ⚙ in the status bar): show where the library lives and
// move it to / adopt one in another folder. Both repoint the root (config.json)

let relocatePending = null; // { mode: "move" | "use", dest: string }

async function openSettings() {
  resetRelocateConfirm();
  $("#settings-status").hidden = true;
  $("#settings-modal").hidden = false;
  // The Kindle-folders rows are misc.js's — loaded fresh on each open, showing what is
  // on disk past what a cancelled edit left behind.
  window.Misc?.editCollections?.();
  try {
    const loc = await window.api.invoke("library_location");
    $("#settings-location").textContent = loc.is_default ? `${loc.root}  (default)` : loc.root;
  } catch {
    $("#settings-location").textContent = "(unavailable)";
  }
}

function closeSettings() {
  $("#settings-modal").hidden = true;
}

function resetRelocateConfirm() {
  relocatePending = null;
  $("#settings-confirm").hidden = true;
  const ok = $("#settings-confirm-ok");
  ok.disabled = false;
  // Restore borrows this box for a second way to say yes and a louder primary
  // button; hand both back, or the next move/use prompt inherits them.
  ok.textContent = "Confirm & restart";
  ok.classList.remove("btn-primary");
  const keep = $("#settings-confirm-keep");
  keep.hidden = true;
  keep.disabled = false;
}

async function pickRelocate(mode) {
  const dest = await window.api.invoke("library_pick_folder");
  if (!dest) return;
  // From a known state: a restore prompt left open lends this one its two
  // yes-buttons.
  resetRelocateConfirm();
  relocatePending = { mode, dest };
  $("#settings-confirm-text").textContent =
    mode === "move"
      ? `Move your library to:\n${dest}\n\nsidle verifies, removes the old location, and restarts.`
      : `Use the library in:\n${dest}\n\nsidle restarts. Nothing is copied.`;
  $("#settings-confirm").hidden = false;
  $("#settings-status").hidden = true;
}

// `keepPrevious` only means anything to restore: it is which of the box's two
// yes-buttons was pressed — set the replaced library aside as an undo, or delete
// it and get its space back.
async function confirmRelocate(keepPrevious) {
  if (!relocatePending) return;
  const { mode, dest } = relocatePending;
  // Both yes-buttons go down together: two live ways to commit during a running
  // restore is two restores.
  $("#settings-confirm-ok").disabled = true;
  $("#settings-confirm-keep").disabled = true;
  setSettingsStatus(
    mode === "move" ? "Copying library…"
    : mode === "restore" ? "Restoring… this can take a while for a large backup."
    : "Switching library…",
  );
  fileopInFlight = true; // only restore emits fileop; harmless for move/use
  try {
    if (mode === "move") {
      await window.api.invoke("library_relocate_move", { dest });
    } else if (mode === "restore") {
      await window.api.invoke("library_restore", { src: dest, keepPrevious });
    } else {
      await window.api.invoke("library_relocate_use", { dir: dest });
    }
    // On success the app restarts and this webview reloads, reaching here in the
    // failure case.
    setSettingsStatus("Restarting…");
  } catch (e) {
    $("#settings-confirm-ok").disabled = false;
    $("#settings-confirm-keep").disabled = false;
    setSettingsStatus(String(e?.message ?? e), true);
    fileopInFlight = false;
    state.fileop = null;
    renderQueue();
  }
}

function setSettingsStatus(msg, isError = false) {
  const el = $("#settings-status");
  el.textContent = msg;
  el.classList.toggle("error", isError);
  el.hidden = false;
}

// Backup is non-destructive — a direct action, not part of the confirm/restart
// flow. Picks a destination, writes the archive, reports the counts.
async function doBackup() {
  const dest = await window.api.invoke("library_backup_pick_dest");
  if (!dest) return;
  resetRelocateConfirm();
  const btn = $("#settings-backup");
  btn.disabled = true;
  fileopInFlight = true;
  setSettingsStatus("Backing up… this can take a while for a large library.");
  try {
    const r = await window.api.invoke("library_backup", { dest });
    setSettingsStatus(
      `Backed up ${plural(r.books, "book")} and ${plural(r.annotations, "annotation")} to:\n${r.path}`,
    );
  } catch (e) {
    setSettingsStatus(String(e?.message ?? e), true);
  } finally {
    btn.disabled = false;
    fileopInFlight = false;
    state.fileop = null;
    renderQueue();
  }
}

// `pickRestore` is destructive and restarts, routing through the confirm flow
// `relocatePending` drives.
async function pickRestore() {
  const src = await window.api.invoke("library_restore_pick_src");
  if (!src) return;
  resetRelocateConfirm(); // same known state the relocate prompts start from
  relocatePending = { mode: "restore", dest: src };
  $("#settings-confirm-text").textContent =
    `Restore from:\n${src}\n\nThis replaces your current library with the one in the backup, and sidle restarts.\n\n` +
    `Replace: the current library is deleted once the restored one is in place, and its space comes back.\n` +
    `Keep a copy: it is set aside next to your library as a dated folder instead — an undo you delete yourself, ` +
    `and until you do the disk holds both libraries.`;
  const ok = $("#settings-confirm-ok");
  ok.textContent = "Replace & restart";
  ok.classList.add("btn-primary");
  $("#settings-confirm-keep").hidden = false;
  $("#settings-confirm").hidden = false;
  $("#settings-status").hidden = true;
}

// Merge is additive and non-destructive, with no replace and no restart: a
// direct action like backup, outside the confirm/restart flow. It picks a
// .sidlebak, folds its books and notebooks in, and re-lists.
async function doMerge() {
  const src = await window.api.invoke("library_merge_pick_src");
  if (!src) return;
  resetRelocateConfirm();
  const btn = $("#settings-merge");
  btn.disabled = true;
  fileopInFlight = true;
  setSettingsStatus("Merging… this can take a while for a large backup.");
  try {
    const r = await window.api.invoke("library_merge", { src });
    const parts = [`${plural(r.books_added, "book")} added`];
    if (r.books_updated) parts.push(`${plural(r.books_updated, "book")} updated`);
    if (r.annotations_added) parts.push(`${plural(r.annotations_added, "highlight")} added`);
    if (r.notebooks_added) parts.push(`${plural(r.notebooks_added, "notebook")} added`);
    if (r.ink_added) parts.push(`${plural(r.ink_added, "ink page")} added`);
    setSettingsStatus(`Merged from:\n${r.path}\n\n${parts.join(", ")}.`);
    await refresh();
  } catch (e) {
    setSettingsStatus(String(e?.message ?? e), true);
  } finally {
    btn.disabled = false;
    fileopInFlight = false;
    state.fileop = null;
    renderQueue();
  }
}

function plural(n, word) {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
}

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
    pill.addEventListener("click", () => {
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

  // Format facet: companion ⇄ source mode toggle.
  $(".filter-dropdown-mode-cb").addEventListener("change", (e) => {
    setFormatFacetMode(e.target.checked ? "source" : "companion");
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
      label.textContent = `${baseLabel}: ${facetOptionLabel(facet, value)}`;
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
    case "formats":  return "Format";
    default:         return facet;
  }
}

function openFilterDropdown(facet, anchorPill) {
  openDropdownFacet = facet;
  dropdownSearch = "";
  const dd = $("#filter-dropdown");
  dd.hidden = false;
  dd.querySelector(".filter-dropdown-search").value = "";
  // The source/companion mode toggle is a Format-only control: shown for that facet,
  // with its checked state synced to the current mode. Set before positioning, which
  // measures the popover's true height.
  const modeWrap = dd.querySelector(".filter-dropdown-mode");
  modeWrap.hidden = facet !== "formats";
  dd.querySelector(".filter-dropdown-mode-cb").checked =
    state.formatFacetMode === "source";
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
  // Match against both the code and its display label: typing "english" finds the "en"
  // option, whose visible label is "English".
  const filtered = needle
    ? all.filter(
        ([v]) =>
          v.toLowerCase().includes(needle) ||
          facetOptionLabel(facet, v).toLowerCase().includes(needle),
      )
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

    const optLabel = facetOptionLabel(facet, value);
    const lbl = document.createElement("span");
    lbl.className = "opt-label";
    lbl.textContent = optLabel;
    lbl.title = optLabel;

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

  // Populate the popover key list. Rebuilt each time, with .active fresh.
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

let metadataBook = null;
// When non-null, the modal is in bulk mode editing this array of books. Bulk
// and single modes are mutually exclusive (one is always null).
let metadataBulk = null;

// Mirror of cover_fetch::looks_like_real_amazon_asin: a real Amazon ASIN is 10 chars,
// uppercase letters and digits. A synthesized file identity is 32 chars, and length
// plus charset separates them.
function looksLikeRealAsin(s) {
  return /^[A-Z0-9]{10}$/.test(s);
}

function wireMetadataModal() {
  $("#metadata-cancel").addEventListener("click", closeMetadataModal);
  $("#metadata-form").addEventListener("submit", (e) => {
    e.preventDefault();
    submitMetadataForm();
  });
  $("#metadata-cover-change").addEventListener("click", onCoverChangeClick);
  $("#metadata-cover-refetch").addEventListener("click", onCoverRefetchClick);
  $("#asin-search").addEventListener("click", onAsinSearchClick);
  $("#metadata-form").amazon_asin.addEventListener("input", renderAsinHint);
  // "↻" regenerate buttons: re-render a romaji field from its source (title /
  // author) via the engine. The user reviews/corrects before saving.
  for (const btn of $("#metadata-form").querySelectorAll(".romaji-regen")) {
    btn.addEventListener("click", onRomajiRegen);
  }

  // Snapshot each input's placeholder: bulk mode swaps to "Leave unchanged", and single
  // mode restores the original hint.
  for (const el of $("#metadata-form").querySelectorAll("input")) {
    el.dataset.ph = el.placeholder || "";
  }

  // Esc closes; Cmd/Ctrl+Enter submits. Scoped to the modal element, clear of the
  // global selection and context-menu shortcuts.
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

// Fields the bulk editor can touch (everything except per-book-unique title +
// asin, and the cover). Order is irrelevant.
const BULK_FIELDS = [
  "author",
  "language",
  "reading_layout",
  "publisher",
  "published_at",
  "series_name",
  "series_index",
  "tags",
];

// The "Reading layout" select value for a book: its explicit writing_mode if
// one was set, else "" (Auto — let the converter derive the axis from source).
// `ppd` is deliberately NOT consulted: it encodes only the page-turn (rtl/ltr)
function readingLayoutValue(book) {
  return book.writing_mode || "";
}

// Open the editor for one book (single mode) or, with { bulk: true }, for an
// array of books (bulk mode). The two modes share the modal; a `.bulk` class
// on the panel hides per-book-unique fields and collapses the layout.
function openMetadataModal(arg, opts = {}) {
  const form = $("#metadata-form");
  const panel = form; // the <form> carries the .modal-panel class

  if (opts.bulk) {
    const books = arg || [];
    if (books.length === 0) return;
    metadataBulk = books;
    metadataBook = null;
    panel.classList.add("bulk");
    const n = books.length;
    $("#metadata-title").textContent = `Edit metadata · ${n} book${n === 1 ? "" : "s"}`;
    $("#metadata-submit").textContent = `Apply to ${n}`;
    $("#field-tags-label").textContent = "Add tags";

    // Every editable field starts empty → "leave unchanged".
    for (const name of BULK_FIELDS) form[name].value = "";
    setBulkPlaceholders(true);
    // Title, the identifiers and romaji are per-book-unique and hide in bulk. Disabling
    // them exempts them from native required-validation and keeps them off the submit;
    // the display-only one empties.
    form.title.disabled = true;
    form.amazon_asin.disabled = true;
    form.content_id.value = "";
    form.title_romaji.disabled = true;
    form.author_romaji.disabled = true;

    $("#metadata-modal").hidden = false;
    setTimeout(() => form.author.focus(), 0);
    return;
  }

  // Single-book mode.
  const book = arg;
  metadataBook = book;
  metadataBulk = null;
  panel.classList.remove("bulk");
  $("#metadata-title").textContent = "Edit metadata";
  $("#metadata-submit").textContent = "Save";
  $("#field-tags-label").textContent = "Tags";
  form.title.disabled = false;
  form.amazon_asin.disabled = false;
  form.title_romaji.disabled = false;
  form.author_romaji.disabled = false;
  setBulkPlaceholders(false);

  form.title.value = book.title || "";
  form.author.value = book.author || "";
  form.title_romaji.value = book.title_romaji || "";
  form.author_romaji.value = book.author_romaji || "";
  form.language.value = book.language || "";
  form.reading_layout.value = readingLayoutValue(book); // "" = Auto
  form.publisher.value = book.publisher || "";
  form.published_at.value = book.published_at || "";
  form.series_name.value = book.series_name || "";
  form.series_index.value =
    book.series_index != null && Number.isFinite(book.series_index)
      ? String(book.series_index)
      : "";
  // Tags display as comma-joined. Canonicalization happens on the backend, and case and
  // duplicates clean themselves up on save.
  form.tags.value = (book.tags || []).join(", ");
  // Two identifiers, and only one of them is the user's. `amazon_asin` names an
  // item in Amazon's catalogue and does nothing but fetch the color cover;
  // `asin` is what the file itself carries, which the device keys everything on.
  form.amazon_asin.value = book.amazon_asin || "";
  form.content_id.value = book.asin || "";
  renderAsinHint();

  renderCoverPreview(book);

  $("#metadata-modal").hidden = false;
  // Focus the title input for keyboard-first flow.
  setTimeout(() => form.title.focus(), 0);
}

function closeMetadataModal() {
  $("#metadata-modal").hidden = true;
  metadataBook = null;
  metadataBulk = null;
  // Restore a clean single-book state for the next open.
  const form = $("#metadata-form");
  form.classList.remove("bulk");
  form.title.disabled = false;
  form.amazon_asin.disabled = false;
}

// Swap the editable fields' placeholders to "Leave unchanged" in bulk mode;
// restore each input's original placeholder (snapshotted in wireMetadataModal)
// in single mode.
function setBulkPlaceholders(bulk) {
  const form = $("#metadata-form");
  for (const name of BULK_FIELDS) {
    form[name].placeholder = bulk ? "Leave unchanged" : form[name].dataset.ph || "";
  }
}

// Live validity feedback for the ASIN field, gating the Re-fetch button on a
// real-looking ASIN. The recrawl backend returns NoAsin for anything else.
function renderAsinHint() {
  const v = $("#metadata-form").amazon_asin.value.trim();
  const hint = $("#asin-hint");
  const refetch = $("#metadata-cover-refetch");
  const real = looksLikeRealAsin(v);
  if (v === "") {
    hint.textContent = "Only used to fetch the color cover. Never written into the book.";
    hint.className = "field-hint";
  } else if (real) {
    hint.textContent = "✓ Looks like a real ASIN — the color cover can be fetched.";
    hint.className = "field-hint";
  } else {
    hint.textContent = "Not an Amazon ASIN — 10 characters, A–Z and 0–9.";
    hint.className = "field-hint warn";
  }
  refetch.disabled = !real;
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

// Regenerate a romaji field from its source field (title / author) via the
// `library_romanize` command. Engine-only — the user reviews and corrects the
// result before saving. Bound to the "↻" buttons in the metadata modal.
async function onRomajiRegen(e) {
  const form = $("#metadata-form");
  const which = e.currentTarget.dataset.romajiFor; // "title" | "author"
  const target = form[`${which}_romaji`];
  if (!target) return;
  const source = (form[which]?.value || "").trim();
  try {
    target.value = await window.api.invoke("library_romanize", {
      text: source,
      language: form.language.value.trim(),
    });
  } catch (err) {
    showToast(`Romanize failed: ${err}`, true);
  }
}

async function submitMetadataForm() {
  if (metadataBulk) return submitBulkMetadataForm();
  if (!metadataBook) return;
  const form = $("#metadata-form");
  const tagsRaw = form.tags.value.trim();
  const patch = {
    title: form.title.value.trim(),
    author: form.author.value.trim(),
    language: form.language.value.trim(),
    // Reading layout drives the writing-mode axis; a chosen layout re-derives
    writing_mode: form.reading_layout.value || null,
    ppd: form.reading_layout.value
      ? form.reading_layout.value.endsWith("-rl")
        ? "rtl"
        : "ltr"
      : metadataBook.ppd || null,
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
    // Tags split on ASCII or CJK comma (tags have no "Surname, Given" hazard,
    // unlike authors). The backend lowercases + dedupes + drops empties.
    tags:
      tagsRaw === ""
        ? []
        : tagsRaw.split(/[,、]/).map((s) => s.trim()).filter(Boolean),
    // Editable search romaji. A blank field self-heals on the backend, re-rendered from
    // title and author, and clearing it regenerates.
    title_romaji: form.title_romaji.value.trim(),
    author_romaji: form.author_romaji.value.trim(),
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

  // The catalogue ASIN is saved by its own command (library_set_asin): it isn't
  const asin = form.amazon_asin.value.trim();
  const asinChanged = asin !== (metadataBook.amazon_asin || "");
  if (asinChanged && asin !== "" && !looksLikeRealAsin(asin)) {
    showToast("An Amazon ASIN is 10 characters, A–Z and 0–9 — or leave it empty.", true);
    return;
  }

  // Reading layout (writing mode + page direction) is honoured once baked into a fresh
  // KFX, and a change kicks off a force-reconvert after save.
  const layoutChanged =
    (form.reading_layout.value || "") !== readingLayoutValue(metadataBook);
  const bookId = metadataBook.id;

  try {
    const updated = await window.api.invoke("library_update_metadata", {
      bookId,
      patch,
    });
    mergeBookRow(updated);
    if (asinChanged) {
      const withAsin = await window.api.invoke("library_set_asin", {
        bookId,
        asin,
      });
      mergeBookRow(withAsin);
    }
    if (layoutChanged) {
      await retryConvert(bookId);
      showToast("Reading layout changed — reconverting…");
    }
    closeMetadataModal();
    render();
  } catch (e) {
    showToast(`Save failed: ${e}`, true);
  }
}

// Bulk mode: build a sparse patch from the filled-in fields alone, leaving every blank
// field untouched across the batch.
async function submitBulkMetadataForm() {
  const books = metadataBulk;
  if (!books || books.length === 0) return;
  const form = $("#metadata-form");
  const patch = {};
  const setIf = (name, key) => {
    const v = form[name].value.trim();
    if (v !== "") patch[key] = v;
  };
  setIf("author", "author");
  setIf("language", "language");
  // Reading layout (writing mode + derived page direction). Empty = leave
  // unchanged; a chosen layout sets both across every selected book.
  const rl = form.reading_layout.value;
  if (rl) {
    patch.writing_mode = rl;
    patch.ppd = rl.endsWith("-rl") ? "rtl" : "ltr";
  }
  setIf("publisher", "publisher");
  setIf("published_at", "published_at");
  setIf("series_name", "series_name");

  const idxRaw = form.series_index.value;
  if (idxRaw !== "") {
    const idx = Number(idxRaw);
    if (!Number.isFinite(idx) || idx < 0) {
      showToast("Series # must be a non-negative number.", true);
      return;
    }
    patch.series_index = idx;
  }

  const tagsRaw = form.tags.value.trim();
  patch.add_tags =
    tagsRaw === "" ? [] : tagsRaw.split(/[,、]/).map((s) => s.trim()).filter(Boolean);
  patch.remove_tags = []; // v1 is add-only

  const hasScalar = [
    "author",
    "language",
    "writing_mode",
    "ppd",
    "publisher",
    "published_at",
    "series_name",
    "series_index",
  ].some((k) => k in patch);
  if (!hasScalar && patch.add_tags.length === 0) {
    showToast("Nothing to apply — fill at least one field.");
    return;
  }

  try {
    const rows = await window.api.invoke("library_bulk_update_metadata", {
      bookIds: books.map((b) => b.id),
      patch,
    });
    for (const r of rows) mergeBookRow(r);
    // A bulk page-direction change needs each book's KFX rebuilt; fire the
    // reconverts off (the queue serializes them) without blocking the close.
    if ("writing_mode" in patch || "ppd" in patch) {
      for (const r of rows) retryConvert(r.id);
    }
    closeMetadataModal();
    render();
    showToast(`Updated ${rows.length} book${rows.length === 1 ? "" : "s"}.`);
  } catch (e) {
    showToast(`Bulk update failed: ${e}`, true);
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
  await applyCoverFromPath(src);
}

// Set the open book's cover from a local image path (shared by the "Change
// cover…" picker and the drag-and-drop onto the preview). On success bumps the
// cache-buster and refreshes the preview + gallery in place.
async function applyCoverFromPath(src) {
  if (!metadataBook) return;
  try {
    const result = await window.api.invoke("library_set_cover", {
      bookId: metadataBook.id,
      srcPath: src,
    });
    if (result.kind === "updated") {
      // Re-pull rows: this book's `cover_rev` carries a new mtime, reloading the
      // tile and the modal preview from disk past the cached image. Then
      // `metadataBook` re-points at the refreshed row and the preview repaints.
      await refresh();
      const idx = state.books.findIndex((b) => b.id === metadataBook.id);
      if (idx !== -1) metadataBook = state.books[idx];
      renderCoverPreview(metadataBook);
    } else if (result.kind === "failed") {
      showToast(`Cover change failed: ${result.error}`, true);
    }
  } catch (e) {
    showToast(`Cover change failed: ${e}`, true);
  }
}

// "Re-fetch cover" in the editor: persist the ASIN, edits included, then re-pull
// the colour cover from Amazon. It stays in the modal and refreshes the preview
// itself; library_recrawl_cover emits no row-updated.
async function onCoverRefetchClick() {
  if (!metadataBook) return;
  const asin = $("#metadata-form").amazon_asin.value.trim();
  if (!looksLikeRealAsin(asin)) {
    showToast("Enter a real 10-character ASIN first.", true);
    return;
  }
  // Save a changed ASIN first, and the backend recrawl reads it.
  if (asin !== (metadataBook.amazon_asin || "")) {
    try {
      const row = await window.api.invoke("library_set_asin", {
        bookId: metadataBook.id,
        asin,
      });
      mergeBookRow(row);
      metadataBook = row;
    } catch (e) {
      showToast(`Couldn't save ASIN: ${e}`, true);
      return;
    }
  }

  let result;
  try {
    result = await window.api.invoke("library_recrawl_cover", {
      bookId: metadataBook.id,
    });
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
  // updated — re-pull rows, and this book's fresh `cover_rev` busts its gallery tile.
  // Then `metadataBook` re-points and the open modal preview repaints.
  await refresh();
  const idx = state.books.findIndex((b) => b.id === metadataBook.id);
  if (idx !== -1) metadataBook = state.books[idx];
  renderCoverPreview(metadataBook);
  render();
  showToast(`cover updated: ${metadataBook.title}`);
}

// "Search Amazon ↗": open the browser to a marketplace search, where the real ASIN can
// be found and pasted. The backend picks the domain from language.
async function onAsinSearchClick() {
  if (!metadataBook) return;
  try {
    await window.api.invoke("library_amazon_search", { bookId: metadataBook.id });
  } catch (e) {
    showToast(`couldn't open Amazon: ${e}`, true);
  }
}

// ---------------------------------------------------------------------------
// Split a collection into a series
// ---------------------------------------------------------------------------

let splitBook = null;

async function openSplitModal(book) {
  splitBook = book;
  const modal = $("#split-modal");
  $("#split-source").textContent = book.title;
  $("#split-series").value = "";
  $("#split-rows").innerHTML = "";
  $("#split-note").hidden = true;
  $("#split-progress").hidden = true;
  setSplitBusy(true, "Reading the book…");
  modal.hidden = false;

  let proposal;
  try {
    proposal = await window.api.invoke("omnibus_propose", { bookId: book.id });
  } catch (e) {
    closeSplitModal();
    showToast(`${e}`, true);
    return;
  }
  // The modal may have been dismissed while the read was in flight.
  if (splitBook?.id !== book.id) return;

  if (!proposal.volumes.length) {
    closeSplitModal();
    showToast("this book shows no volumes to split into");
    return;
  }

  $("#split-series").value = proposal.series_name;
  renderSplitRows(proposal.volumes);
  if (proposal.existing_in_series > 0) {
    const n = proposal.existing_in_series;
    const note = $("#split-note");
    note.textContent = `${n} book${n === 1 ? "" : "s"} already in “${proposal.series_name}”. A volume whose number is taken there is left alone.`;
    note.hidden = false;
  }
  setSplitBusy(false);
  setTimeout(() => $("#split-series").focus(), 0);
}

// One row per proposed volume: keep it or not, its number, its title, and how
// many of the collection's pages it spans (which the user can't change — the
// spans tile the book by construction).
function renderSplitRows(volumes) {
  const tbody = $("#split-rows");
  tbody.innerHTML = "";
  for (const v of volumes) {
    const tr = document.createElement("tr");
    tr.dataset.spineIndex = String(v.spine_index);
    tr.dataset.documents = String(v.documents);
    tr.dataset.cover = v.cover || "";

    const keep = document.createElement("td");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = true;
    box.className = "split-keep-box";
    keep.className = "split-keep";
    keep.appendChild(box);

    const num = document.createElement("td");
    num.className = "split-num";
    const numInput = document.createElement("input");
    numInput.type = "number";
    numInput.step = "any";
    numInput.min = "0";
    numInput.value = String(v.number);
    // A number the splitter counted, past the volume's own label, is the one
    // worth a second look.
    if (v.counted) {
      numInput.classList.add("counted");
      numInput.title = "counted from the volume before — this book didn't say";
    }
    num.appendChild(numInput);

    const title = document.createElement("td");
    const titleInput = document.createElement("input");
    titleInput.type = "text";
    titleInput.className = "split-title-input";
    titleInput.value = v.title;
    title.appendChild(titleInput);

    const docs = document.createElement("td");
    docs.className = "split-docs";
    docs.textContent = String(v.documents);

    tr.append(keep, num, title, docs);
    tbody.appendChild(tr);
  }
  updateSplitFootnote();
}

// The volumes as the form reads: the plan `omnibus_split` is handed.
function splitPlanFromForm() {
  const volumes = [];
  for (const tr of $("#split-rows").querySelectorAll("tr")) {
    if (!tr.querySelector(".split-keep-box").checked) continue;
    volumes.push({
      spine_index: Number(tr.dataset.spineIndex),
      documents: Number(tr.dataset.documents),
      cover: tr.dataset.cover || null,
      number: Number(tr.querySelector(".split-num input").value),
      title: tr.querySelector(".split-title-input").value.trim(),
      counted: false,
    });
  }
  return { series_name: $("#split-series").value.trim(), volumes };
}

function updateSplitFootnote() {
  const n = $("#split-rows").querySelectorAll(".split-keep-box:checked").length;
  $("#split-submit").textContent = n === 1 ? "Split into 1 book" : `Split into ${n} books`;
  $("#split-submit").disabled = n === 0;
}

async function submitSplit() {
  if (!splitBook) return;
  const plan = splitPlanFromForm();
  if (!plan.series_name) {
    showToast("the series needs a name — every volume is grouped by it", true);
    $("#split-series").focus();
    return;
  }
  if (plan.volumes.some((v) => !v.title)) {
    showToast("every volume needs a title", true);
    return;
  }

  const book = splitBook;
  setSplitBusy(true, `Writing ${plan.volumes.length} volumes…`);
  showSplitProgress(0, plan.volumes.length, "");
  let summary;
  try {
    summary = await window.api.invoke("omnibus_split", {
      bookId: book.id,
      plan,
    });
  } catch (e) {
    setSplitBusy(false);
    $("#split-progress").hidden = true;
    showToast(`${e}`, true);
    return;
  }

  closeSplitModal();
  await refresh();
  // Land on the series just made, grouping by series from any prior view.
  setGroup("series");
  enterSeries(summary.series_name);

  const failed = summary.volumes.filter((v) => v.error);
  const skipped = summary.volumes.filter((v) => v.duplicate);
  const made = summary.volumes.length - failed.length - skipped.length;
  let msg = `${made} volume${made === 1 ? "" : "s"} added to “${summary.series_name}”`;
  if (skipped.length) msg += `, ${skipped.length} already there`;
  if (failed.length) msg += `, ${failed.length} failed: ${failed[0].error}`;
  showToast(msg, failed.length > 0);
}

function showSplitProgress(done, total, title) {
  const wrap = $("#split-progress");
  wrap.hidden = false;
  const pct = total ? Math.round((done / total) * 100) : 0;
  $("#split-bar-fill").style.width = `${pct}%`;
  $("#split-progress-label").textContent = title
    ? `${done + 1} of ${total} · ${title}`
    : `preparing ${total} volumes…`;
}

// Disable the form while the backend is working; `label` (when given) says what
// it is doing.
function setSplitBusy(busy, label) {
  const panel = $("#split-modal .split-panel");
  panel.classList.toggle("busy", busy);
  $("#split-submit").disabled = busy;
  $("#split-series").disabled = busy;
  for (const el of $("#split-rows").querySelectorAll("input")) el.disabled = busy;
  if (label) $("#split-submit").textContent = label;
  else updateSplitFootnote();
}

function closeSplitModal() {
  $("#split-modal").hidden = true;
  splitBook = null;
}

function wireSplitModal() {
  $("#split-cancel").addEventListener("click", closeSplitModal);
  $("#split-submit").addEventListener("click", submitSplit);
  // Delegated: the row list rebuilds on every open, stacking no listeners on the tbody.
  $("#split-rows").addEventListener("change", updateSplitFootnote);
  $("#split-modal .modal-backdrop").addEventListener("click", () => {
    // A split in flight must not be dismissed out from under itself.
    if (!$("#split-submit").disabled) closeSplitModal();
  });
  $("#split-modal").addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      if (!$("#split-submit").disabled) closeSplitModal();
    } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      submitSplit();
    }
  });
  window.api.listen("library:split-progress", (e) => {
    const p = e.payload;
    if (splitBook?.id !== p.book_id) return;
    showSplitProgress(p.done, p.total, p.title);
  });
}

// ---------------------------------------------------------------------------
// Inline (list-view) metadata editing
// ---------------------------------------------------------------------------

// A MetadataPatch mirroring the book as it stands, which the modal sends for an
// unchanged form. The caller overrides exactly one field.
function fullPatchFromBook(b) {
  return {
    title: b.title || "",
    author: b.author || "",
    language: b.language || "",
    ppd: b.ppd || null, // unchanged here → no force-reconvert
    writing_mode: b.writing_mode || null,
    publisher: b.publisher || null,
    published_at: b.published_at || null,
    series_name: b.series_name || null,
    series_index:
      b.series_index != null && Number.isFinite(b.series_index) ? b.series_index : null,
    tags: b.tags || [],
    title_romaji: b.title_romaji || "",
    author_romaji: b.author_romaji || "",
  };
}

// Parse the Series cell's single text field back into its two DB columns,
function parseSeriesCell(text) {
  const s = text.trim();
  if (s === "") return { name: null, index: null };
  const m = s.match(/^(.*?)\s*#\s*(\d+(?:\.\d+)?)\s*$/);
  if (m && m[1].trim() !== "") {
    return { name: m[1].trim(), index: Number(m[2]) };
  }
  return { name: s, index: null };
}

async function commitInlineEdit(book, key, value) {
  const patch = fullPatchFromBook(book);
  const v = value.trim();
  switch (key) {
    case "title":
      if (v === "") {
        showToast("Title cannot be empty.", true);
        throw new Error("empty title"); // rejects → TableView leaves the old value
      }
      patch.title = v;
      patch.title_romaji = ""; // blank self-heals: regenerated from the new title
      break;
    case "author":
      patch.author = v;
      patch.author_romaji = ""; // regenerated from the new author
      break;
    case "series": {
      // The Series cell is really two DB fields shown as "Name #index" (see
      const parsed = parseSeriesCell(value);
      patch.series_name = parsed.name;
      patch.series_index = parsed.index;
      break;
    }
    case "publisher":
      patch.publisher = v === "" ? null : v;
      break;
    case "published_at":
      patch.published_at = v === "" ? null : v;
      break;
    case "language":
      patch.language = v; // canonicalized backend-side (en-US → en, zh-TW → zh-Hant)
      break;
    case "tags":
      // Split on ASCII or CJK comma (same as the modal); the backend lowercases,
      // dedupes, and drops empties.
      patch.tags = v === "" ? [] : v.split(/[,、]/).map((s) => s.trim()).filter(Boolean);
      break;
    default:
      return; // not an editable column
  }

  try {
    const updated = await window.api.invoke("library_update_metadata", {
      bookId: book.id,
      patch,
    });
    mergeBookRow(updated);
    render();
  } catch (e) {
    showToast(`Save failed: ${e}`, true);
    throw e; // let the editor know the commit didn't take
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
    // A modal open on this book refreshes its cover preview, and a backend-driven
    // update (a future bulk recrawl) keeps the UI in step.
    if (metadataBook && metadataBook.id === e.payload.id) {
      metadataBook = e.payload;
      renderCoverPreview(metadataBook);
    }
    render();
  });
}

// ---------------------------------------------------------------------------
// LAN server toggle (device popover)
// ---------------------------------------------------------------------------

function wireServer() {
  $("#btn-server-toggle").addEventListener("click", () => toggleServer());
  $("#server-token").addEventListener("click", () => copyServerToken());
}

async function refreshServerStatus() {
  try {
    const s = await window.api.invoke("server_status");
    updateServerUI(s);
  } catch (e) {
    console.error("server_status failed:", e);
  }
}

async function toggleServer() {
  const btn = $("#btn-server-toggle");
  btn.disabled = true;
  try {
    const running = btn.dataset.running === "1";
    const cmd = running ? "server_stop" : "server_start";
    const s = await window.api.invoke(cmd);
    updateServerUI(s);
  } catch (e) {
    showToast(`server toggle failed: ${e}`, true);
  } finally {
    btn.disabled = false;
  }
}

function updateServerUI(s) {
  const label = $("#server-status-label");
  const btn = $("#btn-server-toggle");
  const fields = $("#server-fields");
  const portCell = $("#server-port");
  const tokenBtn = $("#server-token");
  if (s?.running) {
    label.className = "server-status-label on";
    label.textContent = "On";
    btn.textContent = "Stop serving";
    btn.dataset.running = "1";
    fields.hidden = false;
    portCell.textContent = String(s.port ?? s.default_port);
    tokenBtn.textContent = s.token || "—";
    tokenBtn.dataset.token = s.token || "";
  } else {
    label.className = "server-status-label off";
    label.textContent = "Off";
    btn.textContent = `Start serving on port ${s?.default_port ?? 8731}`;
    btn.dataset.running = "0";
    fields.hidden = true;
    portCell.textContent = "—";
    tokenBtn.textContent = "—";
    tokenBtn.dataset.token = "";
  }
}

async function copyServerToken() {
  const token = $("#server-token").dataset.token || "";
  if (!token) return;
  try {
    await navigator.clipboard.writeText(token);
    showToast("token copied");
  } catch (e) {
    showToast(`copy failed: ${e}`, true);
  }
}
