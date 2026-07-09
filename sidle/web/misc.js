// Misc. section: screenshots + KUAL logs backed up off the Kindle on Sync.
//
// Classic script loaded AFTER library.js. Self-contained IIFE exposing
// `window.Misc` ({ refresh, show, hide, invalidate }); library.js's section
// toggle drives show/hide, and each Sync path calls invalidate() to refresh.
// A screenshot thumbnail grid + a log list, each opening an in-app viewer (the
// shared #misc-viewer overlay — image for screenshots, <pre> for logs). Reuses
// the global `window.api` (IPC + fileUrl) and `window.showToast`. Backend:
// commands/misc.rs — misc_list / misc_read_text / misc_reveal.
(function () {
  const api = window.api;
  const q = (sel) => document.querySelector(sel);

  const state = {
    list: [], // MiscFile[] from misc_list, newest first
    loaded: false,
    viewerPath: null, // absolute path of the file the overlay is showing, or null
  };

  function toast(msg, isError = false) {
    if (typeof window.showToast === "function") window.showToast(msg, isError);
    else if (isError) console.error(msg);
    else console.log(msg);
  }

  function fmtDate(iso) {
    if (!iso) return "";
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    const p = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ` +
      `${p(d.getHours())}:${p(d.getMinutes())}`;
  }

  // Binary size, one decimal past KB — matches the native picker's `human_mb`.
  function fmtSize(bytes) {
    if (bytes < 1024) return `${bytes} B`;
    const kb = bytes / 1024;
    if (kb < 1024) return `${kb.toFixed(kb < 10 ? 1 : 0)} KB`;
    return `${(kb / 1024).toFixed(1)} MB`;
  }

  function isVisible() {
    const el = q("#misc");
    return !!el && !el.hidden;
  }

  // ── Public surface ─────────────────────────────────────────────────────────

  async function refresh() {
    try {
      state.list = await api.invoke("misc_list");
    } catch (e) {
      toast(`failed to load misc backups: ${e}`, true);
      state.list = [];
    }
    state.loaded = true;
    render();
  }

  // Lazy-load on first show; re-render from cache afterwards. The device sync
  // clears `loaded` (via window.Misc.invalidate) so new backups show on return.
  function show() {
    if (!state.loaded) refresh();
    else render();
  }

  function hide() {
    closeViewer();
  }

  // Called after a Sync backs up new files, so the next `show()` re-fetches
  // rather than serving a stale cached list.
  function invalidate() {
    state.loaded = false;
    if (isVisible()) refresh();
  }

  // ── Render ───────────────────────────────────────────────────────────────

  function render() {
    const shots = state.list.filter((f) => f.kind === "screenshot");
    const logs = state.list.filter((f) => f.kind === "log");
    const hasAny = state.list.length > 0;

    q("#misc-empty").hidden = hasAny;
    q("#misc-content").hidden = !hasAny;

    renderShots(shots);
    renderLogs(logs);
  }

  function renderShots(shots) {
    const group = q("#misc-shots-group");
    const grid = q("#misc-shots-grid");
    group.hidden = shots.length === 0;
    q("#misc-shots-count").textContent = shots.length ? `${shots.length}` : "";
    grid.innerHTML = "";
    for (const f of shots) grid.appendChild(shotTile(f));
  }

  function shotTile(f) {
    const el = document.createElement("button");
    el.type = "button";
    el.className = "misc-shot";
    el.title = `${f.name} · ${fmtSize(f.size)}${f.modified ? " · " + fmtDate(f.modified) : ""}`;

    const img = document.createElement("img");
    img.loading = "lazy";
    img.alt = f.name;
    img.src = api.fileUrl(f.path);
    el.appendChild(img);

    const cap = document.createElement("span");
    cap.className = "misc-shot-cap";
    cap.textContent = f.modified ? fmtDate(f.modified) : f.name;
    el.appendChild(cap);

    el.addEventListener("click", () => openImage(f));
    return el;
  }

  function renderLogs(logs) {
    const group = q("#misc-logs-group");
    const list = q("#misc-logs-list");
    group.hidden = logs.length === 0;
    q("#misc-logs-count").textContent = logs.length ? `${logs.length}` : "";
    list.innerHTML = "";
    for (const f of logs) list.appendChild(logRow(f));
  }

  function logRow(f) {
    const li = document.createElement("li");
    li.className = "misc-log-row";

    const name = document.createElement("button");
    name.type = "button";
    name.className = "misc-log-name btn-link";
    name.textContent = f.name;
    name.addEventListener("click", () => openLog(f));
    li.appendChild(name);

    const meta = document.createElement("span");
    meta.className = "misc-log-meta";
    const bits = [fmtSize(f.size)];
    if (f.modified) bits.push(fmtDate(f.modified));
    if (f.device) bits.push(f.device);
    meta.textContent = bits.join(" · ");
    li.appendChild(meta);
    return li;
  }

  // ── Viewer overlay (image or log) ────────────────────────────────────────

  function openImage(f) {
    state.viewerPath = f.path;
    q("#misc-viewer-title").textContent = f.name;
    const img = q("#misc-viewer-img");
    const log = q("#misc-viewer-log");
    log.hidden = true;
    log.textContent = "";
    img.src = api.fileUrl(f.path);
    img.hidden = false;
    openViewer();
  }

  async function openLog(f) {
    state.viewerPath = f.path;
    q("#misc-viewer-title").textContent = f.name;
    const img = q("#misc-viewer-img");
    const log = q("#misc-viewer-log");
    img.hidden = true;
    img.src = "";
    log.textContent = "Loading…";
    log.hidden = false;
    openViewer();
    try {
      log.textContent = await api.invoke("misc_read_text", { path: f.path });
      log.scrollTop = log.scrollHeight; // logs append — land on the newest lines
    } catch (e) {
      log.textContent = `Failed to read log: ${e}`;
    }
  }

  function openViewer() {
    q("#misc-viewer").hidden = false;
  }

  function closeViewer() {
    const v = q("#misc-viewer");
    if (v) v.hidden = true;
    state.viewerPath = null;
    const img = q("#misc-viewer-img");
    if (img) img.src = "";
  }

  async function revealCurrent() {
    if (!state.viewerPath) return;
    try {
      await api.invoke("misc_reveal", { path: state.viewerPath });
    } catch (e) {
      toast(`reveal failed: ${e}`, true);
    }
  }

  function wireViewer() {
    q("#misc-viewer-close")?.addEventListener("click", closeViewer);
    q("#misc-viewer-backdrop")?.addEventListener("click", closeViewer);
    q("#misc-viewer-reveal")?.addEventListener("click", revealCurrent);
    // Esc closes the overlay when it's open. Capture-phase + stopPropagation so
    // it beats library.js's global Esc (clear-selection) while the viewer is up;
    // when the viewer is closed this is inert and Esc falls through as usual.
    document.addEventListener(
      "keydown",
      (e) => {
        if (e.key === "Escape" && !q("#misc-viewer").hidden) {
          e.stopPropagation();
          closeViewer();
        }
      },
      true,
    );
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", wireViewer);
  } else {
    wireViewer();
  }

  window.Misc = { refresh, show, hide, invalidate };
})();
