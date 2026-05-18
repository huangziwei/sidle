// Library view: gallery + list, drag-drop, right-click menu, live status.

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));

const state = {
  books: [],
  view: "gallery", // 'gallery' | 'list'
  sort: { key: "imported_at", asc: false },
  device: null, // DeviceInfo | null
  sent: [],     // Vec<DeviceBookRow>
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
  applyView();
}

function persistPreferences() {
  localStorage.setItem("view", state.view);
  localStorage.setItem("sort", JSON.stringify(state.sort));
}

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

function wireToolbar() {
  $("#btn-add").addEventListener("click", onAddClick);
  $("#view-gallery").addEventListener("click", () => setView("gallery"));
  $("#view-list").addEventListener("click", () => setView("list"));
  $("#workers").addEventListener("change", async (e) => {
    const n = Number(e.target.value) || 2;
    try {
      const actual = await window.api.invoke("conversion_set_workers", { n });
      e.target.value = String(actual);
    } catch (err) {
      showToast(`failed to set workers: ${err}`, true);
    }
  });
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
  render();
}

function render() {
  const books = sortedBooks();
  renderGallery(books);
  renderList(books);
  renderQueue();
  updateSendUnsentButton();
  $("#gallery-empty").hidden = books.length > 0;
  $("#list-empty").hidden = books.length > 0;
}

function sortedBooks() {
  const { key, asc } = state.sort;
  const dir = asc ? 1 : -1;
  return [...state.books].sort((a, b) => {
    const av = a[key];
    const bv = b[key];
    if (av == null && bv == null) return 0;
    if (av == null) return 1;
    if (bv == null) return -1;
    if (typeof av === "number" && typeof bv === "number") return (av - bv) * dir;
    return String(av).localeCompare(String(bv)) * dir;
  });
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
  card.dataset.bookId = b.id;
  card.title = `${b.title}\n${b.author}`;

  const coverUrl = window.api.fileUrl(b.cover_path);
  const cover = document.createElement("div");
  cover.className = "cover";
  if (coverUrl) {
    const img = document.createElement("img");
    img.src = coverUrl;
    img.alt = "";
    img.loading = "lazy";
    cover.appendChild(img);
  } else {
    cover.textContent = b.title || "Untitled";
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
  meta.append(t, a);
  card.appendChild(meta);

  const pill = statusPill(b);
  if (pill) card.appendChild(pill);

  card.addEventListener("dblclick", () => openInFinder(b.id));
  card.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    openContextMenu(e.clientX, e.clientY, b);
  });
  return card;
}

function statusPill(b) {
  if (b.status === "done") return null;
  const pill = document.createElement("div");
  pill.className = `status-pill ${b.status}`;
  if (b.status === "converting" || b.status === "pending") {
    const spin = document.createElement("div");
    spin.className = "spinner";
    pill.appendChild(spin);
    pill.appendChild(document.createTextNode(b.status === "pending" ? "queued" : "converting"));
  } else if (b.status === "error") {
    pill.textContent = "error";
    pill.title = b.error || "";
  }
  return pill;
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
  tr.dataset.bookId = b.id;

  tr.appendChild(cell(b.title || "Untitled"));
  tr.appendChild(cell(b.author || ""));
  tr.appendChild(cell(b.language || ""));
  tr.appendChild(cell(formatDate(b.imported_at)));
  tr.appendChild(cell(formatBytes(b.file_size)));

  const statusTd = document.createElement("td");
  const wrap = document.createElement("span");
  wrap.className = `status-cell ${b.status}`;
  const dot = document.createElement("span");
  dot.className = "dot";
  const txt = document.createElement("span");
  txt.textContent = b.status;
  wrap.append(dot, txt);
  statusTd.appendChild(wrap);
  if (b.status === "error") {
    statusTd.title = b.error || "";
    const retry = document.createElement("button");
    retry.className = "row-retry";
    retry.textContent = "Retry";
    retry.addEventListener("click", (e) => {
      e.stopPropagation();
      retryConvert(b.id);
    });
    statusTd.appendChild(retry);
  }
  tr.appendChild(statusTd);

  tr.addEventListener("dblclick", () => openInFinder(b.id));
  tr.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    openContextMenu(e.clientX, e.clientY, b);
  });
  return tr;
}

function cell(text) {
  const td = document.createElement("td");
  td.textContent = text;
  td.title = text;
  return td;
}

$$("#list th[data-sort]").forEach((th) => {
  th.addEventListener("click", () => {
    const key = th.dataset.sort;
    if (state.sort.key === key) state.sort.asc = !state.sort.asc;
    else state.sort = { key, asc: true };
    persistPreferences();
    render();
  });
});

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
  const sentSet = new Set(state.sent.map((s) => s.sha256));
  const unsent = state.books.filter(
    (b) => b.status === "done" && !sentSet.has(b.sha256),
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
  const sentSet = new Set(state.sent.map((s) => s.sha256));
  const count = state.books.filter(
    (b) => b.status === "done" && !sentSet.has(b.sha256),
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
    if (!$("#device-popover").hidden) refreshDeviceList();
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
    state.sent = [];
    $("#device-empty").textContent = "Plug in a Kindle via USB.";
    $("#device-empty").hidden = false;
  }
}

async function refreshDeviceList() {
  if (!state.device) return;
  try {
    state.sent = await window.api.invoke("device_list_ours");
  } catch (e) {
    console.error("device_list_ours failed:", e);
    state.sent = [];
  }
  renderDeviceList();
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
  if (state.device && b.status === "done") {
    const sentSet = new Set(state.sent.map((s) => s.sha256));
    if (sentSet.has(b.sha256)) {
      add(menu, "Remove from Kindle", () => deleteFromDevice([b.sha256], [b.title]));
    } else {
      add(menu, "Send to Kindle", () => sendBooks([b.id]));
    }
  }
  add(menu, "Open in Finder", () => openInFinder(b.id));
  add(menu, "Force re-convert", () => retryConvert(b.id));
  add(menu, "Remove from library", () => removeBook(b), true);
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
