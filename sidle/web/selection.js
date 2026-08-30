// Shared multi-select controller — the ONE implementation of click / cmd-click /
// shift-range / lasso / select-all / clear, used by BOTH the Books and Notes
// sections so selection behaves identically everywhere.
(function () {
  const LASSO_THRESHOLD = 4; // px a mousedown must travel before it's a drag

  function positionLasso(el, x1, y1, x2, y2) {
    el.style.left = `${Math.min(x1, x2)}px`;
    el.style.top = `${Math.min(y1, y2)}px`;
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

  function rectsIntersect(a, b) {
    return !(a.right < b.left || a.left > b.right || a.bottom < b.top || a.top > b.bottom);
  }

  class SelectionController {
    constructor(cfg) {
      this.cfg = cfg;
      this.selected = new Set();
      this.lastClicked = null; // anchor id for shift-range
    }

    has(id) {
      return this.selected.has(id);
    }
    count() {
      return this.selected.size;
    }
    ids() {
      return [...this.selected];
    }

    // Click on an item. Plain = single-select; cmd/ctrl = toggle; shift = range.
    click(e, id) {
      e.stopPropagation();
      if (e.shiftKey && this.lastClicked != null) {
        this.rangeTo(id);
      } else if (e.metaKey || e.ctrlKey) {
        this.toggle(id);
        this.lastClicked = id;
      } else {
        this.selected = new Set([id]);
        this.lastClicked = id;
      }
      this.applyVisuals();
    }

    // Right-click: keep an existing multi-selection (so the menu can act on it);
    // otherwise reset to just the clicked item.
    context(id) {
      if (this.selected.has(id)) return;
      this.selected = new Set([id]);
      this.lastClicked = id;
      this.applyVisuals();
    }

    rangeTo(toId) {
      const ordered = this.cfg.orderedIds();
      const from = ordered.indexOf(this.lastClicked);
      const to = ordered.indexOf(toId);
      if (from === -1 || to === -1) {
        this.selected.add(toId);
        return;
      }
      const [lo, hi] = from < to ? [from, to] : [to, from];
      for (let i = lo; i <= hi; i++) this.selected.add(ordered[i]);
    }

    toggle(id) {
      if (this.selected.has(id)) this.selected.delete(id);
      else this.selected.add(id);
    }

    selectAll() {
      this.selected = new Set(this.cfg.orderedIds());
      this.lastClicked = null;
      this.applyVisuals();
    }

    // Replace the selection with the inclusive range anchorId..toId over the
    selectRangeFromAnchor(anchorId, toId) {
      this.lastClicked = anchorId;
      this.selected = new Set();
      this.rangeTo(toId); // ranges anchorId..toId, adding into the cleared set
      this.applyVisuals();
    }

    clear() {
      if (this.selected.size === 0) {
        this.cfg.onChange(this);
        return;
      }
      this.selected.clear();
      this.lastClicked = null;
      this.applyVisuals();
    }

    // Drop ids no longer present (after a list refresh). Caller re-renders.
    prune(liveIds) {
      const live = liveIds instanceof Set ? liveIds : new Set(liveIds);
      for (const id of [...this.selected]) if (!live.has(id)) this.selected.delete(id);
      if (this.lastClicked != null && !live.has(this.lastClicked)) this.lastClicked = null;
    }

    // Cheap repaint: toggle `.selected` across BOTH views + refresh the bar.
    // Never rebuilds the DOM (so notebook tile thumbnails aren't re-fetched).
    applyVisuals() {
      const attr = this.cfg.idAttr;
      const els = (this.cfg.paintContainers || this.cfg.containers)();
      els.forEach((el) => {
        el.classList.toggle("selected", this.selected.has(Number(el.dataset[attr])));
      });
      this.cfg.onChange(this);
    }

    // Begin a rubber-band drag from a mousedown on empty area. A drag below the
    // threshold is treated as a plain click → clear (unless additive).
    beginLasso(e) {
      if (e.button !== 0) return;
      const startX = e.clientX;
      const startY = e.clientY;
      const additive = e.metaKey || e.ctrlKey || e.shiftKey;
      const base = additive ? new Set(this.selected) : new Set();
      const lasso = this.cfg.lassoEl();
      let active = false;

      const onMove = (ev) => {
        if (!active) {
          if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < LASSO_THRESHOLD) return;
          active = true;
          if (lasso) lasso.hidden = false;
        }
        if (lasso) positionLasso(lasso, startX, startY, ev.clientX, ev.clientY);
        const rect = makeRect(startX, startY, ev.clientX, ev.clientY);
        this.selected = new Set([...base, ...this.hitTest(rect)]);
        this.applyVisuals();
      };

      const onUp = () => {
        document.removeEventListener("mousemove", onMove);
        document.removeEventListener("mouseup", onUp);
        if (active) {
          if (lasso) lasso.hidden = true;
          this.lastClicked = null;
          this.applyVisuals();
        } else if (!additive) {
          // No drag — empty-area click: clear.
          this.clear();
        }
      };

      document.addEventListener("mousemove", onMove);
      document.addEventListener("mouseup", onUp);
      e.preventDefault();
    }

    hitTest(rect) {
      const attr = this.cfg.idAttr;
      const hits = new Set();
      this.cfg.containers().forEach((el) => {
        if (rectsIntersect(rect, el.getBoundingClientRect())) {
          const id = Number(el.dataset[attr]);
          if (id) hits.add(id);
        }
      });
      return hits;
    }
  }

  window.SelectionController = SelectionController;
})();
