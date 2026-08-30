// Menus and action bars, rendered from a declarative list of actions — the ONE
// implementation of "offer these actions on this target", used by every surface
// that offers any.

(function () {
  const MENU_ID = "ctx-menu";
  // A submenu opens at `left: 100%`; below this much room on the right it is
  // flipped to the other side instead of off-screen. Matches `.ctx-submenu`.
  const SUBMENU_WIDTH = 170;
  // How many actions a bar puts in front of its overflow menu. A bar is one
  const BAR_FIXED_WIDTH = 430; // the count, More…, a danger action, Clear, gaps
  const BAR_SLOT_WIDTH = 145; // one action button, counting a "(11)" in its label
  const BAR_MAX_SLOTS = 4; // past this a bar is just a menu laid on its side

  function menuEl() {
    return document.getElementById(MENU_ID);
  }

  // The context an action sees: the caller's, plus the items this action in
  // particular can run on, plus which surface is asking.
  function resolve(action, ctx, surface) {
    const eligible = action.eligible ? ctx.items.filter(action.eligible) : ctx.items;
    return { ...ctx, eligible, surface };
  }

  // Whether `action` is offered for `ctx` at all. Every surface asks exactly
  // this, so none can offer something another hides.
  function applies(action, ctx, surface) {
    if (!action.scopes.includes(ctx.kind)) return false;
    const c = resolve(action, ctx, surface);
    if (action.when && !action.when(c)) return false;
    // An action with nothing to run on is not disabled, it is absent: a menu
    return c.eligible.length > 0 || Boolean(action.submenu);
  }

  // Insert the count into a label once a target holds more than one item, before
  // any trailing ellipsis: "Edit metadata…" → "Edit metadata (11)…". The count
  // is of ELIGIBLE items, so a partial selection says how many it will touch.
  function counted(base, ctx) {
    if (ctx.items.length < 2) return base;
    const n = ctx.eligible.length;
    return base.endsWith("…") ? `${base.slice(0, -1)} (${n})…` : `${base} (${n})`;
  }

  // The bar button the open menu belongs to, so clicking it again closes the
  // menu instead of re-opening it in place. Null whenever no menu is open.
  let openedBy = null;

  function close() {
    const menu = menuEl();
    if (menu) menu.hidden = true;
    openedBy = null;
  }

  // ── Menu ────────────────────────────────────────────────────────────────────

  function separator() {
    const li = document.createElement("li");
    li.className = "ctx-sep";
    li.setAttribute("role", "separator");
    return li;
  }

  function leaf(action, ctx) {
    const li = document.createElement("li");
    li.textContent = action.label(ctx);
    if (action.danger) li.classList.add("danger");
    li.addEventListener("click", (e) => {
      e.stopPropagation();
      close();
      action.run(ctx);
    });
    return li;
  }

  function nested(action, ctx) {
    const li = document.createElement("li");
    li.textContent = action.label(ctx);
    const items = action.submenu(ctx);
    if (!items.length) {
      li.classList.add("disabled");
      // Inert: swallow the click rather than let it reach the document's
      // close-the-menu handler, which would read as having chosen something.
      li.addEventListener("click", (e) => e.stopPropagation());
      return li;
    }
    li.classList.add("has-sub");
    const sub = document.createElement("ul");
    sub.className = "ctx-submenu";
    for (const [label, fn] of items) sub.appendChild(choice(label, fn));
    li.appendChild(sub);
    return li;
  }

  function choice(label, fn) {
    const li = document.createElement("li");
    li.textContent = label;
    li.addEventListener("click", (e) => {
      e.stopPropagation();
      close();
      fn();
    });
    return li;
  }

  // Fill #ctx-menu with every action that applies, separated at each group
  // boundary, and show it. `at` is where to put it — see `place`.
  function openMenu(actions, ctx, at) {
    const menu = menuEl();
    if (!menu) return;
    menu.innerHTML = "";
    let group = null;
    for (const action of actions) {
      if (!applies(action, ctx, "menu")) continue;
      if (group !== null && action.group !== group) menu.appendChild(separator());
      group = action.group;
      const c = resolve(action, ctx, "menu");
      menu.appendChild(action.submenu ? nested(action, c) : leaf(action, c));
    }
    place(menu, at);
  }

  // Fill #ctx-menu with a flat list of `[label, fn]` choices — a submenu shown
  // on its own, for a surface with no menu to nest it in.
  function openChoices(items, at) {
    const menu = menuEl();
    if (!menu) return;
    menu.innerHTML = "";
    for (const [label, fn] of items) menu.appendChild(choice(label, fn));
    place(menu, at);
  }

  // Show the menu at `at`, then nudge it back on-screen.
  function place(menu, at) {
    menu.hidden = false;
    menu.classList.remove("flip-sub"); // default: submenus open to the right
    menu.style.left = `${at.x}px`;
    menu.style.top = `${at.above ?? at.y}px`;
    requestAnimationFrame(() => {
      const r = menu.getBoundingClientRect();
      if (r.right > window.innerWidth) {
        menu.style.left = `${Math.max(4, window.innerWidth - r.width - 4)}px`;
      }
      // Rising above the anchor already puts the menu on-screen, so the two are
      // alternatives — running the overflow clamp as well would undo the rise.
      if (at.above != null) {
        menu.style.top = `${Math.max(4, at.above - r.height - 4)}px`;
      } else if (r.bottom > window.innerHeight) {
        menu.style.top = `${Math.max(4, window.innerHeight - r.height - 4)}px`;
      }
      // A submenu opens to the right (`left: 100%`). When the menu ends up at
      const moved = menu.getBoundingClientRect();
      menu.classList.toggle("flip-sub", moved.right + SUBMENU_WIDTH > window.innerWidth);
    });
  }

  // ── Action bar ──────────────────────────────────────────────────────────────

  // A bar's menu rises from the button that opened it.
  function anchorOf(el) {
    const r = el.getBoundingClientRect();
    return { x: r.left, above: r.top };
  }

  function barSlots() {
    const room = Math.floor((window.innerWidth - BAR_FIXED_WIDTH) / BAR_SLOT_WIDTH);
    return Math.max(1, Math.min(BAR_MAX_SLOTS, room));
  }

  function barButton(label, onClick) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "btn-link";
    b.textContent = label;
    b.addEventListener("click", (e) => {
      // The document-level "a click closes the menu" handler would otherwise
      // fire on this same click and re-hide a menu we just opened.
      e.stopPropagation();
      onClick(e.currentTarget);
    });
    return b;
  }

  // Render `actions` into `host` as a bar of buttons: those marked `bar` that
  function menuButton(label, fill) {
    return barButton(label, (el) => {
      const menu = menuEl();
      if (menu && !menu.hidden && openedBy === el) {
        close();
        return;
      }
      openedBy = el;
      fill(el);
    });
  }

  function renderBar(host, actions, ctx) {
    host.innerHTML = "";
    const shown = actions.filter((a) => a.bar && applies(a, ctx, "bar"));
    const button = (action) => {
      const c = resolve(action, ctx, "bar");
      const btn = action.submenu
        ? menuButton(action.label(c), (el) => openChoices(action.submenu(c), anchorOf(el)))
        : barButton(action.label(c), () => {
            close();
            action.run(c);
          });
      if (action.danger) btn.classList.add("sel-danger");
      host.appendChild(btn);
    };
    for (const action of shown.filter((a) => !a.danger).slice(0, barSlots())) button(action);
    host.appendChild(menuButton("More…", (el) => openMenu(actions, ctx, anchorOf(el))));
    // A destructive action keeps its own slot at the end, apart from the rest.
    for (const action of shown.filter((a) => a.danger)) button(action);
  }

  window.ActionMenu = {
    // (x, y) is where a right-click happened.
    open: (x, y, actions, ctx) => openMenu(actions, ctx, { x, y }),
    renderBar,
    counted,
    close,
    // Position and reveal #ctx-menu after filling it by hand — for a menu whose
    placeAt: (x, y) => {
      const menu = menuEl();
      if (menu) place(menu, { x, y });
    },
  };
})();
