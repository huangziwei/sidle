// Apps section: the programs that install to a Kindle's /mnt/us.
//
// Classic script loaded after library.js, exposing `window.Apps`
// ({ refresh, show, hide, invalidate, setView }). List view is one row per app;
// gallery view is one tile per app, carrying the art from its launcher.
// Backend: commands/apps.rs, commands/device.rs.
(function () {
  const api = window.api;
  const q = (sel) => document.querySelector(sel);

  const state = {
    overview: null, // AppsOverview from apps_overview
    device: null, // AppsDeviceStatus, once one has landed
    deviceBusy: false, // a device read is in flight
    deviceStale: false, // a refresh landed during that read
    busy: null, // app id installing, or "*" for Update all
    seq: 0, // issue number of the newest refresh
    view: "list", // "gallery" | "list", set by library.js's toggle
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

  // Two calls, rendered as each lands: `apps_overview`, then the Kindle read.
  // `state.seq` numbers each refresh; a reply that is not the newest is dropped.
  async function refresh() {
    const mine = ++state.seq;
    let overview = null;
    let error = null;
    try {
      overview = await api.invoke("apps_overview");
    } catch (err) {
      error = err;
    }
    if (mine !== state.seq) return;
    state.overview = overview;
    const connected = !!overview?.device_connected;
    if (!connected) state.device = null;
    if (error) toast(`Could not read the apps: ${error}`, true);
    // `readDevice` sets `state.deviceBusy` before its first await; the render
    // below paints that as "Checking…".
    if (connected) readDevice();
    render();
  }

  // One read of the Kindle at a time. A refresh during one sets
  // `state.deviceStale`, and the reply that lands starts the next read.
  // `state.device` holds the last status through both.
  async function readDevice() {
    if (state.deviceBusy) {
      state.deviceStale = true;
      return;
    }
    state.deviceBusy = true;
    const mine = state.seq;
    let status = null;
    let error = null;
    try {
      status = await api.invoke("apps_device_status");
    } catch (err) {
      error = err;
    }
    state.deviceBusy = false;
    if (mine === state.seq) {
      state.device = status || { apps: [], error: `${error}` };
      // `state.busy` names a push: it writes the summary line itself and
      // refreshes at its end.
      if (state.busy == null) render();
    }
    if (state.deviceStale) {
      state.deviceStale = false;
      if (state.overview?.device_connected) readDevice();
    }
  }

  // This app's state on the Kindle, or null while it is unread.
  function statusOf(app) {
    return state.device?.apps.find((a) => a.id === app.id) || null;
  }

  // Re-reads while the Apps section is open and no push is running.
  // `runInstall` refreshes at the end of one.
  function invalidate() {
    if (state.busy != null) return;
    if (!q("#apps").hidden) refresh();
  }

  function show() {
    refresh();
  }

  function hide() {}

  // The toolbar's Gallery/List toggle, handed over by library.js's applyView.
  function setView(view) {
    if (view !== "gallery" && view !== "list") return;
    if (view === state.view) return;
    state.view = view;
    if (!q("#apps").hidden) render();
  }

  // ---- what a row says ----------------------------------------------------

  // The device half of a row: a label plus the class that colours it. Null with
  // no Kindle, and for an app whose source could not be read.
  function deviceState(app, connected) {
    if (app.error) return { label: "source unreadable", cls: "bad" };
    if (!connected) return null;
    const d = statusOf(app);
    if (!d) return { label: state.deviceBusy ? "Checking…" : "—", cls: "" };
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
      case "diverged_only":
        return { label: "Changed on Kindle", cls: "warn" };
      case "stale":
        return {
          label:
            d.installed_version && d.version
              ? `Update to ${d.version}`
              : `${d.write_count} file${d.write_count === 1 ? "" : "s"} to write`,
          cls: "warn",
        };
      default:
        return { label: d.overall.kind, cls: "" };
    }
  }

  // What a push writes, and what it keeps. Empty when there is neither.
  function preflight(app) {
    const d = statusOf(app);
    if (!d) return "";
    const parts = [];
    if (d.write_count) {
      parts.push(
        `${d.write_count} file${d.write_count === 1 ? "" : "s"} · ${fmtSize(d.write_bytes)}`,
      );
    }
    if (d.diverged_count) {
      parts.push(
        `${d.diverged_count} kept — changed on the Kindle`,
      );
    }
    return parts.join(" · ");
  }

  // ---- rendering ----------------------------------------------------------

  function render() {
    const list = q("#apps-list");
    const grid = q("#apps-grid");
    const empty = q("#apps-empty");
    const note = q("#apps-note");
    if (!list || !grid) return;

    const ov = state.overview;
    const apps = ov?.apps || [];
    const gallery = state.view === "gallery";
    list.innerHTML = "";
    grid.innerHTML = "";
    empty.hidden = apps.length > 0;
    list.hidden = gallery || apps.length === 0;
    grid.hidden = !gallery || apps.length === 0;

    const into = gallery ? grid : list;
    const build = gallery ? renderCard : renderRow;
    for (const app of apps) into.appendChild(build(app, ov));

    renderSummary(ov);

    // Two apps claiming one path, and a connected device that cannot be read.
    const lines = [];
    for (const c of ov?.conflicts || []) {
      lines.push(`${c.dropped} also claims ${c.path} — ${c.kept}'s is used.`);
    }
    if (state.device?.error) lines.push(`Kindle: ${state.device.error}`);
    note.textContent = lines.join(" ");
    note.hidden = lines.length === 0;

    const updateAll = q("#apps-update-all");
    const pending = apps.some((a) => statusOf(a)?.write_count > 0);
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
    const parts = [
      `${apps.length} app${apps.length === 1 ? "" : "s"}`,
      `${files} files`,
      fmtSize(bytes),
      wifiSummary(ov),
    ];
    el.textContent = parts.filter(Boolean).join(" · ");
  }

  // How much of the fleet a Kindle's own Update button reaches. Short of every
  // app when the cross-built picker is absent.
  function wifiSummary(ov) {
    const apps = ov?.apps || [];
    const offered = apps.filter((a) => a.offered).length;
    if (!offered) return "Wi-Fi: nothing offered";
    return `Wi-Fi: ${offered} of ${apps.length} offered`;
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

    // `app.version` is absent for a tree that states none.
    if (app.version) {
      const version = document.createElement("span");
      version.className = "apps-version";
      version.textContent = app.version;
      main.appendChild(version);
    }

    const source = document.createElement("span");
    source.className = "apps-source";
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

    if (!app.offered && !app.error) {
      const wifi = document.createElement("span");
      wifi.className = "apps-preflight";
      wifi.textContent = "Wi-Fi: not offered";
      meta.appendChild(wifi);
    }

    const size = document.createElement("span");
    size.className = "apps-size";
    size.textContent = `${app.file_count} files · ${fmtSize(app.total_bytes)}`;
    meta.appendChild(size);
    li.appendChild(meta);

    li.appendChild(renderActions(app, ov));
    return li;
  }

  // One tile per app: the art its `documents/*.sh` carries, or the app's name
  // over a plain ground when it carries none.
  function renderCard(app, ov) {
    const card = document.createElement("div");
    card.className = "apps-card";

    const cover = document.createElement("div");
    cover.className = "apps-cover";
    if (app.icon) {
      cover.classList.add("has-image");
      const img = document.createElement("img");
      img.src = app.icon;
      img.alt = "";
      cover.appendChild(img);
    } else {
      const placeholder = document.createElement("div");
      placeholder.className = "apps-cover-placeholder";
      placeholder.textContent = app.name;
      cover.appendChild(placeholder);
    }
    card.appendChild(cover);

    const meta = document.createElement("div");
    meta.className = "apps-card-meta";

    const name = document.createElement("div");
    name.className = "apps-name";
    name.textContent = app.name;
    name.title = app.source || "bundled with Sidle";
    meta.appendChild(name);

    const st = deviceState(app, ov?.device_connected);
    const sub = document.createElement("div");
    sub.className = `apps-card-state ${st ? st.cls : ""}`;
    sub.textContent = st ? st.label : app.version || "";
    if (app.error) sub.title = app.error;
    meta.appendChild(sub);

    meta.appendChild(renderActions(app, ov));
    card.appendChild(meta);
    return card;
  }

  function renderActions(app, ov) {
    const actions = document.createElement("div");
    actions.className = "apps-row-actions";

    const d = statusOf(app);
    // The device actions below read `d`; `known` gates them on having it.
    const known = !state.deviceBusy || d != null;
    if (ov?.device_connected && !app.error && known) {
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

      // `d.diverged_count` files the Kindle changed, which a plain push keeps.
      if (d && d.diverged_count) {
        actions.appendChild(
          button("Overwrite", () => overwriteOne(app.id), {
            title:
              `Replace the ${d.diverged_count} file(s) changed on the Kindle ` +
              `with this build's.\nWhatever they hold now is lost.`,
          }),
        );
      }

      // Something on the Kindle to take off it.
      if (d && d.overall.kind !== "not_installed") {
        actions.appendChild(
          button("Remove from Kindle", () => uninstallOne(app), {
            danger: true,
            title:
              `Delete extensions/${app.id}/ and its tile from the Kindle.\n` +
              `${app.name} stays in the Apps tab, ready to install again.`,
          }),
        );
      }
    }

    // `app.source` is absent for an app bundled with Sidle, which has no row to
    // drop.
    if (app.source) {
      actions.appendChild(
        button("Remove from library", () => removeOne(app), {
          danger: true,
          title:
            `Stop tracking ${app.name}.\nIts folder on this machine and its ` +
            `files on the Kindle are left alone.`,
        }),
      );
    }
    return actions;
  }

  function button(label, onClick, opts = {}) {
    const el = document.createElement("button");
    el.type = "button";
    el.className = opts.danger ? "btn-link apps-danger" : "btn-link";
    el.textContent = label;
    if (opts.title) el.title = opts.title;
    el.disabled = state.busy != null;
    el.addEventListener("click", onClick);
    return el;
  }

  // ---- actions ------------------------------------------------------------

  async function installOne(id) {
    await runInstall(id, id, false);
  }

  async function overwriteOne(id) {
    await runInstall(id, id, true);
  }

  async function updateAll() {
    await runInstall(null, "*", false);
  }

  // One path for a row and for the fleet; `only` is the single difference.
  async function runInstall(only, busyKey, force) {
    state.busy = busyKey;
    render();
    const label = only || "every app";
    try {
      const report = await api.invoke("device_app_install", { only, force });
      const wrote = report.results.filter((r) => r.kind === "wrote").length;
      const kept = report.results.filter(
        (r) => r.kind === "kept_device_copy",
      ).length;
      const failed = report.results.filter((r) => r.kind === "failed");
      const keptNote = kept
        ? ` · kept ${kept} changed on the Kindle`
        : "";
      if (failed.length) {
        toast(`${label}: ${failed.length} file(s) failed — ${failed[0].error}`, true);
      } else if (wrote === 0) {
        toast(`${label}: already up to date${keptNote}`);
      } else {
        toast(
          `${label}: pushed ${wrote} file${wrote === 1 ? "" : "s"}${keptNote}`,
        );
      }
    } catch (err) {
      toast(`${label}: ${err}`, true);
    } finally {
      state.busy = null;
      await refresh();
    }
  }

  // Drops the `apps` row. The folder on this machine and the files on the
  // Kindle are left where they are.
  async function removeOne(app) {
    const ok = window.confirm(
      `Remove ${app.name} from the library?\n\n` +
        `Sidle stops tracking ${app.source}. Nothing there is deleted, and ` +
        `anything already on a Kindle stays.`,
    );
    if (!ok) return;
    try {
      await api.invoke("apps_remove", { id: app.id });
      toast(`${app.name}: removed from the library`);
    } catch (err) {
      toast(`Could not remove ${app.name}: ${err}`, true);
    }
    await refresh();
  }

  // Deletes the app off the Kindle. The row stays, ready to install again.
  async function uninstallOne(app) {
    const ok = window.confirm(
      `Remove ${app.name} from the Kindle?\n\n` +
        `Deletes extensions/${app.id}/ and its tile from the device, along ` +
        `with anything it saved in there. ${app.name} stays in the Apps tab.`,
    );
    if (!ok) return;
    state.busy = app.id;
    render();
    try {
      const report = await api.invoke("device_app_uninstall", { id: app.id });
      if (report.errors.length) {
        toast(`${app.name}: ${report.errors[0]}`, true);
      } else if (!report.removed.length) {
        toast(`${app.name}: nothing on the Kindle`);
      } else {
        toast(`${app.name}: removed from the Kindle`);
      }
    } catch (err) {
      toast(`${app.name}: ${err}`, true);
    } finally {
      state.busy = null;
      await refresh();
    }
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
    // Per-file progress during a push, between renders.
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

  window.Apps = { refresh, show, hide, invalidate, setView };
})();
