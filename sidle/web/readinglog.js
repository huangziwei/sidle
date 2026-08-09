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
// time can be unattributed (the book was deleted or re-converted since), and
// nothing appears at all until the user imports an archive, because those logs
// live wherever they were copied to.
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

  function titleOf(entry) {
    if (entry.title) return entry.title;
    // An unattributed row still deserves a stable identity in the list; the
    // fingerprint is what would name it once its book is imported.
    return `Unidentified book (position ${entry.end_position})`;
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
      statTile(o.books.filter((b) => b.book_id !== null).length, "books"),
    ];
    if (o.unattributed_seconds > 0) {
      tiles.push(
        statTile(
          fmtDuration(o.unattributed_seconds),
          "unidentified",
          "Time on books no longer in the library — import them and this is named retroactively",
        ),
      );
    }
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

  function entryRow(e, extra) {
    const named = e.book_id !== null;
    const cls = named ? "rl-row rl-row-book" : "rl-row rl-row-unknown";
    const attr = named ? ` data-book="${e.book_id}" role="button" tabindex="0"` : "";
    const author = e.author ? `<span class="rl-muted">${e.author}</span>` : "";
    return (
      `<li class="${cls}"${attr}>` +
      `<span class="rl-row-title">${titleOf(e)} ${author}</span>` +
      `<span class="rl-row-meta">${extra || ""}</span>` +
      `<span class="rl-row-time">${fmtDuration(e.seconds)}</span></li>`
    );
  }

  function renderBookList(o) {
    q("#rl-book-list").innerHTML = o.books
      .map((b) => entryRow(b, `${b.pages} pages · ${b.sessions} sessions`))
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
      .map((r) => entryRow(r, `${r.pages} pages · ${r.first_at.slice(11, 16)}`))
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
    q("#rl-book-title").textContent = entry ? titleOf(entry) : "Book";
    q("#rl-book-author").textContent = entry?.author || "";

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
        statTile(entry.pages, "pages"),
        statTile(entry.sessions, "sessions"),
        wpm ? statTile(wpm, "words/min") : "",
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

  async function doImport() {
    const btn = q("#rl-import");
    let paths;
    try {
      paths = await api.invoke("reading_log_pick_folders");
    } catch (e) {
      toast(`could not open the folder picker: ${e}`, true);
      return;
    }
    if (!paths || !paths.length) return;

    btn.disabled = true;
    btn.textContent = "Importing…";
    try {
      const r = await api.invoke("reading_log_import", { paths, deviceSerial: null });
      if (!r.events) {
        toast("no reading events in those files — is this a logbackup folder?", true);
      } else if (!r.added) {
        toast(`already imported: ${r.sessions} sessions in ${r.files} files`);
      } else {
        toast(`${r.added} new sessions from ${r.files} files`);
      }
      invalidate();
      await refresh();
    } catch (e) {
      toast(`import failed: ${e}`, true);
    } finally {
      btn.disabled = false;
      btn.textContent = "Import…";
    }
  }

  // ── Wiring ─────────────────────────────────────────────────────────────────

  function init() {
    q("#rl-import").addEventListener("click", doImport);
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
      const row = e.target.closest(".rl-row-book[data-book]");
      if (row) openBook(Number(row.dataset.book));
    });
    q("#reading-log").addEventListener("keydown", (e) => {
      if (e.key !== "Enter" && e.key !== " ") return;
      const hit = e.target.closest(".rl-cell[data-day], .rl-row-book[data-book]");
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
