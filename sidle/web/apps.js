// Apps section: the programs that install to a Kindle's /mnt/us and run there.
//
// Classic script loaded AFTER library.js. Self-contained IIFE exposing
// `window.Apps` ({ refresh, show, hide, invalidate }); the section toggle in
// library.js drives show/hide, and a device connect/disconnect calls
// invalidate() so the per-row device state follows the cable.
//
// This is the only place an app is managed. Install, update, remove and add all
// live here — a second entry point would be a second state to keep true.
//
// One row per app, in the order a push would carry them. A row's left half is
// what the app *is* (name, version, where it comes from); its right half is
// what the connected Kindle has (installed version, and what a push would
// write). With no Kindle the right half is simply absent — what an app is does
// not depend on a cable. Backend: commands/apps.rs, commands/device.rs.
(function () {
  const api = window.api;
  const q = (sel) => document.querySelector(sel);

  const state = {
    overview: null, // AppsOverview from apps_overview
    busy: null, // app id currently installing, or "*" for Update all
  };

  function toast(msg, isError = false) {
    if (typeof window.showToast === "function") window.showToast(msg, isError);
    else if (isError) console.error(msg);
  }

  function fmtSize(bytes) {
    if (bytes == null) return "";
    if (bytes < 1024) return `${bytes} B`;
    const kb = bytes / 1024;
    if (kb < 1024) return `${kb.toFixed(kb < 10 ? 1 : 0)} KB`;
    const mb = kb / 1024;
    if (mb < 1024) return `${mb.toFixed(mb < 10 ? 1 : 0)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
  }

  // ---- data ---------------------------------------------------------------

  async function refresh() {
    try {
      state.overview = await api.invoke("apps_overview");
    } catch (err) {
      state.overview = null;
      toast(`Could not read the apps: ${err}`, true);
    }
    render();
  }

  // A device connect or disconnect changes every row's right half. Only worth
  // a read while the tab is on screen — show() re-reads anyway.
  function invalidate() {
    if (!q("#apps").hidden) refresh();
  }

  // Always re-read. The sources are local trees a `build.sh` can rewrite at any
  // moment, so a cached row is a row that quietly stops being true; the walk is
  // cheap enough that there is nothing to save by keeping one.
  function show() {
    refresh();
  }

  function hide() {}

  // ---- what a row says ----------------------------------------------------

  // The device half of a row, as a label plus the class that colours it. Null
  // when there is nothing true to say — no Kindle, or an app whose source
  // could not be read at all.
  function deviceState(app, connected) {
    if (app.error) return { label: "source unreadable", cls: "bad" };
    if (!connected) return null;
    const d = app.device;
    if (!d) return { label: "—", cls: "" };
    switch (d.overall.kind) {
      case "in_sync":
        return {
          label: d.installed_version
            ? `Installed ${d.installed_version}`
            : "Installed",
          cls: "ok",
        };
      case "not_installed":
        return { label: "Not installed", cls: "warn" };
      case "binary_not_built":
        return { label: "Binary not built", cls: "bad" };
      case "stale":
        return {
          label: d.installed_version
            ? `Update to ${d.version}`
            : `${d.write_count} file${d.write_count === 1 ? "" : "s"} to write`,
          cls: "warn",
        };
      default:
        return { label: d.overall.kind, cls: "" };
    }
  }

  // What a push would move, named before it runs. Absent when there is nothing
  // to do, so "Installed" is not followed by a cost.
  function preflight(app) {
    const d = app.device;
    if (!d || !d.write_count) return "";
    return `${d.write_count} file${d.write_count === 1 ? "" : "s"} · ${fmtSize(d.write_bytes)}`;
  }

  // ---- rendering ----------------------------------------------------------

  function render() {
    const list = q("#apps-list");
    const empty = q("#apps-empty");
    const note = q("#apps-note");
    if (!list) return;

    const ov = state.overview;
    const apps = ov?.apps || [];
    list.innerHTML = "";
    empty.hidden = apps.length > 0;
    list.hidden = apps.length === 0;

    for (const app of apps) list.appendChild(renderRow(app, ov));

    renderSummary(ov);

    // Two apps claiming one path, and a device that is plugged in but cannot be
    // read. Both are things the rows above cannot say for themselves.
    const lines = [];
    for (const c of ov?.conflicts || []) {
      lines.push(`${c.dropped} also claims ${c.path} — ${c.kept}'s is used.`);
    }
    if (ov?.device_error) lines.push(`Kindle: ${ov.device_error}`);
    note.textContent = lines.join(" ");
    note.hidden = lines.length === 0;

    const updateAll = q("#apps-update-all");
    const pending = apps.some((a) => a.device && a.device.write_count > 0);
    updateAll.disabled = !ov?.device_connected || !pending || state.busy != null;
    updateAll.textContent = ov?.device_connected ? "Update all" : "No Kindle";
  }

  function renderSummary(ov) {
    const el = q("#apps-summary");
    if (!el) return;
    const apps = ov?.apps || [];
    if (!apps.length) {
      el.textContent = "";
      return;
    }
    const files = apps.reduce((n, a) => n + a.file_count, 0);
    const bytes = apps.reduce((n, a) => n + a.total_bytes, 0);
    const hashed = apps.reduce((n, a) => n + a.hashed_bytes, 0);
    // The second figure is the `sync` bytes — what a status check reads. It is
    // far below the total for an app with a large `seed` subtree.
    const checked = hashed < bytes ? `, ${fmtSize(hashed)} checked` : "";
    el.textContent =
      `${apps.length} app${apps.length === 1 ? "" : "s"} · ` +
      `${files} files · ${fmtSize(bytes)}${checked}`;
  }

  function renderRow(app, ov) {
    const li = document.createElement("li");
    li.className = "apps-row";

    const main = document.createElement("div");
    main.className = "apps-row-main";

    const name = document.createElement("span");
    name.className = "apps-name";
    name.textContent = app.name;
    main.appendChild(name);

    const version = document.createElement("span");
    version.className = "apps-version";
    version.textContent = app.version;
    main.appendChild(version);

    const source = document.createElement("span");
    source.className = "apps-source";
    // No source means the app ships inside Sidle — the picker and bokai.
    source.textContent = app.source || "bundled with Sidle";
    if (app.source) source.title = app.source;
    main.appendChild(source);
    li.appendChild(main);

    const meta = document.createElement("div");
    meta.className = "apps-row-meta";

    const st = deviceState(app, ov?.device_connected);
    if (st) {
      const badge = document.createElement("span");
      badge.className = `apps-state ${st.cls}`;
      badge.textContent = st.label;
      if (app.error) badge.title = app.error;
      meta.appendChild(badge);
    }

    const cost = preflight(app);
    if (cost) {
      const pre = document.createElement("span");
      pre.className = "apps-preflight";
      pre.textContent = cost;
      meta.appendChild(pre);
    }

    const size = document.createElement("span");
    size.className = "apps-size";
    size.textContent = `${app.file_count} files · ${fmtSize(app.total_bytes)}`;
    meta.appendChild(size);
    li.appendChild(meta);

    li.appendChild(renderActions(app, ov));
    return li;
  }

  function renderActions(app, ov) {
    const actions = document.createElement("div");
    actions.className = "apps-row-actions";

    const d = app.device;
    if (ov?.device_connected && !app.error) {
      const install = document.createElement("button");
      install.type = "button";
      install.className = "btn-link";
      install.textContent = !d
        ? "Install"
        : d.overall.kind === "not_installed"
          ? "Install"
          : d.write_count > 0
            ? "Update"
            : "Re-push";
      install.disabled = state.busy != null;
      install.addEventListener("click", () => installOne(app.id));
      actions.appendChild(install);
    }

    // Only a registered app can be forgotten; the picker and bokai ship inside
    // Sidle and have no registration to undo.
    if (app.source) {
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "btn-link apps-remove";
      remove.textContent = "Remove";
      remove.title =
        `Unregister ${app.id}.\nNothing on disk or on the Kindle is touched.`;
      remove.disabled = state.busy != null;
      remove.addEventListener("click", () => removeOne(app.id));
      actions.appendChild(remove);
    }
    return actions;
  }

  // ---- actions ------------------------------------------------------------

  async function installOne(id) {
    await runInstall(id, id);
  }

  async function updateAll() {
    await runInstall(null, "*");
  }

  // One path for a row and for the fleet: `only` is the single difference, so
  // the two cannot drift apart.
  async function runInstall(only, busyKey) {
    state.busy = busyKey;
    render();
    const label = only || "every app";
    try {
      const report = await api.invoke("device_app_install", { only });
      const wrote = report.results.filter((r) => r.kind === "wrote").length;
      const failed = report.results.filter((r) => r.kind === "failed");
      if (failed.length) {
        toast(`${label}: ${failed.length} file(s) failed — ${failed[0].error}`, true);
      } else if (wrote === 0) {
        toast(`${label}: already up to date`);
      } else {
        toast(`${label}: pushed ${wrote} file${wrote === 1 ? "" : "s"}`);
      }
    } catch (err) {
      toast(`${label}: ${err}`, true);
    } finally {
      state.busy = null;
      await refresh();
    }
  }

  async function removeOne(id) {
    try {
      await api.invoke("apps_remove", { id });
    } catch (err) {
      toast(`Could not unregister ${id}: ${err}`, true);
    }
    await refresh();
  }

  async function add() {
    let folder;
    try {
      folder = await api.invoke("library_pick_folder");
    } catch (err) {
      toast(`${err}`, true);
      return;
    }
    if (!folder) return;
    try {
      const added = await api.invoke("apps_add", { path: folder });
      toast(`Added ${added.map((a) => a.name).join(", ")}`);
    } catch (err) {
      toast(`${err}`, true);
    }
    await refresh();
  }

  function wire() {
    q("#apps-add")?.addEventListener("click", add);
    q("#apps-update-all")?.addEventListener("click", updateAll);
    // Per-file progress during a push. The rows redraw on completion; this is
    // the running commentary in between, without which an app of a hundred-odd
    // files looks frozen.
    api.listen("device-app:install-progress", (e) => {
      if (state.busy == null) return;
      const r = e.payload;
      const el = q("#apps-summary");
      if (!el) return;
      if (r.kind === "wrote") el.textContent = `writing ${r.device_path}`;
      else if (r.kind === "failed") el.textContent = `failed ${r.device_path}`;
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", wire);
  } else {
    wire();
  }

  window.Apps = { refresh, show, hide, invalidate };
})();
