// Library view: gallery + list, drag-drop, right-click menu, live status.

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));

const state = {
  books: [],
  view: "gallery", // 'gallery' | 'list'
  sort: { key: "imported_at", asc: false },
  device: null, // DeviceInfo | null
  sent: [],     // Vec<DeviceBookRow>
  sentSet: new Set(), // sha256s currently on device, derived from `sent`
  columnWidths: {}, // { title: 280, ... } persisted px widths
  selected: new Set(), // book ids currently selected
  lastClicked: null,   // last single-clicked book id, anchor for shift-range
};

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
  wireListHeaderInteractions();
  wireSelection();
  await refresh();
  subscribeStatus();
  subscribeDeviceStatus();
  subscribeSendProgress();
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
  applyView();
}

function persistPreferences() {
  localStorage.setItem("view", state.view);
  localStorage.setItem("sort", JSON.stringify(state.sort));
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
      const epubs = paths.filter((p) => p.toLowerCase().endsWith(".epub"));
      if (epubs.length === 0) {
        showToast("only .epub files are supported", true);
        return;
      }
      importPaths(epubs);
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
  const books = sortedBooks();
  renderGallery(books);
  renderList(books);
  renderQueue();
  renderSelectionBar();
  updateSendUnsentButton();
  $("#gallery-empty").hidden = books.length > 0;
  $("#list-empty").hidden = books.length > 0;
  if (state.view === "list") {
    requestAnimationFrame(() => {
      ensureDefaultColumnWidths();
      applyColumnWidths();
    });
  }
}

function sortedBooks() {
  const { key, asc } = state.sort;
  const dir = asc ? 1 : -1;
  return [...state.books].sort((a, b) => {
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
  return b[key];
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

  const coverUrl = window.api.fileUrl(b.cover_path);
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

  const epub = document.createElement("span");
  epub.className = "fmt-badge epub";
  epub.textContent = "EPUB";
  epub.title = "EPUB available";
  wrap.appendChild(epub);

  const kfx = document.createElement("span");
  kfx.className = `fmt-badge kfx ${b.status}`;
  kfx.textContent = "KFX";
  kfx.title = kfxTooltip(b);
  wrap.appendChild(kfx);

  if (state.sentSet.has(b.sha256)) {
    const dot = document.createElement("span");
    dot.className = "meta-kindle-dot";
    dot.title = "On Kindle";
    wrap.appendChild(dot);
  }

  return wrap;
}

function kfxTooltip(b) {
  switch (b.status) {
    case "done": return "KFX ready";
    case "converting": return "KFX converting…";
    case "pending": return "KFX queued";
    case "error": return `KFX failed: ${b.error || ""}`;
    default: return "KFX";
  }
}

function renderList(books) {
  const tbody = $("#list-body");
  tbody.innerHTML = "";
  for (const b of books) tbody.appendChild(listRow(b));
  $$("#list th[data-sort]").forEach((th) => {
    th.classList.toggle("sorted", th.dataset.sort === state.sort.key);
    th.classList.toggle("asc", state.sort.asc);
  });
}

function listRow(b) {
  const tr = document.createElement("tr");
  if (state.selected.has(b.id)) tr.classList.add("selected");
  tr.dataset.bookId = b.id;

  tr.appendChild(cell(b.title || "Untitled"));
  tr.appendChild(cell(b.author || ""));
  tr.appendChild(cell(b.language || ""));
  tr.appendChild(cell(formatDate(b.imported_at)));
  tr.appendChild(cell(formatBytes(b.file_size)));
  tr.appendChild(formatsCell(b));
  tr.appendChild(onKindleCell(b));

  tr.addEventListener("click", (e) => onItemClick(e, b));
  tr.addEventListener("dblclick", () => openInFinder(b.id));
  tr.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    onItemContext(e, b);
    openContextMenu(e.clientX, e.clientY, b);
  });
  return tr;
}

function formatsCell(b) {
  const td = document.createElement("td");
  const wrap = document.createElement("div");
  wrap.className = "formats";

  // EPUB is always present after import — we own the source file.
  const epub = document.createElement("span");
  epub.className = "fmt-badge epub";
  epub.textContent = "EPUB";
  wrap.appendChild(epub);

  const kfx = document.createElement("span");
  kfx.className = `fmt-badge kfx ${b.status}`;
  if (b.status === "done") {
    kfx.textContent = "KFX";
  } else if (b.status === "converting") {
    kfx.textContent = "KFX · converting";
  } else if (b.status === "pending") {
    kfx.textContent = "KFX · queued";
  } else if (b.status === "error") {
    kfx.textContent = "KFX · failed";
    kfx.title = b.error || "";
    kfx.addEventListener("click", (e) => {
      e.stopPropagation();
      retryConvert(b.id);
    });
    kfx.style.cursor = "pointer";
  }
  wrap.appendChild(kfx);
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

function wireListHeaderInteractions() {
  // Sort handlers: only on TH elements that declare data-sort.
  $$("#list th[data-sort]").forEach((th) => {
    th.addEventListener("click", (e) => {
      // Don't trigger sort when grabbing the resizer.
      if (e.target.classList.contains("resizer")) return;
      const key = th.dataset.sort;
      if (state.sort.key === key) state.sort.asc = !state.sort.asc;
      else state.sort = { key, asc: true };
      persistPreferences();
      render();
    });
  });
  // Column resize handles.
  $$("#list .resizer").forEach((resizer, i) => {
    resizer.addEventListener("mousedown", (e) => onResizerDown(e, resizer, i));
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
  const startWidth = col.getBoundingClientRect().width;
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
    // When a conversion finishes, fetch the row to pick up kfx_path.
    if (status === "done") refresh();
    else render();
  });
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
  if (info) {
    dot.className = "device-dot connected";
    const free = info.free_bytes ? `· ${formatBytes(info.free_bytes)} free` : "";
    label.textContent = `Kindle ${free}`.trim();
    status.className = "device-popover-status connected";
    status.textContent = "Connected";
    $("#device-model").textContent = info.model || "Kindle";
    $("#device-serial").textContent = info.serial || "—";
    $("#device-free").textContent =
      info.free_bytes != null && info.total_bytes != null
        ? `${formatBytes(info.free_bytes)} of ${formatBytes(info.total_bytes)}`
        : "—";
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
    $("#device-free").textContent = "—";
    $("#device-count").textContent = "—";
    $("#device-sent-list").innerHTML = "";
    setSent([]);
    $("#device-empty").textContent = "Plug in a Kindle via USB.";
    $("#device-empty").hidden = false;
    render();
  }
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
    add(menu, "Open in Finder", () => openInFinder(b.id));
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
