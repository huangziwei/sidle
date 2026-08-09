// Reading Log: what was read on which day, and for how long.
//
// Classic script loaded AFTER library.js. Self-contained IIFE exposing
// `window.ReadingLog` ({ refresh, show, hide, invalidate }); library.js's
// section toggle drives show/hide. Backend: commands/reading_log.rs —
// reading_log_overview / _day / _book / _import.
//
// The data behind this comes from the Kindle's own system logs, which name no
// book: every session is identified by the book's end position and matched
// against the library. Two consequences show up in the UI and are deliberate —
// time on a book the library no longer holds is counted nowhere (the backend
// never sends it), and nothing appears at all until the user imports an archive,
// because those logs live wherever they were copied to.
(function () {
  const api = window.api;
  const q = (sel) => document.querySelector(sel);

  const state = {
    overview: null,
    loaded: false,
    year: null, // heatmap year; null = trailing 12 months
    day: null, // selected YYYY-MM-DD, or null
    book: null, // { id, days, entry } when the book page is open
    month: null, // Date anchoring the book page's calendar
  };

  function toast(msg, isError = false) {
    if (typeof window.showToast === "function") window.showToast(msg, isError);
    else if (isError) console.error(msg);
  }

  // ── Formatting ─────────────────────────────────────────────────────────────

  // Durations read as "4h 12m" / "37m" / "2m". Seconds are never shown: the
  // underlying counter is not that precise, and a reading log measured to the
  // second would claim an accuracy it does not have.
  function fmtDuration(secs) {
    if (!secs || secs < 60) return "<1m";
    const h = Math.floor(secs / 3600);
    const m = Math.round((secs % 3600) / 60);
    if (h && m) return `${h}h ${m}m`;
    if (h) return `${h}h`;
    return `${m}m`;
  }

  // Word counts come straight off the device's own `TotalWords` counter, which
  // is why they can be shown at all — and why they, not any page figure, are the
  // measure of how much of a book was read.
  function fmtWords(n) {
    if (!n) return "0";
    if (n >= 1000000) return `${(n / 1000000).toFixed(1)}M`;
    if (n >= 1000) return `${Math.round(n / 1000)}k`;
    return String(n);
  }

  function fmtDay(iso) {
    const d = parseDay(iso);
    if (!d) return iso;
    return d.toLocaleDateString(undefined, {
      weekday: "long",
      year: "numeric",
      month: "long",
      day: "numeric",
    });
  }

  // `YYYY-MM-DD` → local Date. Built component-wise, never `new Date(iso)`,
  // which parses a bare date as UTC and lands on the previous day west of
  // Greenwich — every square would sit one day off.
  function parseDay(iso) {
    const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(iso || "");
    return m ? new Date(+m[1], +m[2] - 1, +m[3]) : null;
  }

  function dayKey(d) {
    const p = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
  }

  // Rows here are built as HTML strings, so anything from the library has to be
  // escaped: a title is whatever the book's metadata said.
  function esc(s) {
    return String(s == null ? "" : s).replace(
      /[&<>"']/g,
      (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
    );
  }

  // ── Public surface ─────────────────────────────────────────────────────────

  async function refresh() {
    try {
      state.overview = await api.invoke("reading_log_overview");
    } catch (e) {
      toast(`failed to load reading log: ${e}`, true);
      state.overview = null;
    }
    state.loaded = true;
    render();
  }

  function show() {
    if (!state.loaded) refresh();
    else render();
  }

  function hide() {}

  function invalidate() {
    state.loaded = false;
  }

  // ── Overview ───────────────────────────────────────────────────────────────

  function render() {
    const o = state.overview;
    const has = !!o && o.days.length > 0;
    q("#rl-empty").hidden = has;
    q("#rl-body").hidden = !has;
    q("#rl-overview").hidden = !!state.book;
    q("#rl-book").hidden = !state.book;
    if (state.book) return renderBook();
    if (!has) {
      q("#rl-stats").innerHTML = "";
      return;
    }
    renderStats(o);
    renderYearPicker(o);
    renderHeatmap(o);
    renderBookList(o);
    renderDay();
  }

  function statTile(value, label, hint) {
    const t = hint ? ` title="${hint}"` : "";
    return `<div class="rl-stat"${t}><b>${value}</b><span>${label}</span></div>`;
  }

  function renderStats(o) {
    const perDay = o.days_read ? Math.round(o.total_seconds / o.days_read) : 0;
    const tiles = [
      statTile(fmtDuration(o.total_seconds), "total"),
      statTile(o.days_read, "days read"),
      statTile(fmtDuration(perDay), "per reading day"),
      statTile(`${o.current_streak}d`, "streak", "Consecutive days up to today"),
      statTile(`${o.longest_streak}d`, "longest streak"),
      statTile(o.books.length, "books"),
    ];
    q("#rl-stats").innerHTML = tiles.join("");
  }

  function renderYearPicker(o) {
    const years = [...new Set(o.days.map((d) => d.day.slice(0, 4)))].sort().reverse();
    const sel = q("#rl-year");
    const opts = [`<option value="">Last 12 months</option>`].concat(
      years.map((y) => `<option value="${y}">${y}</option>`),
    );
    sel.innerHTML = opts.join("");
    sel.value = state.year || "";
  }

  // GitHub-style: one column per week, Sunday at the top. The grid is built
  // from a fixed start so every column is a real week — padding the leading
  // days keeps the weekday rows aligned instead of shearing by one.
  function renderHeatmap(o) {
    const totals = new Map(o.days.map((d) => [d.day, d.seconds]));
    let start;
    let end;
    if (state.year) {
      start = new Date(+state.year, 0, 1);
      end = new Date(+state.year, 11, 31);
    } else {
      end = new Date();
      start = new Date(end.getFullYear(), end.getMonth(), end.getDate() - 364);
    }
    start = new Date(start.getFullYear(), start.getMonth(), start.getDate() - start.getDay());

    // Scale by the busiest day rather than fixed thresholds, so the range is
    // meaningful whether the reader does 20 minutes or 4 hours a day.
    const peak = Math.max(...o.days.map((d) => d.seconds), 1);
    const level = (secs) => {
      if (!secs) return 0;
      const r = secs / peak;
      return r > 0.66 ? 4 : r > 0.4 ? 3 : r > 0.15 ? 2 : 1;
    };

    const cols = [];
    let months = [];
    for (let d = new Date(start); d <= end; d.setDate(d.getDate() + 7)) {
      const week = [];
      for (let i = 0; i < 7; i++) {
        const cur = new Date(d.getFullYear(), d.getMonth(), d.getDate() + i);
        if (cur > end) {
          week.push(`<i class="rl-cell rl-pad"></i>`);
          continue;
        }
        const key = dayKey(cur);
        const secs = totals.get(key) || 0;
        const sel = key === state.day ? " rl-sel" : "";
        week.push(
          `<i class="rl-cell rl-l${level(secs)}${sel}" data-day="${key}" role="button" tabindex="0" ` +
            `title="${fmtDay(key)} — ${secs ? fmtDuration(secs) : "nothing"}"></i>`,
        );
      }
      // A month label sits above the first week that starts inside it.
      const first = new Date(d.getFullYear(), d.getMonth(), d.getDate());
      months.push(
        first.getDate() <= 7
          ? first.toLocaleDateString(undefined, { month: "short" })
          : "",
      );
      cols.push(`<div class="rl-week">${week.join("")}</div>`);
    }
    q("#rl-heatmap").innerHTML =
      `<div class="rl-months">${months.map((m) => `<span>${m}</span>`).join("")}</div>` +
      `<div class="rl-grid">` +
      `<div class="rl-dows"><span></span><span>Mon</span><span></span><span>Wed</span>` +
      `<span></span><span>Fri</span><span></span></div>` +
      `<div class="rl-weeks">${cols.join("")}</div></div>`;
  }

  // The cover markup the gallery uses, so a book looks the same wherever it
  // appears — and `coverUrlFor` (library.js) stays the one place that knows the
  // thumb-vs-full choice and the cache-busting token. Split in two because the
  // book page owns its own frame element and only fills it.
  function coverInner(url, title) {
    return url
      ? `<img src="${esc(url)}" alt="" loading="lazy" draggable="false">`
      : `<div class="cover-placeholder">${esc(title)}</div>`;
  }

  function coverHtml(e) {
    const url = coverUrlFor(e, { thumb: true });
    return `<div class="cover${url ? " has-image" : ""}">${coverInner(url, e.title)}</div>`;
  }

  // Every card is a book in the library, so every card opens its book page.
  function entryCard(e, sub) {
    return (
      `<div class="book-card rl-card" data-book="${e.book_id}" role="button" tabindex="0" ` +
      `title="${esc(e.title)}${e.author ? `\n${esc(e.author)}` : ""}">` +
      coverHtml(e) +
      `<div class="meta">` +
      `<div class="t">${esc(e.title)}</div>` +
      `<div class="a">${esc(e.author || "Unknown author")}</div>` +
      `<div class="rl-card-time">${fmtDuration(e.seconds)}</div>` +
      `<div class="rl-card-sub">${esc(sub || "")}</div>` +
      `</div></div>`
    );
  }

  function renderBookList(o) {
    q("#rl-book-list").innerHTML = o.books
      .map((b) => entryCard(b, `${b.sessions} sessions · ${fmtWords(b.words)} words`))
      .join("");
  }

  async function renderDay() {
    const panel = q("#rl-day");
    if (!state.day) {
      panel.hidden = true;
      return;
    }
    panel.hidden = false;
    q("#rl-day-title").textContent = fmtDay(state.day);
    let rows = [];
    try {
      rows = await api.invoke("reading_log_day", { day: state.day });
    } catch (e) {
      toast(`failed to load ${state.day}: ${e}`, true);
    }
    const total = rows.reduce((a, r) => a + r.seconds, 0);
    q("#rl-day-total").textContent = rows.length ? fmtDuration(total) : "nothing read";
    q("#rl-day-list").innerHTML = rows
      .map((r) => entryCard(r, `from ${r.first_at.slice(11, 16)}`))
      .join("");
  }

  // ── One book ───────────────────────────────────────────────────────────────

  async function openBook(bookId) {
    try {
      const data = await api.invoke("reading_log_book", { bookId });
      state.book = { id: bookId, ...data };
      const last = data.days.length ? parseDay(data.days[data.days.length - 1].day) : new Date();
      state.month = new Date(last.getFullYear(), last.getMonth(), 1);
      render();
    } catch (e) {
      toast(`failed to load book: ${e}`, true);
    }
  }

  function renderBook() {
    const { entry, days } = state.book;
    q("#rl-book-title").textContent = entry ? entry.title : "Book";
    q("#rl-book-author").textContent = entry?.author || "";
    const box = q("#rl-book-cover");
    const cover = entry ? coverUrlFor(entry, { thumb: true }) : null;
    box.className = `cover rl-book-cover${cover ? " has-image" : ""}`;
    box.innerHTML = entry ? coverInner(cover, entry.title) : "";

    if (entry) {
      const span = spanDays(days);
      const perDay = days.length ? Math.round(entry.seconds / days.length) : 0;
      // Pace comes from the device's own word counts, which is why it can be
      // shown at all — nothing here is inferred from page geometry.
      const wpm = entry.seconds > 0 ? Math.round((entry.words * 60) / entry.seconds) : 0;
      q("#rl-book-stats").innerHTML = [
        statTile(fmtDuration(entry.seconds), "total"),
        statTile(days.length, "days"),
        statTile(fmtDuration(perDay), "per day"),
        statTile(fmtWords(entry.words), "words"),
        statTile(entry.sessions, "sessions"),
        wpm ? statTile(wpm, "words/min") : "",
        // Deliberately not called "pages": a converted book has no pagination,
        // and this counts forward taps at whatever font size the device was on.
        statTile(
          entry.page_turns,
          "page turns",
          "Forward page turns on the device — depends on font size, not a page count",
        ),
        statTile(span, "days elapsed", "First to last day read"),
      ].join("");
    } else {
      q("#rl-book-stats").innerHTML = "";
    }
    renderMonth();
  }

  function spanDays(days) {
    if (days.length < 2) return days.length;
    const a = parseDay(days[0].day);
    const b = parseDay(days[days.length - 1].day);
    return Math.round((b - a) / 86400000) + 1;
  }

  function renderMonth() {
    const totals = new Map(state.book.days.map((d) => [d.day, d.seconds]));
    const anchor = state.month;
    q("#rl-month-label").textContent = anchor.toLocaleDateString(undefined, {
      month: "long",
      year: "numeric",
    });
    const first = new Date(anchor.getFullYear(), anchor.getMonth(), 1);
    const lead = first.getDay();
    const len = new Date(anchor.getFullYear(), anchor.getMonth() + 1, 0).getDate();
    const peak = Math.max(...state.book.days.map((d) => d.seconds), 1);

    const cells = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"].map(
      (d) => `<span class="rl-mday-head">${d}</span>`,
    );
    for (let i = 0; i < lead; i++) cells.push(`<span class="rl-mday rl-mpad"></span>`);
    for (let day = 1; day <= len; day++) {
      const key = dayKey(new Date(anchor.getFullYear(), anchor.getMonth(), day));
      const secs = totals.get(key) || 0;
      const r = secs / peak;
      const lvl = !secs ? 0 : r > 0.66 ? 4 : r > 0.4 ? 3 : r > 0.15 ? 2 : 1;
      cells.push(
        `<span class="rl-mday rl-l${lvl}" title="${secs ? fmtDuration(secs) : "nothing"}">` +
          `<b>${day}</b>${secs ? `<em>${fmtDuration(secs)}</em>` : ""}</span>`,
      );
    }
    q("#rl-month-grid").innerHTML = cells.join("");
  }

  // ── Import ─────────────────────────────────────────────────────────────────

  // Named steps, so the label says what is happening rather than just ticking.
  // "index" is the long one and the one that needs explaining: it is not
  // reading the logs at all, it is measuring the library so the logs can be
  // matched against it.
  const PHASE_LABEL = {
    index: "Indexing library",
    read: "Reading logs",
    store: "Saving sessions",
  };

  function showProgress(on) {
    q("#rl-progress").hidden = !on;
    q("#rl-import").disabled = on;
    if (!on) {
      q("#rl-progress-bar").value = 0;
      q("#rl-progress-label").textContent = "";
    }
  }

  function onProgress(p) {
    q("#rl-progress-bar").value = p.fraction || 0;
    const step = PHASE_LABEL[p.phase] || p.phase;
    const count = p.total ? ` ${p.done + 1} / ${p.total}` : "";
    q("#rl-progress-label").textContent = `${step}${count} — ${p.label}`;
  }

  async function doImport() {
    let paths;
    try {
      paths = await api.invoke("reading_log_pick_folders");
    } catch (e) {
      toast(`could not open the folder picker: ${e}`, true);
      return;
    }
    if (!paths || !paths.length) return;

    showProgress(true);
    try {
      const r = await api.invoke("reading_log_import", { paths, deviceSerial: null });
      if (r.cancelled) {
        // Both phases commit as they go, so a cancel keeps its work — say so,
        // or the user re-runs from scratch expecting to have lost it.
        toast("import stopped — what finished was kept, run it again to continue");
      } else if (!r.events) {
        toast("no reading events in those files — is this a logbackup folder?", true);
      } else if (!r.added) {
        toast(`already imported: ${r.sessions} sessions in ${r.files} files`);
      } else if (!r.attributed) {
        // Everything found is on books the library doesn't hold, so nothing was
        // counted — say so, or a successful import looks like a broken page.
        toast(`${r.added} sessions found, none on books in the library`, true);
      } else {
        // `attributed`, not `added`: time on a missing book is stored inert and
        // appears nowhere, so counting it here would promise rows that never show.
        const skipped = Math.max(0, r.added - r.attributed);
        const tail = skipped ? ` · ${skipped} on books not in the library` : "";
        toast(`${r.attributed} sessions added from ${r.files} files${tail}`);
      }
      invalidate();
      await refresh();
    } catch (e) {
      toast(`import failed: ${e}`, true);
    } finally {
      showProgress(false);
    }
  }

  // ── Wiring ─────────────────────────────────────────────────────────────────

  function init() {
    q("#rl-import").addEventListener("click", doImport);
    q("#rl-cancel").addEventListener("click", async () => {
      const btn = q("#rl-cancel");
      btn.disabled = true;
      btn.textContent = "Stopping…";
      try {
        await api.invoke("reading_log_cancel");
      } catch (e) {
        toast(`could not cancel: ${e}`, true);
      }
      // The import's own `finally` hides the panel; restore the button for the
      // next run either way.
      btn.disabled = false;
      btn.textContent = "Cancel";
    });
    api.listen("reading-log:import-progress", (e) => onProgress(e.payload));
    q("#rl-year").addEventListener("change", (e) => {
      state.year = e.target.value || null;
      state.day = null;
      render();
    });
    q("#rl-day-close").addEventListener("click", () => {
      state.day = null;
      render();
    });
    q("#rl-back").addEventListener("click", () => {
      state.book = null;
      render();
    });
    q("#rl-prev").addEventListener("click", () => {
      state.month = new Date(state.month.getFullYear(), state.month.getMonth() - 1, 1);
      renderMonth();
    });
    q("#rl-next").addEventListener("click", () => {
      state.month = new Date(state.month.getFullYear(), state.month.getMonth() + 1, 1);
      renderMonth();
    });

    // One delegated handler for the whole page: the heatmap and both lists are
    // re-rendered wholesale, so per-element listeners would leak on every draw.
    q("#reading-log").addEventListener("click", (e) => {
      const cell = e.target.closest(".rl-cell[data-day]");
      if (cell) {
        state.day = state.day === cell.dataset.day ? null : cell.dataset.day;
        render();
        return;
      }
      const row = e.target.closest(".rl-card[data-book]");
      if (row) openBook(Number(row.dataset.book));
    });
    q("#reading-log").addEventListener("keydown", (e) => {
      if (e.key !== "Enter" && e.key !== " ") return;
      const hit = e.target.closest(".rl-cell[data-day], .rl-card[data-book]");
      if (!hit) return;
      e.preventDefault();
      hit.click();
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }

  window.ReadingLog = { refresh, show, hide, invalidate };
})();
