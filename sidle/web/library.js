// Library view: gallery + list, drag-drop, right-click menu, live status.

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));

const state = {
  books: [],
  view: "gallery", // 'gallery' | 'list'
  section: "books", // 'books' | 'notes' — top-level Books/Notes tab
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
  // When non-null, a foreground import (user-initiated drop / add) is in
  // flight. `{ message, failed }` — shown in the status bar with priority
  // over the autopull and queue summary lines.
  importing: null,
  // True while a Kindle annotation sync (auto-on-connect or manual) is
  // running. Surfaced in the status bar; lower priority than importing /
  // autopull so a book pull's progress isn't hidden behind it.
  annotationSync: false,
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
  wireServer();
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
  setupKualSection();
  refreshServerStatus();
  subscribeSendProgress();
  subscribePullProgress();
  subscribeAnnotationSync();
  subscribeLibraryRowUpdated();
  // If the user left off in the Notes tab, populate it now that boot is done.
  if (state.section === "notes" && window.Notebooks) window.Notebooks.show();
});

function loadPreferences() {
  const view = localStorage.getItem("view");
  if (view === "list") state.view = "list";
  const sort = localStorage.getItem("sort");
  if (sort) {
    try {
      state.sort = { ...state.sort, ...JSON.parse(sort) };
    } catch {
      // malformed JSON in localStorage — keep the default
    }
  }
  const cols = localStorage.getItem("columnWidths");
  if (cols) {
    try {
      state.columnWidths = JSON.parse(cols) || {};
    } catch {
      // malformed JSON in localStorage — keep the default
    }
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
    } catch {
      // malformed JSON in localStorage — keep the default
    }
  }
  const search = localStorage.getItem("search");
  if (typeof search === "string") state.search = search;
  const section = localStorage.getItem("section");
  if (section === "notes") state.section = "notes";
  applySection();
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
  $("#section-books").addEventListener("click", () => setSection("books"));
  $("#section-notes").addEventListener("click", () => setSection("notes"));
  $("#btn-notes-import").addEventListener("click", () => {
    if (window.Notebooks) window.Notebooks.importFolder();
  });

  $("#btn-settings").addEventListener("click", openSettings);
  $("#settings-close").addEventListener("click", closeSettings);
  $("#settings-modal .modal-backdrop").addEventListener("click", closeSettings);
  $("#settings-move").addEventListener("click", () => pickRelocate("move"));
  $("#settings-use").addEventListener("click", () => pickRelocate("use"));
  $("#settings-backup").addEventListener("click", doBackup);
  $("#settings-restore").addEventListener("click", pickRestore);
  $("#settings-confirm-cancel").addEventListener("click", resetRelocateConfirm);
  $("#settings-confirm-ok").addEventListener("click", confirmRelocate);
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
  const notes = state.section === "notes";
  // Content visibility is driven by `.view.active { display: block }`, an author
  // rule that OVERRIDES the `hidden` attribute (`[hidden]` is only a UA rule), so
  // the `active` class — not `hidden` — is what actually shows/hides a section.
  // It must be cleared on the book views in Notes mode or the gallery shows
  // through; `hidden` is kept in sync for a11y.
  const galleryActive = !notes && state.view === "gallery";
  const listActive = !notes && state.view === "list";
  // Toolbar toggle buttons reflect the chosen view regardless of section.
  $("#view-gallery").classList.toggle("active", state.view === "gallery");
  $("#view-list").classList.toggle("active", state.view === "list");
  $("#view-gallery").setAttribute("aria-selected", String(state.view === "gallery"));
  $("#view-list").setAttribute("aria-selected", String(state.view === "list"));
  $("#gallery").classList.toggle("active", galleryActive);
  $("#list").classList.toggle("active", listActive);
  $("#gallery").hidden = !galleryActive;
  $("#list").hidden = !listActive;
}

// Top-level Books / Notes split. Books shows the gallery/list + filter chrome;
// Notes shows the Scribe handwritten-notebook grid (owned by notebooks.js) and
// swaps the toolbar (Add… → Import notebooks…, hides the view toggle + filters).
function setSection(s) {
  state.section = s;
  applySection();
  localStorage.setItem("section", s);
  if (window.Notebooks) {
    if (s === "notes") window.Notebooks.show();
    else window.Notebooks.hide();
  }
}

function applySection() {
  const notes = state.section === "notes";
  $("#section-books").classList.toggle("active", !notes);
  $("#section-notes").classList.toggle("active", notes);
  $("#section-books").setAttribute("aria-selected", String(!notes));
  $("#section-notes").setAttribute("aria-selected", String(notes));
  // Books-only chrome.
  $("#btn-add").hidden = notes;
  $("#btn-notes-import").hidden = !notes;
  $("#view-sep").hidden = notes;
  $("#view-seg").hidden = notes;
  $("#filter-bar").hidden = notes;
  const search = document.querySelector(".filter-search");
  if (search) search.hidden = notes;
  // `#notes` uses the same `.view`/`.view.active` system: the `active` class
  // (not `hidden`) is what `display: block`s it — without it the base
  // `.view { display: none }` keeps it hidden. applyView() owns gallery/list.
  $("#notes").classList.toggle("active", notes);
  $("#notes").hidden = !notes;
  applyView();
}

// ---------------------------------------------------------------------------
// Drag and drop
// ---------------------------------------------------------------------------

// True while an in-flight OS drag carries at least one image file. Latched on
// `enter` (the only phase before `drop` that reports paths) so `over` can light
// up the cover drop target without re-sniffing.
let dragHasImage = false;

// The cover preview element when it can accept a dropped image — i.e. the
// single-book metadata editor is open. `null` (no cover target) otherwise, so
// drops fall through to the normal library import.
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
    // anywhere on it replaces the cover (OS file drags arrive through Tauri, not
    // HTML5 drop, so we route them here instead of a DOM dropzone). Non-image
    // files fall through to the normal import below — so dropping a book while
    // the editor happens to be open still imports it.
    const coverBox = coverDropTarget();
    if (coverBox) {
      if (t === "enter") dragHasImage = (payload.paths || []).some(isImagePath);
      if ((t === "enter" || t === "over") && dragHasImage) {
        veil.hidden = true; // keep the editor focused; no full-screen veil
        // Arm the preview regardless of cursor position: the drop is accepted
        // anywhere on the open editor, so the lit target truthfully says "this
        // image becomes the cover" (and dodges window-vs-webview coord skew).
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
          // device renders the PDF; the pen draws over it). See
          // .claude/plans/pdf-to-kfx.md.
          lower.endsWith(".pdf") ||
          // Plain .zip is accepted silently so an Aozora Bunko archive can
          // be dropped in. Non-aozora .zips fail at the backend with a
          // standard import-failed toast; no special UI signal that .zip
          // is supported (intentional — see import.rs convert_aozora_zip).
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

async function importPaths(paths) {
  startImportStatus(paths);

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
    showImportFailure();
    return;
  }

  // Status-bar failure mode is the all-failed case (status bar is the
  // single-line summary; per-file detail still goes in the toast below).
  if (failed > 0 && imported === 0 && dupes === 0) {
    showImportFailure();
  } else {
    clearImportStatus();
  }

  await refresh();

  const parts = [];
  if (imported) parts.push(`${imported} imported`);
  if (dupes) parts.push(`${dupes} already in library`);
  if (failed) parts.push(`${failed} failed`);
  if (parts.length) showToast(parts.join(" · "), failed > 0);
}

// Two-phase reveal for the aozora pipeline: the slow step is the
// parse → cover render → build_epub inside library_import. Without an
// event from Rust we can't tell exactly when it kicks in, but in
// practice the file-IO prefix is microseconds, so a short timer is a
// good-enough approximation.
let importPhaseTimer = null;

function startImportStatus(paths) {
  state.importing = { message: importInitialMessage(paths), failed: false };
  if (importPhaseTimer) clearTimeout(importPhaseTimer);
  // Only aozora gets a phase-2 message — for EPUB / KFX the import
  // returns almost instantly anyway.
  if (paths.length === 1 && paths[0].toLowerCase().endsWith(".zip")) {
    importPhaseTimer = setTimeout(() => {
      if (state.importing && !state.importing.failed) {
        state.importing.message = "Converting Aozora zip to EPUB…";
        render();
      }
    }, 300);
  }
  render();
}

function clearImportStatus() {
  if (importPhaseTimer) {
    clearTimeout(importPhaseTimer);
    importPhaseTimer = null;
  }
  state.importing = null;
  render();
}

function showImportFailure() {
  if (importPhaseTimer) {
    clearTimeout(importPhaseTimer);
    importPhaseTimer = null;
  }
  state.importing = { message: "Import failed", failed: true };
  render();
  // Linger briefly so the user notices, then fall back to queue summary.
  setTimeout(() => {
    if (state.importing && state.importing.failed) {
      state.importing = null;
      render();
    }
  }, 3000);
}

function importInitialMessage(paths) {
  if (paths.length === 1) {
    const lower = paths[0].toLowerCase();
    if (lower.endsWith(".zip")) return "Importing Aozora zip…";
    if (lower.endsWith(".epub")) return "Importing EPUB…";
    if (lower.endsWith(".kfx-zip")) return "Importing KFX bundle…";
    if (lower.endsWith(".kfx")) return "Importing KFX…";
    if (lower.endsWith(".azw3")) return "Importing AZW3…";
    if (lower.endsWith(".mobi")) return "Importing MOBI…";
    if (lower.endsWith(".pdf")) return "Importing PDF…";
  }
  return `Importing ${paths.length} file${paths.length === 1 ? "" : "s"}…`;
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
  // Only `sent` rows have a full sha256 we can intersect with the local
  // library; `orphan` rows expose a sha8 prefix only. The sentSet drives
  // the green-badge / Send vs Remove decisions, which only matter for
  // books that actually exist locally — so excluding orphans here is
  // correct, not an omission.
  state.sentSet = new Set(
    state.sent.filter((r) => r.kind === "sent").map((r) => r.sha256),
  );
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
// Author splitting: extractFacetValues splits on /\s*[&、]\s*/ — " & " (the
// import/editor join, matching calibre/KFX) or the CJK ideographic comma 「、」.
// NEVER a plain comma: that separates "Surname, Given" inside a single Western
// name (e.g. "Kafka, Franz"), which import flips to "Franz Kafka". Everywhere
// else is just JS Unicode-native string ops (.includes, .toLowerCase, etc.).
// ---------------------------------------------------------------------------

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
  card.addEventListener("dblclick", () => openReader(b));
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

// A book pairs KFX with exactly one non-KFX side: EPUB for reflowable books,
// PDF for PDF-backed (container) books. Derived from the conversion `kind`.
function nonKfxFormat(b) {
  return b.kind === "pdf_to_kfx" || b.kind === "kfx_to_pdf" ? "pdf" : "epub";
}

// Returns the conversion status as it applies to the given format side
// (`"epub"`, `"pdf"`, or `"kfx"`). The format the import wrote directly is
// always "done"; the format the queue produces follows b.status. `b.kind` is
// "<source>_to_<target>" — the queue produces the target.
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
  tr.addEventListener("dblclick", () => openReader(b));
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
  wrap.appendChild(formatBadge(nonKfxFormat(b), b, /*compact=*/ false));
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

  // Suppress the browser's default text-selection-on-drag behavior.
  // Does NOT prevent the click event from firing on mouseup, so the sort
  // handler still runs for plain clicks.
  e.preventDefault();

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
    if (!pop.hidden) {
      refreshDeviceList();
      // KUAL section staleness can change between opens (server token
      // rotates, native rebuilt, etc.) so always re-pull on show.
      refreshKualStatus();
      // Re-stage the LAN self-update bundle so an untethered "Update over Wi-Fi"
      // serves the latest cross-built picker: the dev loop is "rebuild armv7 →
      // open this popover → device pulls", no cable, no app restart.
      // Fire-and-forget + mtime-gated (a no-op once warm); non-fatal on error.
      window.api
        .invoke("kual_stage_dist")
        .catch((err) => console.warn("kual_stage_dist failed:", err));
      // The LAN server can start/stop out-of-band (sakabar, CLI, or it outlived a
      // previous app session), so re-probe on every open — the toggle is
      // observation-based, not pinned to this app's own start/stop actions.
      refreshServerStatus();
    }
  });
  $("#btn-send-unsent").addEventListener("click", () => sendUnsent());
  $("#btn-sync-annotations").addEventListener("click", () => syncAnnotations());
  $("#btn-import-all-orphans").addEventListener("click", () => importAllOrphans());
  $("#btn-device-eject").addEventListener("click", () => ejectDevice());
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

// ----- KUAL deploy section ---------------------------------------------------

function setupKualSection() {
  // Per-file progress event from the install command. Surfaces a live
  // count next to the button so a multi-file push doesn't look frozen.
  window.api.listen("kual:install-progress", (e) => {
    const r = e.payload;
    const prog = $("#kual-install-progress");
    if (!prog) return;
    const path = r.device_path || "";
    let line;
    if (r.kind === "wrote") line = `wrote ${path}`;
    else if (r.kind === "skipped") line = `skipped ${path}`;
    else if (r.kind === "failed") line = `failed ${path}: ${r.error}`;
    else line = JSON.stringify(r);
    prog.textContent = line;
    prog.hidden = false;
  });

  $("#btn-kual-install").addEventListener("click", async () => {
    const btn = $("#btn-kual-install");
    const prog = $("#kual-install-progress");
    btn.disabled = true;
    prog.hidden = false;
    prog.textContent = "pushing…";
    try {
      const report = await window.api.invoke("kual_install");
      const wrote = report.results.filter((r) => r.kind === "wrote").length;
      const skipped = report.results.filter((r) => r.kind === "skipped").length;
      const failed = report.results.filter((r) => r.kind === "failed");
      if (failed.length) {
        prog.textContent = `${failed.length} failed — see file list`;
      } else if (wrote === 0) {
        prog.textContent = `already in sync (${skipped} skipped)`;
      } else {
        prog.textContent = `pushed ${wrote}, skipped ${skipped}`;
      }
    } catch (err) {
      prog.textContent = `error: ${err}`;
    } finally {
      // Re-pull status; that'll re-enable/disable the button based on
      // the new state.
      await refreshKualStatus();
    }
  });
}

async function refreshKualStatus() {
  try {
    const status = await window.api.invoke("kual_status");
    renderKualStatus(status);
  } catch (err) {
    console.error("kual_status failed:", err);
  }
}

function renderKualStatus(status) {
  const section = $("#kual-section");
  const label = $("#kual-status-label");
  const btn = $("#btn-kual-install");
  const tip = $("#kual-tip");
  const list = $("#kual-file-list");

  if (!status || status.overall.kind === "device_disconnected") {
    // Hide the whole section when no Kindle is connected (or MTP-only).
    // The server section above stays visible regardless.
    section.hidden = true;
    return;
  }
  section.hidden = false;

  const overall = status.overall.kind;
  label.className = "kual-status-label " + overallClass(overall);
  label.textContent = overallLabel(status.overall);

  if (overall === "binary_not_built") {
    btn.disabled = true;
    btn.textContent = "Install KUAL";
    tip.textContent =
      "Run `cargo build --release --target armv7-unknown-linux-musleabihf -p sidle-native`, then click again.";
    tip.hidden = false;
  } else {
    btn.disabled = false;
    btn.textContent =
      overall === "in_sync"
        ? "Re-push KUAL"
        : overall === "not_installed"
          ? "Install KUAL"
          : "Update KUAL";
    // Show "binary older than source" hint only when nothing else is wrong.
    if (
      overall === "in_sync" &&
      status.binary_mtime_ms != null &&
      status.native_source_mtime_ms != null &&
      status.native_source_mtime_ms > status.binary_mtime_ms
    ) {
      tip.textContent =
        "Binary is older than native source — you have unbuilt code changes.";
      tip.hidden = false;
    } else {
      tip.hidden = true;
      tip.textContent = "";
    }
  }

  // Per-file rows.
  list.innerHTML = "";
  if (status.files && status.files.length) {
    for (const f of status.files) {
      const li = document.createElement("li");
      const name = document.createElement("span");
      name.textContent = f.device_path;
      const state = document.createElement("span");
      state.className = "kual-file-state " + fileStateClass(f.state.kind);
      state.textContent = fileStateLabel(f.state.kind);
      li.appendChild(name);
      li.appendChild(state);
      list.appendChild(li);
    }
    list.hidden = false;
  } else {
    list.hidden = true;
  }
}

function overallClass(kind) {
  switch (kind) {
    case "in_sync": return "in-sync";
    case "stale": return "stale";
    case "not_installed": return "not-installed";
    case "binary_not_built": return "binary-not-built";
    default: return "unknown";
  }
}

function overallLabel(overall) {
  switch (overall.kind) {
    case "in_sync": return "In sync";
    case "stale": {
      const n = (overall.stale_count || 0) + (overall.missing_count || 0);
      return `${n} file${n === 1 ? "" : "s"} out of date`;
    }
    case "not_installed": return "Not installed";
    case "binary_not_built": return "Binary not built";
    default: return "—";
  }
}

function fileStateClass(kind) {
  switch (kind) {
    case "synced": return "synced";
    case "stale": return "stale";
    case "missing": return "missing";
    case "source_missing": return "source-missing";
    default: return "";
  }
}

function fileStateLabel(kind) {
  switch (kind) {
    case "synced": return "synced";
    case "stale": return "stale";
    case "missing": return "missing";
    case "source_missing": return "source missing";
    default: return kind;
  }
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
    // Eject is mass-storage-only. MTP devices close their USB session
    // on unplug — no eject concept — so hide the button instead of
    // showing a no-op.
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
    refreshKualStatus();
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
    // Hide KUAL section when no device is connected.
    const kualSection = $("#kual-section");
    if (kualSection) kualSection.hidden = true;
    // Hide eject button on disconnect — nothing to eject.
    const ejectBtn = $("#btn-device-eject");
    if (ejectBtn) ejectBtn.hidden = true;
    const syncBtn = $("#btn-sync-annotations");
    if (syncBtn) syncBtn.disabled = true;
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
  // `device_import_orphan` per row, which already enqueues the KFX→EPUB
  // background job; nothing extra to do here.
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
    // orphan — no local library entry. Show the filename minus the sha8
    // infix so the user sees something recognizable.
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
    // The popup row always carries the exact filename — delete by name,
    // no sha translation. The backend verifies the `.<sha8>.kfx` shape
    // before touching anything so a stale UI can't drive arbitrary deletes.
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
  try {
    results = await window.api.invoke("device_delete", { filenames });
  } catch (e) {
    showToast(`delete failed: ${e}`, true);
    return;
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
  for (const r of orphans) {
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
  let result;
  try {
    result = await window.api.invoke("device_import_orphan", { filename });
  } catch (e) {
    showToast(`import failed: ${e}`, true);
    return;
  }
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
// Kindle annotation sync (highlights / notes / bookmarks)
//
// Auto-runs in the background on mass-storage connect (the backend monitor
// fires `annotations:sync-*`); the popover button is the manual re-run for
// when the auto sync didn't catch something. Either way it's idempotent.
// ---------------------------------------------------------------------------

// Turn a DeviceImportReport into a one-line summary for the toast. Reports both
// new annotations and ones removed because they were deleted on the device
// (full-mirror sync), so a delete-only sync isn't silently "up to date".
function annotationSyncSummary(report) {
  const added = report?.annotations?.inserted ?? 0;
  const removed = report?.annotations?.removed ?? 0;
  if (added === 0 && removed === 0) return "Highlights already up to date";
  const books = report?.matched ?? 0;
  const from = books > 0 ? ` across ${books} book${books === 1 ? "" : "s"}` : "";
  const noun = added + removed === 1 ? "annotation" : "annotations";
  const bits = [];
  if (added > 0) bits.push(`synced ${added}`);
  if (removed > 0) bits.push(`removed ${removed}`);
  const s = `${bits.join(", ")} ${noun}${from}`;
  return s.charAt(0).toUpperCase() + s.slice(1);
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
    prog.textContent = "syncing highlights…";
  }
  try {
    const report = await window.api.invoke("annotations_import_from_device");
    showToast(annotationSyncSummary(report));
    window.sidleReader?.reloadAnnotations?.();
  } catch (e) {
    showToast(`highlight sync failed: ${e}`, true);
  } finally {
    btn.textContent = prevLabel;
    // Re-enable as long as a Kindle (either transport) is still connected.
    btn.disabled = !state.device;
    if (prog) setTimeout(() => (prog.hidden = true), 2000);
  }
}

// Background sync driven by the device monitor on connect.
function subscribeAnnotationSync() {
  window.api.listen("annotations:sync-start", () => {
    state.annotationSync = true;
    renderQueue();
  });
  window.api.listen("annotations:sync-done", (e) => {
    state.annotationSync = false;
    renderQueue();
    const report = e.payload;
    const added = report?.annotations?.inserted ?? 0;
    const removed = report?.annotations?.removed ?? 0;
    // Only toast when the auto sync actually changed something (added or removed
    // on the device) — a no-op reconnect shouldn't nag.
    if (added > 0 || removed > 0) showToast(annotationSyncSummary(report));
    // If the user is reading one of the synced books, repaint in place.
    window.sidleReader?.reloadAnnotations?.();
  });
  window.api.listen("annotations:sync-error", (e) => {
    state.annotationSync = false;
    renderQueue();
    showToast(`Kindle highlight sync failed: ${e.payload}`, true);
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

  // Priority order in the status bar:
  //   1. foreground import (state.importing) — user-initiated drop / add
  //   2. autopull from Kindle's /dedrm folder (state.autopull)
  //   3. Kindle annotation sync (state.annotationSync)
  //   4. background conversion queue summary (the default)
  if (state.importing) {
    summary.textContent = state.importing.message;
    if (state.importing.failed) toggle.classList.add("errors");
    else toggle.classList.add("active");
  } else if (state.autopull) {
    summary.textContent =
      `Pulling ${state.autopull.done}/${state.autopull.total} from Kindle…`;
    toggle.classList.add("active");
  } else if (state.annotationSync) {
    summary.textContent = "Syncing Kindle highlights…";
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
  // Suppress the WebView's native right-click menu (Reload, Open Frame in New
  // Window, …) app-wide — it exposes nothing useful and its Reload reloads
  // index.html, dropping you out of the reader back to the library. Our own
  // card/row/header handlers preventDefault and open #ctx-menu in the target
  // phase, before this bubble-phase listener runs, so they're unaffected; this
  // just kills the native menu everywhere they don't. Editable fields are the
  // exception — there the native Cut/Copy/Paste/spellcheck menu is genuinely
  // useful (metadata editor, search boxes). (Reader section iframes are
  // separate documents whose events don't bubble here; they're handled in
  // reader.js's create-overlayer hook.)
  document.addEventListener("contextmenu", (e) => {
    const t = e.target;
    const inField =
      t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable);
    if (!inField) e.preventDefault();
  });
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
    add(menu, `Edit metadata (${sel.length})…`, () =>
      openMetadataModal(sel, { bulk: true }),
    );
    add(menu, `Remove ${sel.length} from library`, () => bulkRemove(), true);
  } else {
    // Single-item menu.
    add(menu, "Read", () => openReader(b));
    if (state.device && b.status === "done") {
      if (state.sentSet.has(b.sha256)) {
        const row = state.sent.find(
          (r) => r.kind === "sent" && r.sha256 === b.sha256,
        );
        const filename = row ? row.filename : null;
        if (filename) {
          add(menu, "Remove from Kindle", () =>
            deleteFromDevice([filename], [b.title]),
          );
        }
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
  $("#sel-edit").addEventListener("click", () => {
    const sel = selectedBooks();
    if (sel.length === 1) openMetadataModal(sel[0]);
    else if (sel.length > 1) openMetadataModal(sel, { bulk: true });
  });
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
  const byKind = new Map(
    state.sent
      .filter((r) => r.kind === "sent")
      .map((r) => [r.sha256, r.filename]),
  );
  const pairs = eligible
    .map((b) => ({ filename: byKind.get(b.sha256), title: b.title }))
    .filter((p) => p.filename);
  if (pairs.length === 0) return;
  await deleteFromDevice(
    pairs.map((p) => p.filename),
    pairs.map((p) => p.title),
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
  let failed = 0;
  for (const b of sel) {
    try {
      await window.api.invoke("library_remove", { bookId: b.id });
    } catch (e) {
      // library_remove now surfaces IO errors (e.g. Spotlight/Books.app
      // holding a handle on the EPUB) instead of silently leaving the
      // files behind. Show the first one in a toast and keep going so
      // partial success still gets reported.
      failed += 1;
      if (failed === 1) showToast(`remove failed for "${b.title}": ${e}`, true);
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

// Open a book in the built-in reader. `reader.js` (an ES module) installs
// `window.sidleReader`; it loads after this classic script, so it's always
// present by the time a card is clicked.
function openReader(b) {
  if (window.sidleReader) window.sidleReader.open(b.id);
  else showToast("reader not ready", true);
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
// Library settings (the ⚙ in the status bar): show where the library lives and
// move it to / adopt one in another folder. Both repoint the root (config.json)
// and restart the app — see commands::library::library_relocate_*.
// ---------------------------------------------------------------------------

let relocatePending = null; // { mode: "move" | "use", dest: string }

async function openSettings() {
  resetRelocateConfirm();
  $("#settings-status").hidden = true;
  $("#settings-modal").hidden = false;
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
  $("#settings-confirm-ok").disabled = false;
}

async function pickRelocate(mode) {
  const dest = await window.api.invoke("library_pick_folder");
  if (!dest) return;
  relocatePending = { mode, dest };
  $("#settings-confirm-text").textContent =
    mode === "move"
      ? `Move your library to:\n${dest}\n\nsidle verifies, removes the old location, and restarts.`
      : `Use the library in:\n${dest}\n\nsidle restarts. Nothing is copied.`;
  $("#settings-confirm").hidden = false;
  $("#settings-status").hidden = true;
}

async function confirmRelocate() {
  if (!relocatePending) return;
  const { mode, dest } = relocatePending;
  $("#settings-confirm-ok").disabled = true;
  setSettingsStatus(
    mode === "move" ? "Copying library…"
    : mode === "restore" ? "Restoring… this can take a while for a large backup."
    : "Switching library…",
  );
  try {
    if (mode === "move") {
      await window.api.invoke("library_relocate_move", { dest });
    } else if (mode === "restore") {
      await window.api.invoke("library_restore", { src: dest });
    } else {
      await window.api.invoke("library_relocate_use", { dir: dest });
    }
    // On success the app restarts and this webview reloads, so we normally
    // never reach here.
    setSettingsStatus("Restarting…");
  } catch (e) {
    $("#settings-confirm-ok").disabled = false;
    setSettingsStatus(String(e?.message ?? e), true);
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
  setSettingsStatus("Backing up… this can take a while for a large library.");
  try {
    const r = await window.api.invoke("library_backup", { dest });
    setSettingsStatus(
      `Backed up ${plural(r.books, "book")} and ${plural(r.annotations, "highlight")} to:\n${r.path}`,
    );
  } catch (e) {
    setSettingsStatus(String(e?.message ?? e), true);
  } finally {
    btn.disabled = false;
  }
}

// Restore IS destructive and restarts, so it routes through the shared
// confirm box (mode "restore"), handled in confirmRelocate.
async function pickRestore() {
  const src = await window.api.invoke("library_restore_pick_src");
  if (!src) return;
  relocatePending = { mode: "restore", dest: src };
  $("#settings-confirm-text").textContent =
    `Restore from:\n${src}\n\nThis replaces your current library. A dated safety copy is kept next to it, and sidle restarts.`;
  $("#settings-confirm").hidden = false;
  $("#settings-status").hidden = true;
}

function plural(n, word) {
  return `${n} ${word}${n === 1 ? "" : "s"}`;
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
// When non-null, the modal is in bulk mode editing this array of books. Bulk
// and single modes are mutually exclusive (one is always null).
let metadataBulk = null;

// Mirror of cover_fetch::looks_like_real_amazon_asin: a real Amazon ASIN is 10
// chars, uppercase letters + digits. boko's fabricated fallback is 32-char, so
// length + charset distinguishes them.
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
  $("#metadata-form").asin.addEventListener("input", renderAsinHint);

  // Snapshot each input's placeholder so bulk mode can swap to "Leave
  // unchanged" and single mode can restore the original hint.
  for (const el of $("#metadata-form").querySelectorAll("input")) {
    el.dataset.ph = el.placeholder || "";
  }

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

// Fields the bulk editor can touch (everything except per-book-unique title +
// asin, and the cover). Order is irrelevant.
const BULK_FIELDS = [
  "author",
  "language",
  "publisher",
  "published_at",
  "series_name",
  "series_index",
  "tags",
];

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
    // Title + ASIN are hidden in bulk; disable them so they're exempt from
    // native required-validation and aren't read on submit.
    form.title.disabled = true;
    form.asin.disabled = true;

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
  form.asin.disabled = false;
  setBulkPlaceholders(false);

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
  form.asin.value = book.asin || "";
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
  form.asin.disabled = false;
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

// Live validity feedback for the ASIN field, and gate the Re-fetch button on a
// real-looking ASIN (the recrawl backend would otherwise just return NoAsin).
function renderAsinHint() {
  const v = $("#metadata-form").asin.value.trim();
  const hint = $("#asin-hint");
  const refetch = $("#metadata-cover-refetch");
  const real = looksLikeRealAsin(v);
  if (v === "") {
    hint.textContent = "No ASIN — needed to fetch the color cover.";
    hint.className = "field-hint";
  } else if (real) {
    hint.textContent = "✓ Looks like a real ASIN.";
    hint.className = "field-hint";
  } else {
    hint.textContent = "Fabricated id — paste the real 10-char ASIN to enable cover fetch.";
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

async function submitMetadataForm() {
  if (metadataBulk) return submitBulkMetadataForm();
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
    // Tags split on ASCII or CJK comma (tags have no "Surname, Given" hazard,
    // unlike authors). The backend lowercases + dedupes + drops empties.
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

  // ASIN is saved by its own command (library_set_asin), and only when it
  // actually changed to a real value — that keeps the full-patch save working
  // on books that still carry their fabricated 32-char id.
  const asin = form.asin.value.trim();
  const asinChanged = asin !== (metadataBook.asin || "");
  if (asinChanged && asin !== "" && !looksLikeRealAsin(asin)) {
    showToast("ASIN must be a real 10-character Amazon id, or left unchanged.", true);
    return;
  }

  try {
    const updated = await window.api.invoke("library_update_metadata", {
      bookId: metadataBook.id,
      patch,
    });
    mergeBookRow(updated);
    if (asinChanged && looksLikeRealAsin(asin)) {
      const withAsin = await window.api.invoke("library_set_asin", {
        bookId: metadataBook.id,
        asin,
      });
      mergeBookRow(withAsin);
    }
    closeMetadataModal();
    render();
  } catch (e) {
    showToast(`Save failed: ${e}`, true);
  }
}

// Bulk mode: build a sparse patch from only the fields the user filled in
// (empty = leave unchanged), tags additive. Calls library_bulk_update_metadata,
// which returns the updated rows (it doesn't emit row-updated, to avoid one
// render per book), so we merge them all and render once.
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

// "Re-fetch cover" in the editor: persist the (possibly just-edited) ASIN, then
// re-pull the color cover from Amazon. Stays in the modal and refreshes the
// preview itself, because library_recrawl_cover does NOT emit row-updated.
async function onCoverRefetchClick() {
  if (!metadataBook) return;
  const asin = $("#metadata-form").asin.value.trim();
  if (!looksLikeRealAsin(asin)) {
    showToast("Enter a real 10-character ASIN first.", true);
    return;
  }
  // Save the ASIN first (if changed) so the backend recrawl reads it.
  if (asin !== (metadataBook.asin || "")) {
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
  // updated — refresh the gallery tile + the open modal preview ourselves.
  state.coverCacheBust += 1;
  const idx = state.books.findIndex((b) => b.id === metadataBook.id);
  if (idx !== -1) {
    state.books[idx] = { ...state.books[idx], cover_path: result.cover_path };
    metadataBook = state.books[idx];
  }
  renderCoverPreview(metadataBook);
  render();
  showToast(`cover updated: ${metadataBook.title}`);
}

// "Search Amazon ↗": open the browser to a marketplace search so the user can
// find the real ASIN to paste. The backend picks the domain from language.
async function onAsinSearchClick() {
  if (!metadataBook) return;
  try {
    await window.api.invoke("library_amazon_search", { bookId: metadataBook.id });
  } catch (e) {
    showToast(`couldn't open Amazon: ${e}`, true);
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
