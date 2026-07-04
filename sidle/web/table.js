// Shared list-view (data table) for every tab — the ONE implementation of
// sortable headers, drag-to-reorder columns, drag-to-resize column widths, and
// the right-click column-visibility menu. Books and Notes each instantiate one
// (see library.js `booksTable`, notebooks.js `nbTable`); future tabs do too,
// rather than re-inventing a table per tab.
//
// Per-section config supplies only what differs:
//   - table:      the <table> element (must contain <colgroup>, <thead><tr>,
//                 <tbody>; this fills them).
//   - columns:    [{ key, label, sortable, render(item)->string|Node, align? }].
//   - idOf(item), idAttr:  the row's numeric id + the dataset attr to stamp it on
//                 (so the shared SelectionController can find rows).
//   - configKey, widthsKey:  localStorage keys for this table's column
//                 order/visibility and widths (kept per-tab).
//   - getSort()/onSort(key):  SORT lives in the section (Books shares it with the
//                 gallery), so the table only renders the indicator + reports clicks.
//   - isSelected(id):  for the `.selected` row class (from the section's controller).
//   - onRowClick/onRowDblClick/onRowContext(e?, item):  row interactions.
//   - onChange():  re-render hook — called after a reorder / visibility change so
//                 the section repaints with the new layout.
//   - ctxMenu:    the shared #ctx-menu element (column-visibility menu).
//
// Column ORDER/VISIBILITY/WIDTHS are owned + persisted here. SORT and the DATA
// are the section's.
(function () {
  const REORDER_THRESHOLD = 4; // px a header drag must travel to become a reorder
  // How long after an inline editor opens a double-click still counts as "the
  // user meant to open the item, not edit" — see the row dblclick handler.
  const EDIT_FRESH_MS = 350;

  class TableView {
    constructor(cfg) {
      this.cfg = cfg;
      this.table = cfg.table;
      this.colgroup = cfg.table.querySelector("colgroup");
      this.headRow = cfg.table.querySelector("thead tr");
      this.tbody = cfg.table.querySelector("tbody");
      this.defs = Object.fromEntries(cfg.columns.map((c) => [c.key, c]));
      this.defaultOrder = cfg.columns.map((c) => c.key);
      this.columnConfig = this._loadConfig();
      this.widths = this._loadWidths();
      // The cell currently open in an inline editor, or null. Holds enough to
      // commit/cancel and to survive a background re-render (see render()).
      this.editing = null;
      this._editFresh = false; // true briefly after an editor opens
      // The <thead> element persists across renders (only its <tr> is rebuilt),
      // so wire the column-visibility menu ONCE here — not in _wireHeaders, where
      // a fresh arrow each render would accumulate listeners.
      this.table.querySelector("thead").addEventListener("contextmenu", (e) => this._onHeaderMenu(e));
    }

    // ── Persistence (column order/visibility + widths) ─────────────────────────

    _loadConfig() {
      let stored = null;
      try {
        stored = JSON.parse(localStorage.getItem(this.cfg.configKey) || "null");
      } catch {
        stored = null;
      }
      // Merge persisted with the current column set: drop unknown keys, append
      // newly-added columns at the end (visible) so a new feature column shows up
      // without nuking the user's order.
      const known = new Set(this.defaultOrder);
      const valid = (stored || [])
        .filter((c) => c && known.has(c.key))
        .map((c) => ({ key: c.key, visible: c.visible !== false }));
      const present = new Set(valid.map((c) => c.key));
      for (const key of this.defaultOrder) {
        if (!present.has(key)) valid.push({ key, visible: true });
      }
      return valid;
    }

    _saveConfig() {
      localStorage.setItem(this.cfg.configKey, JSON.stringify(this.columnConfig));
    }

    _loadWidths() {
      try {
        return JSON.parse(localStorage.getItem(this.cfg.widthsKey) || "{}") || {};
      } catch {
        return {};
      }
    }

    _saveWidths() {
      localStorage.setItem(this.cfg.widthsKey, JSON.stringify(this.widths));
    }

    // ── Render ─────────────────────────────────────────────────────────────────

    render(items) {
      const visible = this.columnConfig.filter((c) => c.visible);
      const sort = this.cfg.getSort();

      // Rebuild colgroup so the resize logic indexes correctly into it.
      this.colgroup.innerHTML = "";
      for (const c of visible) {
        const col = document.createElement("col");
        col.dataset.col = c.key;
        this.colgroup.appendChild(col);
      }

      // Header row.
      this.headRow.innerHTML = "";
      for (const c of visible) {
        this.headRow.appendChild(this._headerCell(this.defs[c.key], sort));
      }

      // Body. Preserve the row currently open in an inline editor across a
      // background re-render (a conversion-status tick, say) so an open editor
      // isn't destroyed mid-keystroke: its live <tr> — input, caret and focus
      // intact — is spliced back in at its new position, other rows rebuild
      // normally. A column change (which blurs+commits first) or the edited
      // item leaving the view cancels the edit instead.
      if (this.editing && this.editing.visKey !== this._visKey(visible)) this._cancelEdit();
      const editId = this.editing ? this.editing.id : null;
      let keptEdit = false;
      const frag = document.createDocumentFragment();
      for (const item of items) {
        if (editId != null && this.cfg.idOf(item) === editId) {
          keptEdit = true;
          frag.appendChild(this.editing.tr);
        } else {
          frag.appendChild(this._row(item, visible));
        }
      }
      this.tbody.innerHTML = "";
      this.tbody.appendChild(frag);
      if (editId != null && !keptEdit) this._cancelEdit();

      this._applyWidths();
      this._wireHeaders();
    }

    _headerCell(def, sort) {
      const th = document.createElement("th");
      th.dataset.col = def.key;
      if (def.sortable) {
        th.dataset.sort = def.key;
        th.classList.toggle("sorted", !!sort && sort.key === def.key);
        th.classList.toggle("asc", !!sort && !!sort.asc);
      }
      // .th-label is the drag handle. We do NOT use the HTML5 draggable API:
      // Tauri's webview (dragDropEnabled for file-drop import) intercepts native
      // drags at the OS level. Reorder is mouse-based (see _onLabelDown).
      const label = document.createElement("span");
      label.className = "th-label";
      label.textContent = def.label;
      th.appendChild(label);
      const resizer = document.createElement("span");
      resizer.className = "resizer";
      th.appendChild(resizer);
      return th;
    }

    _row(item, visible) {
      const tr = document.createElement("tr");
      const id = this.cfg.idOf(item);
      tr.dataset[this.cfg.idAttr] = id;
      if (this.cfg.isSelected(id)) tr.classList.add("selected");
      for (const c of visible) {
        const def = this.defs[c.key];
        const td = document.createElement("td");
        td.dataset.col = c.key; // addressable per-column (mirrors <col>/<th>)
        if (def.edit) td.classList.add("editable"); // click-to-edit affordance
        this._fillCell(td, def, item);
        tr.appendChild(td);
      }
      tr.addEventListener("click", (e) => {
        // Click-to-edit: on an already-selected row, a plain click on an
        // editable cell opens an inline editor instead of re-selecting
        // (Calibre-style). The first click on an unselected row just selects
        // (canEditNow stays false until it's the sole selection); the second
        // click of a double-click (detail > 1) is left for the reader.
        if (this._scheduleEdit(e, item)) return;
        this.cfg.onRowClick(e, item);
      });
      tr.addEventListener("dblclick", () => {
        // A double-click that only just popped the inline editor open means the
        // user meant to open the item, not edit it — roll the editor back and
        // hand off to the reader.
        if (this.editing && this.editing.tr === tr && this._editFresh) {
          this._cancelEdit();
          this.cfg.onRowDblClick(item);
          return;
        }
        if (this.editing) return; // an open editor owns the row; don't read
        this.cfg.onRowDblClick(item);
      });
      tr.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        this.cfg.onRowContext(e, item);
      });
      return tr;
    }

    // Paint a cell's normal (non-editing) content. Shared by the initial row
    // build and by the inline editor when it commits/cancels and restores the
    // cell to plain text.
    _fillCell(td, def, item) {
      td.innerHTML = "";
      const out = def.render(item);
      if (out instanceof Node) {
        td.title = "";
        td.appendChild(out);
      } else {
        td.textContent = out == null ? "" : String(out);
        td.title = td.textContent; // full value on truncation
      }
      if (def.align) td.style.textAlign = def.align;
    }

    // ── Inline cell editing ──────────────────────────────────────────────────
    // Click a field on a selected row to edit it. Opt-in per column via
    //   def.edit = { type: "text" | "select", get(item), options?(item) }
    // The section supplies onCellEdit(item, key, value) (persist + re-render)
    // and an optional canEditNow(id) (defaults to isSelected) gating the first
    // click. table.js owns only the interaction + the editor widget — it has no
    // idea what the values mean.

    _visKey(visible) {
      return (visible || this.columnConfig.filter((c) => c.visible)).map((c) => c.key).join(",");
    }

    // Decide whether a row click should open an editor; if so, open it and
    // return true (so the caller skips selection). Returns false to fall through
    // to normal row selection.
    _scheduleEdit(e, item) {
      if (e.detail > 1) return false; // part of a double-click → let it read
      if (e.metaKey || e.ctrlKey || e.shiftKey) return false; // modifiers select
      if (this.editing) return false;
      const td = e.target.closest("td");
      if (!td) return false;
      const tr = td.closest("tr");
      if (tr && tr.classList.contains("just-dragged")) return false; // trailing drag click
      const def = this.defs[td.dataset.col];
      if (!def || !def.edit) return false; // column isn't editable
      const id = this.cfg.idOf(item);
      const ready = this.cfg.canEditNow ? this.cfg.canEditNow(id) : this.cfg.isSelected(id);
      if (!ready) return false; // first click just selects the row
      this._openEditor(td, item, def);
      return true;
    }

    _openEditor(td, item, def) {
      if (this.editing) this._commitEdit(); // moving between cells commits the last
      const seed = String(def.edit.get(item) ?? "");
      let input;
      if (def.edit.type === "select") {
        input = document.createElement("select");
        for (const opt of def.edit.options(item)) {
          const o = document.createElement("option");
          o.value = opt.value;
          o.textContent = opt.label;
          input.appendChild(o);
        }
        input.value = seed;
        // Keep an unlisted stored value (an unusual code) selectable so merely
        // opening the editor never silently rewrites it.
        if (input.value !== seed) {
          const o = document.createElement("option");
          o.value = seed;
          o.textContent = seed || "—";
          input.insertBefore(o, input.firstChild);
          input.value = seed;
        }
      } else {
        input = document.createElement("input");
        input.type = "text";
        input.value = seed;
      }
      input.className = "cell-editor";
      // Keep the field's own mouse gestures from bubbling to the row (select /
      // read / drag). dblclick is deliberately NOT stopped so a just-opened
      // editor can still fall back to "open in reader" (see the row handler).
      input.addEventListener("mousedown", (ev) => ev.stopPropagation());
      input.addEventListener("click", (ev) => ev.stopPropagation());
      input.addEventListener("keydown", (ev) => this._onEditorKey(ev));
      input.addEventListener("blur", () => this._commitEdit());
      // A <select>: choosing an option is a complete edit, so commit at once.
      if (def.edit.type === "select") input.addEventListener("change", () => this._commitEdit());
      td.innerHTML = "";
      td.title = "";
      td.appendChild(input);

      // Commit when the user mouses down anywhere outside the field. Blur alone
      // isn't enough: drag-source rows and the selection lasso preventDefault on
      // mousedown, which suppresses the input's native blur. Capture phase so
      // this runs before those handlers.
      const onDocDown = (ev) => {
        if (ev.target !== input && !input.contains(ev.target)) this._commitEdit();
      };
      document.addEventListener("mousedown", onDocDown, true);

      this.editing = {
        id: this.cfg.idOf(item),
        item,
        def,
        td,
        input,
        seed,
        tr: td.closest("tr"),
        visKey: this._visKey(),
        onDocDown,
      };
      this._editFresh = true;
      setTimeout(() => {
        this._editFresh = false;
      }, EDIT_FRESH_MS);
      input.focus();
      if (input.select) input.select();
    }

    _onEditorKey(e) {
      // Mid-IME-composition (typing kana → kanji), Enter/Escape belong to the
      // input method (confirm/cancel the candidate), not to the cell.
      if (e.isComposing) return;
      if (e.key === "Enter") {
        e.preventDefault();
        e.stopPropagation();
        this._commitEdit();
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        this._cancelEdit();
      }
    }

    _commitEdit() {
      const ed = this.editing;
      if (!ed) return;
      this.editing = null; // release before any re-render onCellEdit triggers
      document.removeEventListener("mousedown", ed.onDocDown, true);
      const value = ed.input.value;
      this._fillCell(ed.td, ed.def, ed.item); // drop the input; show current value
      if (value === ed.seed) return; // unchanged → nothing to persist
      Promise.resolve(this.cfg.onCellEdit(ed.item, ed.def.key, value)).catch(() => {
        // The section surfaces its own error toast; the cell already reads back
        // the pre-edit value, so there's nothing to roll back here.
      });
    }

    _cancelEdit() {
      const ed = this.editing;
      if (!ed) return;
      this.editing = null;
      document.removeEventListener("mousedown", ed.onDocDown, true);
      this._fillCell(ed.td, ed.def, ed.item); // restore the original cell content
    }

    // ── Header interactions ─────────────────────────────────────────────────────

    _wireHeaders() {
      // Sort on header click — skip the resizer, and the synthetic click that
      // trails a drag-reorder.
      this.headRow.querySelectorAll("th[data-sort]").forEach((th) => {
        th.addEventListener("click", (e) => {
          if (e.target.classList.contains("resizer")) return;
          if (e.target.classList.contains("th-label") && th.classList.contains("just-dragged")) {
            th.classList.remove("just-dragged");
            return;
          }
          this.cfg.onSort(th.dataset.sort);
        });
      });
      this.headRow.querySelectorAll(".resizer").forEach((resizer, i) => {
        resizer.addEventListener("mousedown", (e) => this._onResizerDown(e, resizer, i));
      });
      this.headRow.querySelectorAll(".th-label").forEach((label) => {
        label.addEventListener("mousedown", (e) => this._onLabelDown(e));
      });
      // The header-row th/resizer/label listeners above are re-wired every render
      // because those elements are rebuilt; the <thead> contextmenu is wired once
      // in the constructor (the thead element persists).
    }

    // Drag-to-reorder (mouse-based, not HTML5 drag — see _headerCell).
    _onLabelDown(e) {
      if (e.button !== 0) return;
      const th = e.target.closest("th");
      if (!th) return;
      const fromKey = th.dataset.col;
      e.preventDefault(); // suppress text-selection; click still fires for sort

      const startX = e.clientX;
      const startY = e.clientY;
      let dragging = false;
      let ghost = null;

      const onMove = (ev) => {
        if (!dragging) {
          if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < REORDER_THRESHOLD) return;
          dragging = true;
          th.classList.add("dragging");
          ghost = document.createElement("div");
          ghost.className = "col-drag-ghost";
          ghost.textContent = this.defs[fromKey]?.label ?? fromKey;
          ghost.style.width = `${th.offsetWidth}px`;
          document.body.appendChild(ghost);
          document.body.style.cursor = "grabbing";
        }
        ghost.style.left = `${ev.clientX + 8}px`;
        ghost.style.top = `${ev.clientY + 8}px`;
        this.headRow.querySelectorAll("th").forEach((t) => t.classList.remove("drop-left", "drop-right"));
        const overTh = document.elementFromPoint(ev.clientX, ev.clientY)?.closest("th");
        if (overTh && this.headRow.contains(overTh) && overTh.dataset.col !== fromKey) {
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
        this.headRow
          .querySelectorAll("th")
          .forEach((t) => t.classList.remove("dragging", "drop-left", "drop-right"));
        if (!dragging) return; // plain click — let the sort handler run

        th.classList.add("just-dragged"); // suppress the trailing synthetic click
        const overTh = document.elementFromPoint(ev.clientX, ev.clientY)?.closest("th");
        if (!overTh || !this.headRow.contains(overTh) || overTh.dataset.col === fromKey) return;
        const r = overTh.getBoundingClientRect();
        const before = ev.clientX - r.left < r.width / 2;
        this._reorder(fromKey, overTh.dataset.col, before);
      };

      document.addEventListener("mousemove", onMove);
      document.addEventListener("mouseup", onUp);
    }

    _reorder(fromKey, toKey, before) {
      const order = [...this.columnConfig];
      const fromIdx = order.findIndex((c) => c.key === fromKey);
      if (fromIdx === -1) return;
      const [dragged] = order.splice(fromIdx, 1);
      let toIdx = order.findIndex((c) => c.key === toKey);
      if (toIdx === -1) return;
      if (!before) toIdx++;
      order.splice(toIdx, 0, dragged);
      this.columnConfig = order;
      this._saveConfig();
      this.cfg.onChange();
    }

    // Right-click anywhere in the header → column-visibility menu.
    _onHeaderMenu(e) {
      e.preventDefault();
      e.stopPropagation();
      const menu = this.cfg.ctxMenu;
      menu.innerHTML = "";
      const visibleCount = this.columnConfig.filter((c) => c.visible).length;
      for (const col of this.columnConfig) {
        const def = this.defs[col.key];
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
          this._saveConfig();
          this.cfg.onChange();
        });
        menu.appendChild(li);
      }
      menu.hidden = false;
      menu.style.left = `${e.clientX}px`;
      menu.style.top = `${e.clientY}px`;
      requestAnimationFrame(() => {
        const r = menu.getBoundingClientRect();
        if (r.right > window.innerWidth) menu.style.left = `${window.innerWidth - r.width - 4}px`;
        if (r.bottom > window.innerHeight) menu.style.top = `${window.innerHeight - r.height - 4}px`;
      });
    }

    _onResizerDown(e, resizer, idx) {
      e.preventDefault();
      e.stopPropagation();
      const col = this.colgroup.querySelectorAll("col")[idx];
      if (!col) return;
      const key = col.dataset.col;
      const startX = e.clientX;
      // <col> is invisible to layout (rect is 0), so measure the th instead.
      const th = resizer.closest("th");
      const startWidth = th ? th.getBoundingClientRect().width : this.widths[key] || 100;
      resizer.classList.add("active");
      document.body.style.cursor = "col-resize";
      const onMove = (ev) => {
        const w = Math.max(48, startWidth + ev.clientX - startX);
        this.widths[key] = w;
        col.style.width = `${w}px`;
      };
      const onUp = () => {
        document.removeEventListener("mousemove", onMove);
        document.removeEventListener("mouseup", onUp);
        resizer.classList.remove("active");
        document.body.style.cursor = "";
        this._saveWidths();
      };
      document.addEventListener("mousemove", onMove);
      document.addEventListener("mouseup", onUp);
    }

    _applyWidths() {
      this.colgroup.querySelectorAll("col").forEach((col) => {
        const w = this.widths[col.dataset.col];
        col.style.width = w ? `${w}px` : "";
      });
    }

    // Measure natural content widths and seed any missing column widths once the
    // table is on screen with rows. After that, widths are sticky (drag to change).
    // The caller invokes this only when the list view is actually visible.
    ensureWidths() {
      const cols = [...this.colgroup.querySelectorAll("col")];
      if (!cols.length || !this.tbody.children.length) return;
      if (cols.every((c) => this.widths[c.dataset.col])) return;
      cols.forEach((c) => (c.style.width = ""));
      const prev = this.table.style.tableLayout;
      this.table.style.tableLayout = "auto";
      void this.table.offsetWidth; // force reflow
      this.headRow.querySelectorAll("th").forEach((th, i) => {
        const key = cols[i]?.dataset.col;
        if (key && !this.widths[key]) {
          this.widths[key] = Math.ceil(th.getBoundingClientRect().width) + 8;
        }
      });
      this.table.style.tableLayout = prev || "fixed";
      this._applyWidths();
      this._saveWidths();
    }
  }

  window.TableView = TableView;
})();
