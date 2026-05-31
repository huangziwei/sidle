// Notes section: Scribe handwritten-notebook grid/list + paged SVG viewer.
//
// Classic script loaded AFTER library.js. Self-contained IIFE that exposes
// `window.Notebooks` ({ refresh, show, hide, importDevice, setView, … }); library.js's
// Books/Notes toggle drives it, and its Gallery/List toggle calls setView().
// Multi-select (click / cmd / shift) + bulk remove mirror the Books side, with
// a dedicated #notebook-selection-bar. Reuses the global `window.api` (IPC +
// fileUrl) and `window.showToast` when present. Backend: commands/notebook.rs —
// notebook_list / notebook_page_svg / notebook_thumbnail / notebook_rename /
// notebook_remove / notebook_import_folder.
(function () {
  const api = window.api;
  const q = (sel) => document.querySelector(sel);

  // Module state. `viewer` is null unless the paged SVG viewer is open.
  const nb = {
    list: [],
    loaded: false,
    view: "gallery", // "gallery" | "list" — driven by library.js's view toggle
    sort: loadNbSort(), // { key, asc } — list-header sort, applied to grid + list
    viewer: null, // { id, title, pageCount, page, cache: Map<page, svgString> }
  };

  // The Notes section's multi-select — the SAME SelectionController the Books
  // section uses (library.js), so click / cmd / shift / lasso / select-all all
  // behave identically. library.js's #main mousedown + keydown handlers route to
  // this via window.Notebooks.selection() when Notes is the active section.
  const sel = new window.SelectionController({
    idAttr: "notebookId",
    orderedIds: () => nb.list.map((n) => n.id),
    containers: () =>
      document.querySelectorAll(
        nb.view === "list" ? "#notes-list tbody tr" : "#notes-grid .notebook-card",
      ),
    // render() empties the inactive view, but paint both for symmetry with Books.
    paintContainers: () => [
      ...document.querySelectorAll("#notes-grid .notebook-card"),
      ...document.querySelectorAll("#notes-list tbody tr"),
    ],
    lassoEl: () => document.querySelector("#lasso"),
    skipSelector: ".notebook-card, #notes-list tbody tr, #notes-list thead",
    onChange: () => renderSelectionBar(),
  });

  // The Notes list view uses the SAME shared TableView as Books (table.js): its
  // columns become sortable, drag-to-reorder, and resizable, with order + widths
  // persisted — no bespoke notebook table. Sort lives in nb.sort (applied to the
  // grid too), so the table only renders the indicator + reports header clicks.
  const NOTEBOOK_COLUMNS = [
    { key: "title", label: "Title", sortable: true, render: (n) => n.title || "Notebook" },
    { key: "pages", label: "Pages", sortable: true, render: (n) => String(n.page_count) },
    { key: "updated", label: "Updated", sortable: true, render: (n) => fmtDate(n.updated_at) },
  ];

  const nbTable = new window.TableView({
    table: q("#notes-list"),
    columns: NOTEBOOK_COLUMNS,
    idOf: (n) => n.id,
    idAttr: "notebookId",
    configKey: "notebooks.columnConfig",
    widthsKey: "notebooks.columnWidths",
    getSort: () => nb.sort,
    onSort: (key) => {
      if (nb.sort.key === key) nb.sort.asc = !nb.sort.asc;
      else nb.sort = { key, asc: true };
      persistNbSort();
      render();
    },
    isSelected: (id) => sel.has(id),
    onRowClick: (e, n) => sel.click(e, n.id),
    onRowDblClick: (n) => openViewer(n),
    onRowContext: (e, n) => {
      sel.context(n.id);
      openMenu(e.clientX, e.clientY, n);
    },
    onChange: () => render(),
    ctxMenu: q("#ctx-menu"),
  });

  // Two-click arm for the selection bar's destructive "Remove from library"
  // (the app avoids native confirm() dialogs — see the context-menu remove).
  let removeArmed = false;

  // True while a device import is in flight — gates re-entry and progress paints.
  let importing = false;

  function fmtDate(iso) {
    if (typeof window.formatDate === "function") return window.formatDate(iso);
    return iso || "";
  }

  function loadNbSort() {
    try {
      const s = JSON.parse(localStorage.getItem("notebooks.sort") || "null");
      if (s && typeof s.key === "string") return { key: s.key, asc: !!s.asc };
    } catch {
      // malformed — fall through to default
    }
    return { key: "updated", asc: false }; // most-recently-edited first
  }
  function persistNbSort() {
    localStorage.setItem("notebooks.sort", JSON.stringify(nb.sort));
  }

  // nb.list in the current sort order — drives BOTH the grid and the list view.
  function sortedList() {
    const { key, asc } = nb.sort;
    const dir = asc ? 1 : -1;
    const val = (n) =>
      key === "pages"
        ? n.page_count
        : key === "updated"
          ? n.updated_at || ""
          : (n.title || "Notebook").toLowerCase();
    return [...nb.list].sort((a, b) => {
      const av = val(a);
      const bv = val(b);
      if (typeof av === "number" && typeof bv === "number") return (av - bv) * dir;
      return String(av).localeCompare(String(bv)) * dir;
    });
  }

  // The Notes section is the active `.view` (not `hidden`) only while the user
  // is on the Notes tab. Used to skip selection/keyboard work in Books mode.
  function isVisible() {
    const el = q("#notes");
    return !!el && !el.hidden;
  }

  function toast(msg, isError = false) {
    if (typeof window.showToast === "function") window.showToast(msg, isError);
    else if (isError) console.error(msg);
    else console.log(msg);
  }

  // ── Public surface ─────────────────────────────────────────────────────────

  async function refresh() {
    try {
      nb.list = await api.invoke("notebook_list");
    } catch (e) {
      toast(`failed to load notebooks: ${e}`, true);
      nb.list = [];
    }
    // Drop selection entries for notebooks that no longer exist (after a
    // remove/import the list changes underneath us).
    sel.prune(new Set(nb.list.map((n) => n.id)));
    nb.loaded = true;
    render();
  }

  // Mirror the library's Gallery/List toggle. Stores the choice; re-renders only
  // when Notes is actually on screen so Books-side toggles don't refetch thumbs.
  function setView(view) {
    if (view !== "gallery" && view !== "list") return;
    if (view === nb.view) return;
    nb.view = view;
    if (nb.loaded && isVisible()) render();
  }

  // Called when the Notes tab is shown. Loads lazily on first show, re-renders
  // (from cached list) on subsequent shows.
  function show() {
    if (!nb.loaded) refresh();
    else render();
  }

  // Called when leaving the Notes tab. The viewer (if open) is a fixed overlay
  // independent of the tab. Drop the selection so its bar doesn't linger over
  // the Books view.
  function hide() {
    sel.selected.clear();
    sel.lastClicked = null;
    const bar = q("#notebook-selection-bar");
    if (bar) bar.hidden = true;
    removeArmed = false;
  }

  // ── Grid ───────────────────────────────────────────────────────────────────

  function render() {
    const grid = q("#notes-grid");
    const list = q("#notes-list");
    const empty = q("#notes-empty");
    const isList = nb.view === "list";
    const hasItems = nb.list.length > 0;
    const items = sortedList();

    // Only the ACTIVE view is (re)built — rebuilding the grid would re-fetch every
    // tile thumbnail, and the hidden view is rebuilt when it next becomes active.
    if (grid) {
      if (!isList) {
        grid.innerHTML = "";
        for (const n of items) grid.appendChild(card(n));
      }
      grid.hidden = isList || !hasItems;
    }
    if (list) {
      if (isList) nbTable.render(items); // shared TableView: sort/reorder/resize
      list.hidden = !isList || !hasItems;
    }
    if (empty) empty.hidden = hasItems;
    renderSelectionBar();

    // Seed column widths once the list is actually on screen with rows.
    if (isList && hasItems && isVisible()) {
      requestAnimationFrame(() => nbTable.ensureWidths());
    }
  }

  function card(n) {
    const el = document.createElement("div");
    el.className = "book-card notebook-card";
    el.dataset.notebookId = n.id;
    el.title = n.title || "Notebook";
    if (sel.has(n.id)) el.classList.add("selected");

    const cover = document.createElement("div");
    cover.className = "cover notebook-cover";
    loadThumb(n, cover);
    el.appendChild(cover);

    const meta = document.createElement("div");
    meta.className = "meta";
    const t = document.createElement("div");
    t.className = "t";
    t.textContent = n.title || "Notebook";
    const a = document.createElement("div");
    a.className = "a";
    a.textContent = `${n.page_count} page${n.page_count === 1 ? "" : "s"}`;
    meta.append(t, a);
    el.appendChild(meta);

    // Single click selects (mirrors Books); double click opens the viewer.
    el.addEventListener("click", (e) => sel.click(e, n.id));
    el.addEventListener("dblclick", () => openViewer(n));
    el.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      sel.context(n.id);
      openMenu(e.clientX, e.clientY, n);
    });
    return el;
  }

  // ── Selection bar + bulk remove ─────────────────────────────────────────────
  // The selection MECHANICS (click / cmd / shift / lasso / select-all / clear)
  // are the shared `sel` controller; only the bar's content and the bulk-remove
  // action are notebook-specific.

  function selectedNotebooks() {
    return nb.list.filter((n) => sel.has(n.id));
  }

  function renderSelectionBar() {
    const bar = q("#notebook-selection-bar");
    if (!bar) return;
    const n = sel.count();
    removeArmed = false; // any re-render disarms the two-click remove
    if (n === 0) {
      bar.hidden = true;
      return;
    }
    bar.hidden = false;
    q("#notebook-selection-count").textContent = `${n} selected`;
    const del = q("#nb-sel-delete");
    if (del) del.textContent = "Remove from library";
  }

  async function bulkRemove() {
    const picked = selectedNotebooks();
    if (picked.length === 0) return;
    let failed = 0;
    for (const n of picked) {
      try {
        await api.invoke("notebook_remove", { notebookId: n.id });
      } catch (e) {
        failed += 1;
        if (failed === 1) toast(`remove failed for “${n.title || "Notebook"}”: ${e}`, true);
        console.error("notebook remove failed:", n.id, e);
      }
    }
    const removed = picked.length - failed;
    if (removed > 0) toast(`removed ${removed} notebook${removed === 1 ? "" : "s"}`);
    sel.selected.clear();
    sel.lastClicked = null;
    await refresh();
  }

  // Tile thumbnail: prefer the device cover PNG; fall back to page 0's SVG; then
  // a text placeholder. Async — fills `coverEl` in place.
  async function loadThumb(n, coverEl) {
    try {
      const path = await api.invoke("notebook_thumbnail", { notebookId: n.id });
      if (path) {
        const url = api.fileUrl(path);
        if (url) {
          coverEl.classList.add("has-image");
          const img = document.createElement("img");
          img.src = url;
          img.alt = "";
          img.loading = "lazy";
          coverEl.appendChild(img);
          return;
        }
      }
    } catch {
      // fall through to the SVG / placeholder fallbacks
    }
    if (n.page_count > 0) {
      try {
        const svg = await api.invoke("notebook_page_svg", { notebookId: n.id, page: 0 });
        if (svg) {
          coverEl.classList.add("has-image", "svg-thumb");
          coverEl.innerHTML = svg;
          const el = coverEl.querySelector("svg");
          if (el) el.classList.add("notebook-thumb-svg");
          return;
        }
      } catch {
        // fall through to placeholder
      }
    }
    const ph = document.createElement("div");
    ph.className = "cover-placeholder";
    ph.textContent = n.title || "Notebook";
    coverEl.appendChild(ph);
  }

  // ── Context menu (rename / remove) ──────────────────────────────────────────
  // Reuses the shared #ctx-menu element. library.js's own openContextMenu wires
  // a one-shot close-on-click only when IT opens the menu, so we register our
  // own dismissers here.

  function openMenu(x, y, n) {
    const menu = q("#ctx-menu");
    if (!menu) return;
    menu.innerHTML = "";

    const multi = sel.count() > 1 && sel.has(n.id);
    if (multi) {
      // Bulk remove for the whole selection. Two-click confirm (no native
      // dialog), same convention as the single-item remove below.
      const remove = document.createElement("li");
      remove.className = "danger";
      remove.textContent = `Remove ${sel.count()} from library`;
      let armed = false;
      remove.addEventListener("click", (e) => {
        e.stopPropagation();
        if (!armed) {
          armed = true;
          remove.textContent = "Click again to remove";
          return;
        }
        closeMenu();
        bulkRemove();
      });
      menu.append(remove);
    } else {
      const rename = document.createElement("li");
      rename.textContent = "Rename…";
      rename.addEventListener("click", (e) => {
        e.stopPropagation();
        closeMenu();
        startRename(n);
      });

      // Two-click confirm for the destructive action (no window.confirm — the
      // app avoids native dialogs; see the inline confirm in Settings).
      const remove = document.createElement("li");
      remove.className = "danger";
      remove.textContent = "Remove from library";
      let armed = false;
      remove.addEventListener("click", (e) => {
        e.stopPropagation();
        if (!armed) {
          armed = true;
          remove.textContent = "Click again to remove";
          return;
        }
        closeMenu();
        doRemove(n);
      });

      menu.append(rename, remove);
    }
    menu.hidden = false;
    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;
    requestAnimationFrame(() => {
      const r = menu.getBoundingClientRect();
      if (r.right > window.innerWidth) menu.style.left = `${window.innerWidth - r.width - 4}px`;
      if (r.bottom > window.innerHeight) menu.style.top = `${window.innerHeight - r.height - 4}px`;
    });

    // Dismiss on the next outside interaction / Escape.
    const onDocDown = (ev) => {
      if (!menu.contains(ev.target)) closeMenu();
    };
    const onEsc = (ev) => {
      if (ev.key === "Escape") closeMenu();
    };
    function closeMenu() {
      menu.hidden = true;
      menu.innerHTML = "";
      document.removeEventListener("mousedown", onDocDown);
      document.removeEventListener("keydown", onEsc);
    }
    setTimeout(() => {
      document.addEventListener("mousedown", onDocDown);
      document.addEventListener("keydown", onEsc);
    }, 0);
  }

  // Inline rename: swap the card's title for an input (no native prompt dialog).
  function startRename(n) {
    const card = q(`#notes-grid .notebook-card[data-notebook-id="${n.id}"]`);
    const tEl = card && card.querySelector(".meta .t");
    if (!tEl) return;

    const input = document.createElement("input");
    input.className = "notebook-rename-input";
    input.value = n.title || "";
    tEl.replaceWith(input);
    input.focus();
    input.select();

    let done = false;
    const commit = async () => {
      if (done) return;
      done = true;
      const title = input.value.trim();
      if (!title || title === n.title) {
        render();
        return;
      }
      try {
        await api.invoke("notebook_rename", { notebookId: n.id, title });
        await refresh();
      } catch (e) {
        toast(`rename failed: ${e}`, true);
        render();
      }
    };
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        commit();
      } else if (e.key === "Escape") {
        e.preventDefault();
        done = true;
        render();
      }
    });
    input.addEventListener("blur", commit);
  }

  async function doRemove(n) {
    try {
      await api.invoke("notebook_remove", { notebookId: n.id });
      toast(`removed “${n.title || "Notebook"}”`);
      await refresh();
    } catch (e) {
      toast(`remove failed: ${e}`, true);
    }
  }

  // ── Import ──────────────────────────────────────────────────────────────────

  // Default path (the toolbar + empty-state buttons): pull notebooks straight
  // off the connected Kindle over MTP. Each notebook's on-device Date Modified
  // becomes its `updated_at`. The pull + decode is slow over USB, so the button
  // shows live "Importing N/M…" progress (see the listener in wire()).
  async function importDevice() {
    if (importing) return; // already running — ignore a double click
    importing = true;
    setImportBusy("Importing…");
    toast("Importing notebooks from Kindle…");
    let summary;
    try {
      summary = await api.invoke("notebook_import_device");
    } catch (e) {
      toast(`${e}`, true); // e.g. "no Kindle connected"
      return;
    } finally {
      importing = false;
      setImportBusy(null);
    }
    reportImport(summary, "no notebooks on the Kindle");
    await refresh();
  }

  // Reflect import state on the Import buttons: a non-null label busies + disables
  // the toolbar button (where the user clicked) and disables the empty-state
  // button; `null` restores the idle "Import".
  function setImportBusy(label) {
    const btn = q("#btn-notes-import");
    const empty = q("#notes-empty-import");
    const busy = label !== null;
    if (btn) {
      btn.disabled = busy;
      btn.textContent = busy ? label : "Import";
    }
    if (empty) empty.disabled = busy;
  }

  // Fallback path (not wired to a button): import from a picked `.notebooks/`
  // folder. Kept for importing a backup when no device is attached.
  async function importFolder() {
    let summary;
    try {
      summary = await api.invoke("notebook_import_folder");
    } catch (e) {
      toast(`import failed: ${e}`, true);
      return;
    }
    if (!summary) return; // picker cancelled
    reportImport(summary, "no notebooks found in that folder");
    await refresh();
  }

  function reportImport(summary, emptyMsg) {
    if (!summary) return;
    const failed = (summary.failed && summary.failed.length) || 0;
    const parts = [];
    if (summary.imported) parts.push(`${summary.imported} imported`);
    if (summary.unchanged) parts.push(`${summary.unchanged} unchanged`);
    if (failed) parts.push(`${failed} failed`);
    toast(parts.join(" · ") || emptyMsg, failed > 0);
    if (failed) console.error("notebook import failures:", summary.failed);
  }

  // ── Paged SVG viewer ────────────────────────────────────────────────────────

  async function openViewer(n) {
    nb.viewer = {
      id: n.id,
      title: n.title || "Notebook",
      pageCount: n.page_count,
      page: 0,
      cache: new Map(),
    };
    q("#notebook-title").textContent = nb.viewer.title;
    q("#notebook-view").hidden = false;
    document.addEventListener("keydown", onViewerKey);
    if (nb.viewer.pageCount > 0) {
      await showPage(0);
    } else {
      q("#notebook-page-host").innerHTML = "";
      updatePageInfo();
    }
  }

  function closeViewer() {
    const view = q("#notebook-view");
    if (view) view.hidden = true;
    const host = q("#notebook-page-host");
    if (host) host.innerHTML = "";
    document.removeEventListener("keydown", onViewerKey);
    nb.viewer = null;
  }

  async function showPage(i) {
    const v = nb.viewer;
    if (!v || v.pageCount === 0) return;
    i = Math.max(0, Math.min(v.pageCount - 1, i));
    v.page = i;
    updatePageInfo();

    let svg = v.cache.get(i);
    if (svg == null) {
      try {
        svg = await api.invoke("notebook_page_svg", { notebookId: v.id, page: i });
        v.cache.set(i, svg);
      } catch (e) {
        toast(`failed to render page ${i + 1}: ${e}`, true);
        return;
      }
    }
    // The user may have paged again while we awaited — only paint if still here.
    if (!nb.viewer || nb.viewer.page !== i) return;
    const host = q("#notebook-page-host");
    host.innerHTML = svg || "";
    const el = host.querySelector("svg");
    if (el) el.classList.add("notebook-page-svg");
    prefetch(i + 1);
  }

  // Warm the next page's SVG so a forward turn is instant.
  function prefetch(i) {
    const v = nb.viewer;
    if (!v || i < 0 || i >= v.pageCount || v.cache.has(i)) return;
    api
      .invoke("notebook_page_svg", { notebookId: v.id, page: i })
      .then((svg) => {
        if (nb.viewer === v) v.cache.set(i, svg);
      })
      .catch(() => {});
  }

  function updatePageInfo() {
    const v = nb.viewer;
    if (!v) return;
    q("#notebook-pageinfo").textContent = v.pageCount
      ? `${v.page + 1} / ${v.pageCount}`
      : "—";
    q("#notebook-nav-left").style.visibility = v.page > 0 ? "" : "hidden";
    q("#notebook-nav-right").style.visibility =
      v.page < v.pageCount - 1 ? "" : "hidden";
  }

  function onViewerKey(e) {
    if (!nb.viewer) return;
    if (e.key === "Escape") {
      closeViewer();
    } else if (e.key === "ArrowLeft" || e.key === "ArrowUp" || e.key === "PageUp") {
      e.preventDefault();
      showPage(nb.viewer.page - 1);
    } else if (
      e.key === "ArrowRight" ||
      e.key === "ArrowDown" ||
      e.key === "PageDown" ||
      e.key === " "
    ) {
      e.preventDefault();
      showPage(nb.viewer.page + 1);
    }
  }

  // ── Wiring ──────────────────────────────────────────────────────────────────

  function wire() {
    const close = q("#notebook-close");
    if (close) close.addEventListener("click", closeViewer);
    const left = q("#notebook-nav-left");
    if (left)
      left.addEventListener("click", () => nb.viewer && showPage(nb.viewer.page - 1));
    const right = q("#notebook-nav-right");
    if (right)
      right.addEventListener("click", () => nb.viewer && showPage(nb.viewer.page + 1));
    const emptyImport = q("#notes-empty-import");
    if (emptyImport) emptyImport.addEventListener("click", importDevice);

    // Live device-import progress → the Import button shows "Importing N/M…".
    if (typeof api.listen === "function") {
      api.listen("notebook:import-progress", (e) => {
        if (!importing) return;
        const p = e && e.payload;
        if (p && p.total) setImportBusy(`Importing ${p.done}/${p.total}…`);
      });
    }

    // Selection bar. The selection mechanics (click / lasso / Esc / Cmd-A /
    // empty-click) come from the shared controller, driven by library.js's #main
    // + document handlers when Notes is active — only these two buttons are
    // notebook-specific.
    const selDelete = q("#nb-sel-delete");
    if (selDelete) {
      selDelete.addEventListener("click", () => {
        if (!removeArmed) {
          removeArmed = true;
          selDelete.textContent = `Click again to remove ${sel.count()}`;
          return;
        }
        removeArmed = false;
        bulkRemove();
      });
    }
    const selClear = q("#nb-sel-clear");
    if (selClear) selClear.addEventListener("click", () => sel.clear());
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", wire);
  } else {
    wire();
  }

  // `selection` + `viewerOpen` let library.js's shared #main mousedown + keydown
  // handlers drive the Notes controller when Notes is the active section.
  window.Notebooks = {
    refresh,
    show,
    hide,
    importDevice,
    importFolder,
    setView,
    selection: () => sel,
    viewerOpen: () => !!nb.viewer,
  };
})();
