// Notes section: Scribe handwritten-notebook grid + paged SVG viewer.
//
// Classic script loaded AFTER library.js. Self-contained IIFE that exposes
// `window.Notebooks` ({ refresh, show, hide, importFolder }); library.js's
// Books/Notes toggle drives it. Reuses the global `window.api` (IPC + fileUrl)
// and `window.showToast` when present. Backend: commands/notebook.rs —
// notebook_list / notebook_page_svg / notebook_thumbnail / notebook_rename /
// notebook_remove / notebook_import_folder.
(function () {
  const api = window.api;
  const q = (sel) => document.querySelector(sel);

  // Module state. `viewer` is null unless the paged SVG viewer is open.
  const nb = {
    list: [],
    loaded: false,
    viewer: null, // { id, title, pageCount, page, cache: Map<page, svgString> }
  };

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
    nb.loaded = true;
    render();
  }

  // Called when the Notes tab is shown. Loads lazily on first show, re-renders
  // (from cached list) on subsequent shows.
  function show() {
    if (!nb.loaded) refresh();
    else render();
  }

  // Called when leaving the Notes tab. The viewer (if open) is a fixed overlay
  // independent of the tab, so there's nothing to tear down here.
  function hide() {}

  // ── Grid ───────────────────────────────────────────────────────────────────

  function render() {
    const grid = q("#notes-grid");
    const empty = q("#notes-empty");
    if (!grid) return;
    grid.innerHTML = "";
    for (const n of nb.list) grid.appendChild(card(n));
    grid.hidden = nb.list.length === 0;
    if (empty) empty.hidden = nb.list.length > 0;
  }

  function card(n) {
    const el = document.createElement("div");
    el.className = "book-card notebook-card";
    el.dataset.notebookId = n.id;
    el.title = n.title || "Notebook";

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

    el.addEventListener("click", () => openViewer(n));
    el.addEventListener("dblclick", () => openViewer(n));
    el.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      openMenu(e.clientX, e.clientY, n);
    });
    return el;
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

    const rename = document.createElement("li");
    rename.textContent = "Rename…";
    rename.addEventListener("click", (e) => {
      e.stopPropagation();
      closeMenu();
      startRename(n);
    });

    // Two-click confirm for the destructive action (no window.confirm — the app
    // avoids native dialogs; see the inline confirm in Settings).
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

  // ── Manual folder import ────────────────────────────────────────────────────

  async function importFolder() {
    let summary;
    try {
      summary = await api.invoke("notebook_import_folder");
    } catch (e) {
      toast(`import failed: ${e}`, true);
      return;
    }
    if (!summary) return; // picker cancelled
    const failed = (summary.failed && summary.failed.length) || 0;
    const parts = [];
    if (summary.imported) parts.push(`${summary.imported} imported`);
    if (summary.unchanged) parts.push(`${summary.unchanged} unchanged`);
    if (failed) parts.push(`${failed} failed`);
    toast(parts.join(" · ") || "no notebooks found in that folder", failed > 0);
    if (failed) console.error("notebook import failures:", summary.failed);
    await refresh();
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
    if (emptyImport) emptyImport.addEventListener("click", importFolder);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", wire);
  } else {
    wire();
  }

  window.Notebooks = { refresh, show, hide, importFolder };
})();
