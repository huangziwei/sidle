// Files section: what a Sync brings off the Kindle besides books.
//
// Classic script loaded AFTER library.js. Self-contained IIFE exposing
// `window.Misc` ({ refresh, show, hide, invalidate, editCollections }); the
// section toggle in library.js drives show/hide, and each Sync path calls
// invalidate() to refresh.
//
// One group per collection in the library's device-sync.json, in that order,
// each hidden when it has no files. Within a group, images render as a
// thumbnail grid and everything else as a list — inferred per file, so a folder
// of drafts reads as a list and a folder of captures as a grid without either
// having to say so. Both open the shared #misc-viewer overlay (image or <pre>).
//
// Also owns the settings editor for the collections themselves, since it is the
// same vocabulary. Reuses the global `window.api` (IPC + fileUrl) and
// `window.showToast`. Backend: commands/misc.rs.
(function () {
  const api = window.api;
  const q = (sel) => document.querySelector(sel);

  const IMAGE_EXT = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];

  const state = {
    groups: [], // [{id, label}] from misc_list, in config order
    list: [], // MiscFile[] from misc_list, newest first
    loaded: false,
    viewerPath: null, // absolute path of the file the overlay is showing, or null
    editing: null, // the collections being edited in settings, or null
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

  function isImage(f) {
    const dot = f.name.lastIndexOf(".");
    if (dot < 0) return false;
    return IMAGE_EXT.includes(f.name.slice(dot + 1).toLowerCase());
  }

  function isVisible() {
    const el = q("#misc");
    return !!el && !el.hidden;
  }

  // ── Public surface ─────────────────────────────────────────────────────────

  async function refresh() {
    try {
      const listing = await api.invoke("misc_list");
      state.groups = listing.groups ?? [];
      state.list = listing.files ?? [];
    } catch (e) {
      toast(`failed to load backed-up files: ${e}`, true);
      state.groups = [];
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
    const content = q("#misc-content");
    const hasAny = state.list.length > 0;
    q("#misc-empty").hidden = hasAny;
    content.hidden = !hasAny;
    content.innerHTML = "";
    for (const group of state.groups) {
      const files = state.list.filter((f) => f.collection === group.id);
      if (files.length === 0) continue; // a folder with nothing shows nothing
      content.appendChild(groupSection(group, files));
    }
  }

  function groupSection(group, files) {
    const section = document.createElement("section");
    section.className = "misc-group";

    const header = document.createElement("header");
    header.className = "misc-group-header";
    const name = document.createElement("strong");
    name.textContent = group.label;
    header.appendChild(name);
    const count = document.createElement("span");
    count.className = "misc-count";
    count.textContent = `${files.length}`;
    header.appendChild(count);
    section.appendChild(header);

    const images = files.filter(isImage);
    const rest = files.filter((f) => !isImage(f));
    if (images.length > 0) {
      const grid = document.createElement("div");
      grid.className = "misc-shots-grid";
      for (const f of images) grid.appendChild(shotTile(f));
      section.appendChild(grid);
    }
    if (rest.length > 0) {
      const list = document.createElement("ul");
      list.className = "misc-logs-list";
      for (const f of rest) list.appendChild(fileRow(f));
      section.appendChild(list);
    }
    return section;
  }

  function shotTile(f) {
    const wrap = document.createElement("div");
    wrap.className = "misc-shot";

    // The image + caption is the click target (opens the lightbox). A nested
    // button would be invalid HTML, hence the wrapper div.
    const open = document.createElement("button");
    open.type = "button";
    open.className = "misc-shot-open";
    open.title = `${f.name} · ${fmtSize(f.size)}${f.modified ? " · " + fmtDate(f.modified) : ""}`;

    const img = document.createElement("img");
    img.loading = "lazy";
    img.alt = f.name;
    img.src = api.fileUrl(f.path);
    open.appendChild(img);

    const cap = document.createElement("span");
    cap.className = "misc-shot-cap";
    cap.textContent = f.modified ? fmtDate(f.modified) : f.name;
    open.appendChild(cap);
    open.addEventListener("click", () => openImage(f));
    wrap.appendChild(open);

    wrap.appendChild(deleteButton(f, "misc-shot-del", "✕"));
    return wrap;
  }

  // A delete control for one backup file: removes the local copy, then refreshes.
  // `label` is the button text; `cls` its extra class.
  function deleteButton(f, cls, label) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = cls;
    btn.title = "Delete this backup copy";
    btn.textContent = label;
    btn.addEventListener("click", (e) => {
      e.stopPropagation(); // never trigger the tile/row's own open handler
      deleteFile(f);
    });
    return btn;
  }

  async function deleteFile(f) {
    try {
      await api.invoke("misc_delete", { path: f.path });
      if (state.viewerPath === f.path) closeViewer();
      await refresh();
    } catch (e) {
      toast(`delete failed: ${e}`, true);
    }
  }

  function fileRow(f) {
    const li = document.createElement("li");
    li.className = "misc-log-row";

    const name = document.createElement("button");
    name.type = "button";
    name.className = "misc-log-name btn-link";
    name.textContent = f.name;
    name.addEventListener("click", () => openText(f));
    li.appendChild(name);

    const meta = document.createElement("span");
    meta.className = "misc-log-meta";
    const bits = [fmtSize(f.size)];
    if (f.modified) bits.push(fmtDate(f.modified));
    if (f.device) bits.push(f.device);
    meta.textContent = bits.join(" · ");
    li.appendChild(meta);

    li.appendChild(deleteButton(f, "misc-log-del btn-link", "Delete"));
    return li;
  }

  // ── Viewer overlay (image or text) ───────────────────────────────────────

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

  async function openText(f) {
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
      log.textContent = `Failed to read: ${e}`;
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

  // Delete the file the viewer is currently showing (closes the overlay).
  async function deleteCurrent() {
    if (state.viewerPath) await deleteFile({ path: state.viewerPath });
  }

  // ── Settings: which Kindle folders a Sync brings back ─────────────────────

  // Load the library's collections into the settings editor. Called when the
  // settings modal opens, so the rows always reflect what is on disk.
  async function editCollections() {
    try {
      const cfg = await api.invoke("misc_collections_get");
      // `loadedId` is where this collection's files currently sit. Saving under
      // a different id has to move them, and this snapshot is the only record of
      // where they were — nothing else remembers the id before the edit.
      state.editing = (cfg.collections ?? []).map((c) => ({ ...c, loadedId: c.id }));
      setCollectionsStatus(null);
    } catch (e) {
      state.editing = [];
      setCollectionsStatus(`failed to load folders: ${e}`, true);
    }
    renderCollections();
  }

  // The library folder a collection's files are stored in, derived from what the
  // user named it. Deliberately NOT from the folder on the Kindle: that path is
  // the volatile one — an app's folder gets renamed — while the name the user
  // gave the collection is theirs. Either can still change, which is what
  // `renames` is for; this only decides what the storage folder is called.
  function storageId(label) {
    const slug = (label ?? "")
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "");
    return slug || "collection";
  }

  function setCollectionsStatus(msg, isError = false) {
    const el = q("#settings-collections-status");
    if (!el) return;
    el.hidden = !msg;
    el.textContent = msg ?? "";
    el.classList.toggle("error", !!isError);
  }

  function renderCollections() {
    const host = q("#settings-collections");
    if (!host) return;
    host.innerHTML = "";
    (state.editing ?? []).forEach((c, i) => host.appendChild(collectionRow(c, i)));
  }

  // One editable collection. Nothing here is a permanent identity: the Kindle
  // folder can be renamed, the name can be renamed, and saving moves whatever
  // was already synced to match. The row shows where in the library the files
  // land so that follow-the-name is visible rather than a surprise later.
  function collectionRow(c, index) {
    const row = document.createElement("div");
    row.className = "settings-collection";

    // Lists are comma-separated, not space-separated: a folder on the Kindle may
    // well have a space in its name, and splitting on one would quietly turn it
    // into two folders that don't exist.
    const LISTS = ["dirs", "include", "purge"];
    const field = (label, value, key, placeholder) => {
      const wrap = document.createElement("label");
      wrap.className = "settings-collection-field";
      const span = document.createElement("span");
      span.textContent = label;
      wrap.appendChild(span);
      const input = document.createElement("input");
      input.type = "text";
      input.value = value;
      if (placeholder) input.placeholder = placeholder;
      input.addEventListener("input", () => {
        state.editing[index][key] = LISTS.includes(key)
          ? input.value.split(",").map((s) => s.trim()).filter(Boolean)
          : input.value.trim();
      });
      wrap.appendChild(input);
      return wrap;
    };

    const name = field("Name", c.label, "label", "Drafts");
    row.appendChild(name);
    row.appendChild(field("Folder", c.dirs.join(", "), "dirs", "writing"));
    row.appendChild(field("Files", c.include.join(", "), "include", "*.md"));

    // Where the files land, kept live as the name is typed. A rename moves what
    // was already synced, so this is a preview of the move, not a warning.
    const where = document.createElement("p");
    where.className = "settings-collection-where";
    const showWhere = () => {
      const id = storageId(state.editing[index].label);
      const from = state.editing[index].loadedId;
      where.textContent = from && from !== id
        ? `In your library: ${id} — moved from ${from} when you save`
        : `In your library: ${id}`;
    };
    showWhere();
    name.querySelector("input").addEventListener("input", showWhere);
    row.appendChild(where);

    const flags = document.createElement("div");
    flags.className = "settings-collection-flags";
    flags.appendChild(checkbox("Subfolders", c.recursive, (v) => {
      state.editing[index].recursive = v;
    }));
    flags.appendChild(checkbox("Only copy new files", c.update === "once", (v) => {
      state.editing[index].update = v ? "once" : "always";
    }, "Leave a file alone once it's been copied. Off means each Sync takes the Kindle's current copy — right for anything that grows or gets edited."));
    flags.appendChild(checkbox("Delete from Kindle after Sync", c.clear_device, (v) => {
      state.editing[index].clear_device = v;
    }, "The library keeps the only copy afterwards."));
    row.appendChild(flags);

    row.appendChild(field("Also delete, never copy", (c.purge ?? []).join(", "), "purge",
      "wininfo_screenshot*"));

    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "btn-link settings-collection-remove";
    remove.textContent = "Remove";
    remove.title = "Stop syncing this folder. Files already copied stay in your library.";
    remove.addEventListener("click", () => {
      state.editing.splice(index, 1);
      renderCollections();
    });
    row.appendChild(remove);
    return row;
  }

  function checkbox(label, checked, onChange, title) {
    const wrap = document.createElement("label");
    wrap.className = "settings-collection-check";
    if (title) wrap.title = title;
    const input = document.createElement("input");
    input.type = "checkbox";
    input.checked = !!checked;
    input.addEventListener("change", () => onChange(input.checked));
    wrap.appendChild(input);
    const span = document.createElement("span");
    span.textContent = label;
    wrap.appendChild(span);
    return wrap;
  }

  // A new row has no `loadedId`: nothing is stored for it yet, so its first save
  // is a create rather than a move.
  function addCollection() {
    if (!state.editing) state.editing = [];
    state.editing.push({
      id: "",
      label: "",
      dirs: [],
      include: ["*"],
      recursive: true,
      update: "always",
      clear_device: false,
      purge: [],
    });
    renderCollections();
  }

  async function saveCollections() {
    const rows = state.editing ?? [];
    if (rows.some((c) => !c.label || c.dirs.length === 0)) {
      setCollectionsStatus("every folder needs a name and a path on the Kindle", true);
      return;
    }
    // No patterns means nothing matches, which would read as "synced, empty".
    if (rows.some((c) => c.include.length === 0)) {
      setCollectionsStatus("every folder needs a file pattern — use * for everything", true);
      return;
    }
    // The storage folder follows the name. `renames` is what makes that safe:
    // it tells the backend where each collection's files are now, so they move
    // with it rather than being stranded under an id nothing refers to.
    const collections = rows.map(({ loadedId: _drop, ...c }) => ({
      ...c,
      id: storageId(c.label),
    }));
    const ids = collections.map((c) => c.id);
    const dup = ids.find((id, i) => ids.indexOf(id) !== i);
    if (dup) {
      setCollectionsStatus(`two folders would both be stored as “${dup}” — rename one`, true);
      return;
    }
    const renames = rows
      .map((c, i) => [c.loadedId, collections[i].id])
      .filter(([from, to]) => from && from !== to);
    try {
      const saved = await api.invoke("misc_collections_set", { config: { collections }, renames });
      state.editing = (saved.collections ?? []).map((c) => ({ ...c, loadedId: c.id }));
      renderCollections();
      setCollectionsStatus(
        renames.length > 0
          ? `Saved — ${renames.length} folder${renames.length === 1 ? "" : "s"} renamed, ` +
            "carrying everything already synced into them. Takes effect on the next Sync."
          : "Saved — takes effect on the next Sync.",
      );
      invalidate(); // group labels, order, and storage folders may all have changed
    } catch (e) {
      setCollectionsStatus(`save failed: ${e}`, true);
    }
  }

  function wireViewer() {
    q("#misc-viewer-close")?.addEventListener("click", closeViewer);
    q("#misc-viewer-backdrop")?.addEventListener("click", closeViewer);
    q("#misc-viewer-reveal")?.addEventListener("click", revealCurrent);
    q("#misc-viewer-delete")?.addEventListener("click", deleteCurrent);
    q("#settings-collections-add")?.addEventListener("click", addCollection);
    q("#settings-collections-save")?.addEventListener("click", saveCollections);
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

  window.Misc = { refresh, show, hide, invalidate, editCollections };
})();
