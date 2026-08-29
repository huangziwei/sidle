// Reading Log: what was read on which day, and for how long.
//
// Classic script loaded AFTER library.js. Self-contained IIFE exposing
// `window.ReadingLog` ({ refresh, show, hide, invalidate }); library.js's
// section toggle drives show/hide. Backend: commands/reading_log.rs —
// reading_log_overview / _books / _book / _import / _clear / _cancel /
// _pick_folders, plus commands/reader.rs's `annotations_for_book` for the
// highlights and notes on a book's page.
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
    year: null, // heatmap year, as a number; resolved on first render
    calView: "year", // which calendar is drawn: year | month
    calMonth: null, // Date anchoring the month calendar
    calRows: new Map(), // "YYYY-MM" → that month's per-day book rows
    calPending: 0, // guards the month fetch against a stale reply
    calShapes: new Map(), // "YYYY-MM" → that month's per-day hour shapes
    tlPending: 0, // guards the timeline fetch against a stale reply
    day: null, // selected YYYY-MM-DD within that year, or null
    books: [], // the grid: books of the selected day, else of the year
    bucket: "year", // how finely the grid cuts the year up: year | month | day
    clockView: "hour", // which cut of the clock cube is drawn: hour | week | month
    sort: { key: "last", asc: false }, // most recently read first
    scope: 0, // guards the async grid fetch against a stale reply
    book: null, // { id, days, entry } when the book page is open
    notes: [], // that book's annotations, as `annotations_for_book` returns them
    notesFailed: false, // that query failed, so an empty list means "unknown"
    ambiguous: [], // reading several books fit equally, one entry per position
    month: null, // Date anchoring the book page's calendar
    overviewScroll: 0, // where the overview was left when a book was opened
  };

  // Every section shares one scroll container, and swapping the overview for the
  // book page does not move it — so a book opened from halfway down the list
  // opens halfway down itself. The drill-in parks the overview's offset and
  // starts the book at its top; going back puts the overview back where it was.
  const scroller = () => q("#main");

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

  // A "~" on a figure the Kindle's own reading timer did not produce.
  //
  // That timer runs on words and reading speed, so a book it can count no words
  // in — manga, a fixed-layout magazine — is never timed: the device's own book
  // info reads zero however long it was read. Two things stand in, and they are
  // not equally good. `dwell_seconds` is the reader's page records, timed page
  // by page — a measurement. `awake_seconds` is how long the device was awake
  // with the book open — a bound. The mark names whichever carries more of the
  // figure, and an entry with any of either takes it.
  function estimateMark(e) {
    const dwell = e.dwell_seconds || 0;
    const awake = e.awake_seconds || 0;
    if (!dwell && !awake) return "";
    const part = dwell + awake;
    const all = part >= e.seconds;
    const how =
      awake > dwell
        ? "measured as time awake with the book open"
        : "timed page by page from the reader's own page records";
    const title = all
      ? `Not counted by the Kindle — this book has no word count, so its reading ` +
        `timer never ran. Instead ${how}.`
      : `${fmtDuration(part)} of this was not counted by the Kindle — ${how}.`;
    return `<span class="rl-estimate" title="${esc(title)}">~</span>`;
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

  // "Aug 9" — enough to place a book in the year at a glance. Takes a full
  // timestamp or a bare day; only the date part is ever used.
  function shortDay(iso) {
    const d = parseDay((iso || "").slice(0, 10));
    return d ? d.toLocaleDateString(undefined, { month: "short", day: "numeric" }) : "";
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

  // ── A book's own colour ────────────────────────────────────────────────────

  // Degrees of hue between one book id and the next: the golden angle.
  const GOLDEN_ANGLE_DEG = 137.508;

  // `book_id` → a hue in [0, 360).
  function bookHue(id) {
    return ((Number(id) || 0) * GOLDEN_ANGLE_DEG) % 360;
  }

  // `--rl-span-*` set the lightness and chroma; `bookHue` sets the hue.
  function bookFill(id) {
    return `oklch(var(--rl-span-l) var(--rl-span-c) ${bookHue(id).toFixed(1)})`;
  }

  function bookInk(id) {
    return `oklch(var(--rl-span-ink-l) var(--rl-span-ink-c) ${bookHue(id).toFixed(1)})`;
  }

  // ── Public surface ─────────────────────────────────────────────────────────

  async function refresh() {
    // `doImport`, `doPurge` and `nameBook` all reach `refresh`; `calRows` is
    // dropped for every one of them.
    state.calRows.clear();
    state.calShapes.clear();
    try {
      // Two halves of the same picture: what the library could name on its own,
      // and the ties it would not guess at. The second is reading the page
      // reports in no total, so it is fetched alongside rather than behind a
      // click nobody would think to make.
      [state.overview, state.ambiguous] = await Promise.all([
        api.invoke("reading_log_overview"),
        api.invoke("reading_log_ambiguous"),
      ]);
    } catch (e) {
      toast(`failed to load reading log: ${e}`, true);
      state.overview = null;
      state.ambiguous = [];
    }
    state.loaded = true;
    render();
  }

  function show() {
    if (!state.loaded) refresh();
    else render();
  }

  // A popover left open would float over whichever section replaces this one.
  function hide() {
    closeSort();
  }

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
    // Before the early return below: a library whose reading is *all* tied has
    // no days to draw, and that is the library that most needs to see this.
    renderAmbiguous();
    if (!has) {
      q("#rl-stats").innerHTML = "";
      return;
    }
    // The current year, which is what someone opening this page wants to see —
    // falling back to the newest year that has any reading, so a library whose
    // logs stop last year opens on data rather than on an empty grid.
    const years = yearsWithData(o);
    if (!years.includes(state.year)) {
      const now = new Date().getFullYear();
      state.year = years.includes(now) ? now : years[years.length - 1];
      state.day = null;
    }
    renderStats(o);
    renderRecent(o);
    renderYearNav(o, years);
    renderCalendar(o);
    renderClock(o);
    renderTimeline();
    renderScope();
  }

  // Draws `#rl-heatmap` or `#rl-monthcal` per `state.calView`, and hides the
  // other with the controls belonging to it.
  function renderCalendar(o) {
    if (!o) return;
    const month = state.calView === "month";
    q("#rl-heatmap").hidden = month;
    q("#rl-legend").hidden = month;
    q("#rl-monthcal").hidden = !month;
    q("#rl-cal-nav").hidden = !month;
    for (const b of q("#rl-cal-seg").querySelectorAll(".seg-btn")) {
      const on = (b.dataset.cal === "month") === month;
      b.classList.toggle("active", on);
      b.setAttribute("aria-selected", String(on));
    }
    if (month) renderMonthCal(o);
    else renderHeatmap(o);
  }

  function yearsWithData(o) {
    return [...new Set(o.days.map((d) => +d.day.slice(0, 4)))].sort((a, b) => a - b);
  }

  function daysOfYear(o, year) {
    const p = `${year}-`;
    return o.days.filter((d) => d.day.startsWith(p));
  }

  function statTile(value, label, hint) {
    const t = hint ? ` title="${hint}"` : "";
    return `<div class="rl-stat"${t}><b>${value}</b><span>${label}</span></div>`;
  }

  // The first three tiles cover `state.year`, matching the calendar and the
  // grid. The last three are all-time and say so.
  function renderStats(o) {
    const days = daysOfYear(o, state.year);
    const secs = days.reduce((a, d) => a + d.seconds, 0);
    const perDay = days.length ? Math.round(secs / days.length) : 0;
    const tiles = [
      statTile(fmtDuration(secs), `in ${state.year}`, `${fmtDuration(o.total_seconds)} all time`),
      statTile(days.length, "days read", `${o.days_read} all time`),
      statTile(fmtDuration(perDay), "per reading day", `In ${state.year}`),
      statTile(`${o.current_streak}d`, "streak", "Consecutive days up to today, all time"),
      statTile(`${o.longest_streak}d`, "longest streak", "All time"),
      statTile(o.books_total, "books", "Distinct books ever read"),
    ];
    q("#rl-stats").innerHTML = tiles.join("");
  }

  // The arrows step to the next year that *has* reading, never onto a blank
  // grid; with a single year of history there is nowhere to go, so the pair is
  // hidden rather than shown dead.
  function renderYearNav(o, years) {
    const i = years.indexOf(state.year);
    const prev = i > 0 ? years[i - 1] : null;
    const next = i >= 0 && i < years.length - 1 ? years[i + 1] : null;
    q("#rl-year-nav").classList.toggle("rl-nav-fixed", years.length < 2);
    q("#rl-year-label").textContent = state.year;
    setStep("#rl-year-prev", prev, (y) => `Go to ${y}`);
    setStep("#rl-year-next", next, (y) => `Go to ${y}`);
    const secs = daysOfYear(o, state.year).reduce((a, d) => a + d.seconds, 0);
    q("#rl-year-total").textContent = secs ? fmtDuration(secs) : "";
  }

  // A navigation arrow: disabled and unlabelled when there is nothing that way.
  // `target` is stashed on the element so the click handler reads its
  // destination rather than recomputing the bounds.
  function setStep(sel, target, title) {
    const btn = q(sel);
    btn.disabled = target === null;
    btn.dataset.target = target === null ? "" : String(target);
    btn.title = target === null ? "" : title(target);
  }

  // Scale by the busiest day rather than fixed thresholds, so the range is
  // meaningful whether the reader does 20 minutes or 4 hours a day.
  function levelScale(days) {
    const peak = Math.max(...days.map((d) => d.seconds), 1);
    return (secs) => {
      if (!secs) return 0;
      const r = secs / peak;
      return r > 0.66 ? 4 : r > 0.4 ? 3 : r > 0.15 ? 2 : 1;
    };
  }

  // GitHub-style: one column per week, Sunday at the top. The grid is built
  // from a fixed start so every column is a real week — padding the leading
  // days keeps the weekday rows aligned instead of shearing by one.
  function renderHeatmap(o) {
    const totals = new Map(o.days.map((d) => [d.day, d.seconds]));
    const end = new Date(state.year, 11, 31);
    let start = new Date(state.year, 0, 1);
    start = new Date(start.getFullYear(), start.getMonth(), start.getDate() - start.getDay());

    // Across all years, so a quiet year reads as quiet instead of being
    // stretched to fill the same five shades as a heavy one.
    const level = levelScale(o.days);

    const cols = [];
    const months = [];
    // `d` walks a week at a time by mutation, so the binding itself never moves.
    for (const d = new Date(start); d <= end; d.setDate(d.getDate() + 7)) {
      const week = [];
      for (let i = 0; i < 7; i++) {
        const cur = new Date(d.getFullYear(), d.getMonth(), d.getDate() + i);
        // A week can start in December and run into January; only the days
        // inside the year being shown belong to this grid.
        if (cur > end || cur.getFullYear() !== state.year) {
          week.push(`<i class="rl-cell rl-pad"></i>`);
          continue;
        }
        const key = dayKey(cur);
        const secs = totals.get(key) || 0;
        // Only a day with reading is clickable: selecting an empty one would
        // filter the grid down to nothing.
        const hit = secs ? ` data-day="${key}" role="button" tabindex="0"` : "";
        const sel = key === state.day ? " rl-sel" : "";
        week.push(
          `<i class="rl-cell rl-l${level(secs)}${sel}"${hit} ` +
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

  // ── The month calendar ─────────────────────────────────────────────────────
  // One bar per book per run of consecutive days. Rows come from
  // `reading_log_books` at `bucket: "day"`.

  // Lanes of bars per day. Also set on `.rl-monthcal` as `--rl-lanes`.
  const SPAN_LANES = 4;

  // `YYYY-MM` for a Date.
  function monthKey(d) {
    return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}`;
  }

  // `days`: 7 entries of `[{ book_id, title, seconds }]`, `seconds` descending,
  // empty outside the month. Returns 7 columns of `SPAN_LANES` lanes, where a
  // book on consecutive days holds one lane and carries `start` and `span`.
  function layoutWeek(days) {
    const lanes = [];
    for (let col = 0; col < days.length; col++) {
      const here = [];
      for (let lane = 0; lane < SPAN_LANES; lane++) {
        const book = days[col][lane];
        here[lane] = book ? { ...book, start: col, span: 1 } : null;
      }
      const prev = col > 0 ? lanes[col - 1] : null;
      if (prev) {
        for (let pn = 0; pn < prev.length; pn++) {
          const before = prev[pn];
          if (!before) continue;
          const tn = here.findIndex((b) => b && b.book_id === before.book_id);
          if (tn < 0) continue;
          // `before.span` is read before the loop below writes to `before`.
          const spanBefore = before.span;
          here[tn].start = before.start;
          here[tn].span = spanBefore + 1;
          for (let back = 1; back <= spanBefore; back++) {
            const cell = lanes[col - back][pn];
            if (cell) cell.span = here[tn].span;
          }
          if (tn !== pn) [here[tn], here[pn]] = [here[pn], here[tn]];
        }
      }
      lanes[col] = here;
    }
    return lanes;
  }

  // The books of each day of one month, keyed `YYYY-MM-DD`, busiest first.
  function booksByDay(rows) {
    const out = new Map();
    for (const r of rows) {
      if (!out.has(r.bucket)) out.set(r.bucket, []);
      out.get(r.bucket).push({ book_id: r.book_id, title: r.title, seconds: r.seconds });
    }
    for (const list of out.values()) list.sort((a, b) => b.seconds - a.seconds);
    return out;
  }

  // One month's per-day book rows and hour shapes, memoised in `state.calRows`
  // and `state.calShapes`.
  async function loadMonth(anchor) {
    const key = monthKey(anchor);
    const last = new Date(anchor.getFullYear(), anchor.getMonth() + 1, 0).getDate();
    const from = `${key}-01`;
    const to = `${key}-${String(last).padStart(2, "0")}`;
    if (!state.calRows.has(key)) {
      const [rows, shapes] = await Promise.all([
        api.invoke("reading_log_books", { from, to, sort: "seconds", asc: false, bucket: "day" }),
        api.invoke("reading_log_day_hours", { from, to }),
      ]);
      state.calRows.set(key, rows);
      state.calShapes.set(key, new Map(shapes.map((s) => [s.day, s])));
    }
    return state.calRows.get(key);
  }

  // Seconds of a day placed whole on one hour, past which `hours` carries one
  // saturated bar and 23 empty ones. Such a shape is left undrawn.
  function shapeIsEvidence(shape) {
    if (!shape) return false;
    const total = shape.hours.reduce((a, h) => a + h, 0);
    return total > 0 && shape.unplaced_seconds * 2 <= total;
  }

  // 24 bars, scaled to the day's own busiest hour.
  function shapeHtml(shape) {
    if (!shapeIsEvidence(shape)) return "";
    const peak = Math.max(...shape.hours, 1);
    const bars = shape.hours
      .map((s) => `<i style="--v:${((s / peak) * 100).toFixed(1)}%"></i>`)
      .join("");
    return bars;
  }

  // `YYYY-MM` of every month in `state.year` with reading, ascending.
  function monthsWithData(o) {
    const p = `${state.year}-`;
    return [...new Set(o.days.filter((d) => d.day.startsWith(p)).map((d) => d.day.slice(0, 7)))]
      .sort();
  }

  async function renderMonthCal(o) {
    const months = monthsWithData(o);
    if (!months.length) {
      q("#rl-monthcal").innerHTML = "";
      return;
    }
    // `state.calMonth` outside `state.year` is reset to a month in `months`.
    if (!state.calMonth || monthKey(state.calMonth).slice(0, 4) !== String(state.year)) {
      const pick = months.includes(monthKey(new Date()))
        ? monthKey(new Date())
        : months[months.length - 1];
      state.calMonth = new Date(+pick.slice(0, 4), +pick.slice(5, 7) - 1, 1);
    }
    const anchor = state.calMonth;
    const key = monthKey(anchor);
    const i = months.indexOf(key);
    q("#rl-cal-label").textContent = anchor.toLocaleDateString(undefined, {
      month: "long",
      year: "numeric",
    });
    const label = (k) =>
      new Date(+k.slice(0, 4), +k.slice(5, 7) - 1, 1).toLocaleDateString(undefined, {
        month: "long",
        year: "numeric",
      });
    setStep("#rl-cal-prev", i > 0 ? months[i - 1] : null, (k) => `Go to ${label(k)}`);
    setStep("#rl-cal-next", i >= 0 && i < months.length - 1 ? months[i + 1] : null, (k) =>
      `Go to ${label(k)}`,
    );

    const token = ++state.calPending;
    let rows = [];
    try {
      rows = await loadMonth(anchor);
    } catch (e) {
      toast(`failed to load ${key}: ${e}`, true);
    }
    if (token !== state.calPending) return;
    const shapes = state.calShapes.get(key) || new Map();
    q("#rl-monthcal").innerHTML = monthCalHtml(o, anchor, booksByDay(rows), shapes);
  }

  function monthCalHtml(o, anchor, byDay, shapes) {
    const totals = new Map(o.days.map((d) => [d.day, d.seconds]));
    const year = anchor.getFullYear();
    const month = anchor.getMonth();
    const len = new Date(year, month + 1, 0).getDate();
    const lead = new Date(year, month, 1).getDay();
    const level = levelScale(daysOfYear(o, state.year));
    const today = dayKey(new Date());

    // Whole weeks, Sunday first, padded at both ends with null.
    const cells = [];
    for (let i = 0; i < lead; i++) cells.push(null);
    for (let d = 1; d <= len; d++) cells.push(new Date(year, month, d));
    while (cells.length % 7) cells.push(null);

    const out = [
      `<div class="rl-cal-dows">` +
        DOW.map((d) => `<span>${d}</span>`).join("") +
        `</div>`,
    ];
    q("#rl-monthcal").style.setProperty("--rl-lanes", String(SPAN_LANES));
    for (let w = 0; w < cells.length; w += 7) {
      const week = cells.slice(w, w + 7);
      const days = week.map((c) => (c ? byDay.get(dayKey(c)) || [] : []));
      const lanes = layoutWeek(days);
      const parts = [];

      week.forEach((cur, col) => {
        if (!cur) {
          parts.push(
            `<div class="rl-cal-pad" style="grid-column:${col + 1}; grid-row:1/-1"></div>`,
          );
          return;
        }
        const key = dayKey(cur);
        const secs = totals.get(key) || 0;
        const hit = secs ? ` data-day="${key}" role="button" tabindex="0"` : "";
        const sel = key === state.day ? " rl-cal-sel" : "";
        const now = key === today ? " rl-cal-today" : "";
        // `grid-row:1/-1` puts the background under the date row and the lanes.
        parts.push(
          `<div class="rl-cal-cell${sel}${now}" style="grid-column:${col + 1}; grid-row:1/-1"${hit} ` +
            `title="${fmtDay(key)} — ${secs ? fmtDuration(secs) : "nothing"}"></div>`,
        );
        const bars = shapeHtml(shapes.get(key));
        if (bars) {
          parts.push(
            `<div class="rl-cal-shape" style="grid-column:${col + 1}; grid-row:-2">${bars}</div>`,
          );
        }
        const extra = days[col].length - SPAN_LANES;
        parts.push(
          `<div class="rl-cal-num" style="grid-column:${col + 1}; grid-row:1">` +
            `<b>${cur.getDate()}</b>` +
            (secs ? `<em class="rl-l${level(secs)} rl-cal-dot"></em>` : "") +
            (secs ? `<span>${fmtDuration(secs)}</span>` : "") +
            (extra > 0 ? `<i class="rl-cal-more">+${extra}</i>` : "") +
            `</div>`,
        );
      });

      // A run is emitted once, at `book.start`.
      lanes.forEach((lane, col) => {
        lane.forEach((book, row) => {
          if (!book || book.start !== col) return;
          parts.push(
            `<span class="rl-cal-span" data-book="${book.book_id}" role="button" tabindex="0" ` +
              `style="grid-column:${col + 1} / span ${book.span}; grid-row:${row + 2}; ` +
              `--fill:${bookFill(book.book_id)}; --ink:${bookInk(book.book_id)}" ` +
              `title="${esc(book.title)}">${esc(book.title)}</span>`,
          );
        });
      });

      out.push(`<div class="rl-cal-week">${parts.join("")}</div>`);
    }
    return out.join("");
  }

  // ── The day timeline ───────────────────────────────────────────────────────
  // The sittings of `state.day` on one 24-hour axis. A block spans
  // `[started_at, ended_at]`; the fill inside it is `seconds`.

  const DAY_SECS = 86400;

  // Seconds into the day, from a `YYYY-MM-DDTHH:MM:SS` stamp.
  function clockSecs(iso) {
    if (!iso || iso.length < 19) return null;
    return +iso.slice(11, 13) * 3600 + +iso.slice(14, 16) * 60 + +iso.slice(17, 19);
  }

  // `[start, end]` of one sitting in seconds of `day`, clipped to it. An end
  // before its start ran past midnight and stops at the day's edge.
  function sessionSpan(s, day) {
    const from = clockSecs(s.started_at);
    if (from == null) return null;
    let to = clockSecs(s.ended_at);
    if (to == null || s.ended_at.slice(0, 10) !== day || to < from) to = DAY_SECS;
    return [from, Math.max(to, from + 60)];
  }

  // Packs sittings into rows where no two overlap, earliest first.
  function packLanes(spans) {
    const lanes = [];
    for (const item of spans) {
      let lane = lanes.find((l) => l[l.length - 1].span[1] <= item.span[0]);
      if (!lane) {
        lane = [];
        lanes.push(lane);
      }
      lane.push(item);
    }
    return lanes;
  }

  async function renderTimeline() {
    const day = state.day;
    const box = q("#rl-timeline");
    box.hidden = !day;
    if (!day) return;
    q("#rl-timeline-title").textContent = fmtDay(day);

    const token = ++state.tlPending;
    let rows = [];
    try {
      rows = await api.invoke("reading_log_sessions", { from: day, to: day });
    } catch (e) {
      toast(`failed to load sittings for ${day}: ${e}`, true);
    }
    if (token !== state.tlPending) return;

    const spans = rows
      .map((s) => ({ s, span: sessionSpan(s, day) }))
      .filter((x) => x.span)
      .sort((a, b) => a.span[0] - b.span[0]);
    const counted = rows.reduce((a, s) => a + s.seconds, 0);
    q("#rl-timeline-note").textContent = spans.length
      ? `${spans.length} sitting${spans.length === 1 ? "" : "s"} · ${fmtDuration(counted)}`
      : "";
    q("#rl-timeline-body").innerHTML = spans.length ? timelineHtml(spans, day) : "";
  }

  function timelineHtml(spans, day) {
    const pct = (v) => `${((v / DAY_SECS) * 100).toFixed(3)}%`;
    const lanes = packLanes(spans)
      .map((lane) => {
        const blocks = lane.map(({ s, span }) => {
          const width = span[1] - span[0];
          // `seconds` is counted reading and never exceeds the window it is
          // drawn in.
          const fill = Math.min(1, s.seconds / width);
          return (
            `<span class="rl-tl-block" data-book="${s.book_id}" role="button" tabindex="0" ` +
            `style="left:${pct(span[0])}; width:${pct(width)}; ` +
            `--fill:${bookFill(s.book_id)}; --ink:${bookInk(s.book_id)}; ` +
            `--read:${(fill * 100).toFixed(1)}%" ` +
            `title="${esc(s.title)}\n${s.started_at.slice(11, 16)}–${s.ended_at.slice(11, 16)}` +
            ` · ${fmtDuration(s.seconds)} read">` +
            `<i class="rl-tl-read"></i><b>${esc(s.title)}</b></span>`
          );
        });
        return `<div class="rl-tl-lane">${blocks.join("")}</div>`;
      })
      .join("");

    const ticks = [0, 3, 6, 9, 12, 15, 18, 21]
      .map(
        (h) =>
          `<span class="rl-tl-tick" style="left:${pct(h * 3600)}">` +
          `${String(h).padStart(2, "0")}</span>`,
      )
      .join("");
    // A marker for the current time, on today only.
    const now = new Date();
    const isToday = dayKey(now) === day;
    const marker = isToday
      ? `<i class="rl-tl-now" style="left:${pct(
          now.getHours() * 3600 + now.getMinutes() * 60,
        )}"></i>`
      : "";
    return `<div class="rl-tl-plot">${lanes}${marker}</div><div class="rl-tl-axis">${ticks}</div>`;
  }

  // ── Recently ───────────────────────────────────────────────────────────────

  // Days drawn in the band above the calendar.
  const RECENT_DAYS = 14;

  function renderRecent(o) {
    const totals = new Map(o.days.map((d) => [d.day, d.seconds]));
    const today = new Date();
    const bars = [];
    let peak = 1;
    const window = [];
    for (let i = RECENT_DAYS - 1; i >= 0; i--) {
      const d = new Date(today.getFullYear(), today.getMonth(), today.getDate() - i);
      const key = dayKey(d);
      const secs = totals.get(key) || 0;
      peak = Math.max(peak, secs);
      window.push({ key, d, secs });
    }
    for (const { key, d, secs } of window) {
      const hit = secs ? ` data-day="${key}" role="button" tabindex="0"` : "";
      bars.push(
        `<span class="rl-recent-bar${key === state.day ? " rl-recent-sel" : ""}"${hit} ` +
          `style="--v:${((secs / peak) * 100).toFixed(1)}%" ` +
          `title="${fmtDay(key)} — ${secs ? fmtDuration(secs) : "nothing"}">` +
          `<i></i><em>${d.getDate()}</em></span>`,
      );
    }
    const sum = window.reduce((a, w) => a + w.secs, 0);
    const todaySecs = totals.get(dayKey(today)) || 0;
    q("#rl-recent-note").textContent =
      `${fmtDuration(sum)} in ${RECENT_DAYS} days` +
      (todaySecs ? ` · ${fmtDuration(todaySecs)} today` : "");
    q("#rl-recent-bars").innerHTML = bars.join("");
    q("#rl-recent").hidden = sum === 0;
  }

  // ── When you read ──────────────────────────────────────────────────────────
  //
  // The heatmap answers "which days"; this answers "when in them". Both draw the
  // selected year, so they read as one picture — a square on the left is a day,
  // and the panel on the right is what the hours of such days look like.
  //
  // The backend sends one cube of (month, weekday, hour) seconds for all time
  // (`db::reading_clock`, where the spreading rule and its one caveat live).
  // Every view here is a marginal of it, which is why they can be cut in the
  // page rather than asked for: hour-seconds add up, unlike the per-book
  // aggregates the grid below shows, where re-slicing a total is exactly the
  // bug `ReadingBucket` exists to prevent.

  const DOW = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
  const MONTHS = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun",
    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
  ];

  // The cube, narrowed to the year on screen.
  function clockOfYear(o) {
    const p = `${state.year}-`;
    return (o.clock || []).filter((c) => c.month.startsWith(p));
  }

  // Seconds per hour, for one grouping of the cube. `keyOf` says which row a
  // cell belongs to; the result is a Map of row key → 24 hours.
  function clockRows(cells, keyOf) {
    const rows = new Map();
    for (const c of cells) {
      const key = keyOf(c);
      if (!rows.has(key)) rows.set(key, new Array(24).fill(0));
      rows.get(key)[c.hour] += c.seconds;
    }
    return rows;
  }

  function renderClock(o) {
    const cells = clockOfYear(o);
    // A year always has days here — `render` only reaches this with data — but
    // a year whose every session predates the stamps would have no cube.
    q(".rl-clock-wrap").hidden = cells.length === 0;
    if (!cells.length) return;
    renderClockSeg();
    const view = state.clockView;
    const [html, note] =
      view === "hour" ? clockBars(cells) : clockGrid(cells, view);
    q("#rl-clock").innerHTML = html;
    q("#rl-clock-note").textContent = note;
  }

  function renderClockSeg() {
    for (const b of q("#rl-clock-seg").querySelectorAll(".seg-btn")) {
      const on = b.dataset.clock === state.clockView;
      b.classList.toggle("active", on);
      b.setAttribute("aria-selected", String(on));
    }
  }

  // "23:00–24:00" — an hour is a span, and labelling a bar `23` alone invites
  // reading it as an instant.
  function hourSpan(h) {
    const p = (n) => String(n).padStart(2, "0");
    return `${p(h)}:00–${p(h + 1)}:00`;
  }

  // The year's hours as bars. Heights are a fraction of the busiest hour rather
  // than of a fixed scale, for the same reason the heatmap shades that way: the
  // shape of a day is the point, not how it compares to somebody else's.
  function clockBars(cells) {
    const hours = clockRows(cells, () => "").get("") || new Array(24).fill(0);
    const peak = Math.max(...hours, 1);
    const bars = hours
      .map((secs, h) => {
        // A zero bar draws nothing at all — a stub of colour would read as a
        // little reading rather than as none.
        const v = secs / peak;
        return (
          `<div class="rl-bar" style="--v:${v.toFixed(4)}" ` +
          `title="${hourSpan(h)} — ${secs ? fmtDuration(secs) : "nothing"}">` +
          `<i></i></div>`
        );
      })
      .join("");
    // Every third hour, so the axis stays legible at any panel width while each
    // label still sits under the bar it names.
    const axis = hours
      .map((_, h) => `<span>${h % 3 === 0 ? String(h).padStart(2, "0") : ""}</span>`)
      .join("");
    return [
      `<div class="rl-bars">${bars}</div><div class="rl-bar-axis">${axis}</div>`,
      peakNote(hours.map((secs, h) => [hourSpan(h), secs])),
    ];
  }

  // Hour of the day against the weekday, or against the month: 24 columns on the
  // heatmap's own five-step ramp, so a cell here and a square there mean the
  // same kind of thing.
  function clockGrid(cells, view) {
    const byWeek = view === "week";
    const rows = clockRows(cells, (c) => (byWeek ? c.dow : c.month));
    const order = byWeek ? [...rows.keys()].sort((a, b) => a - b) : monthSpan(rows);
    const label = (k) => (byWeek ? DOW[k] : MONTHS[+k.slice(5, 7) - 1]);

    // Scaled across the whole grid, not per row: a quiet month must look quiet
    // beside a busy one, which is the entire point of the month view.
    const peak = Math.max(...[...rows.values()].flat(), 1);
    const level = (secs) => {
      if (!secs) return 0;
      const r = secs / peak;
      return r > 0.66 ? 4 : r > 0.4 ? 3 : r > 0.15 ? 2 : 1;
    };

    const head =
      `<span></span>` +
      new Array(24)
        .fill(0)
        .map((_, h) => `<span>${h % 3 === 0 ? String(h).padStart(2, "0") : ""}</span>`)
        .join("");
    const body = order
      .map((key) => {
        // A month inside the reading span with nothing in it is a real row of
        // zeroes — a month you did not read is worth seeing — so it is drawn
        // rather than skipped.
        const hours = rows.get(key) || new Array(24).fill(0);
        const name = label(key);
        return (
          `<span class="rl-clock-label">${esc(name)}</span>` +
          hours
            .map(
              (secs, h) =>
                `<i class="rl-clock-cell rl-l${level(secs)}" title="${esc(name)} ` +
                `${hourSpan(h)} — ${secs ? fmtDuration(secs) : "nothing"}"></i>`,
            )
            .join("")
        );
      })
      .join("");
    const flat = [];
    for (const key of order) {
      (rows.get(key) || []).forEach((secs, h) =>
        flat.push([`${label(key)} ${hourSpan(h)}`, secs]),
      );
    }
    return [
      `<div class="rl-clock-grid">${head}${body}</div>`,
      peakNote(flat),
    ];
  }

  // Every month from the first read to the last, so a gap between two reading
  // months is visible instead of being closed up — the same rule the book
  // page's calendar arrows follow.
  function monthSpan(rows) {
    const keys = [...rows.keys()].sort();
    const [lo, hi] = [+keys[0].slice(5, 7), +keys[keys.length - 1].slice(5, 7)];
    const year = keys[0].slice(0, 4);
    const out = [];
    for (let m = lo; m <= hi; m++) out.push(`${year}-${String(m).padStart(2, "0")}`);
    return out;
  }

  // "Most at Tue 22:00–23:00 · 4h 12m". One line, because a chart of this size
  // states its shape and nothing else — the figure behind the tallest bar is
  // the one thing you cannot read off it.
  function peakNote(pairs) {
    let best = null;
    for (const [label, secs] of pairs) {
      if (secs > 0 && (!best || secs > best[1])) best = [label, secs];
    }
    return best ? `most at ${best[0]} · ${fmtDuration(best[1])}` : "";
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
      `<div class="rl-card-time">${fmtDuration(e.seconds)}${estimateMark(e)}</div>` +
      `<div class="rl-card-sub">${esc(sub || "")}</div>` +
      `</div></div>`
    );
  }

  // What the grid can be ordered by. Every one is a column of the aggregate, so
  // the ordering happens in SQL and matches the figures on the cards.
  const SORT_KEYS = [
    ["last", "Last read", "Most recently read first"],
    ["seconds", "Reading time", "Longest first"],
    ["sessions", "Sessions", "Most sittings first"],
    ["words", "Words", "Most words read first"],
  ];

  // The grid always shows exactly what the heatmap above it is showing: the
  // whole year, or one day of it once a square is clicked. Its figures come
  // from a windowed query rather than a filtered all-time list, because a
  // book's hours *that day* are not its hours ever — which is also why the
  // month/day bands are asked for by the query instead of being cut out of a
  // yearly list here: a book read across three months has three sets of figures.
  async function renderScope() {
    const day = state.day;
    const [from, to] = day ? [day, day] : [`${state.year}-01-01`, `${state.year}-12-31`];
    // A selected day is already a single band, and the header names it.
    const bucket = day ? "total" : state.bucket;
    q("#rl-books-title").textContent = day ? fmtDay(day) : `Books in ${state.year}`;
    q("#rl-day-clear").hidden = !day;
    renderBucketControl();
    renderSortControl();

    // A later click must win, however the replies happen to arrive back.
    const token = ++state.scope;
    let rows = [];
    try {
      rows = await api.invoke("reading_log_books", {
        from,
        to,
        sort: state.sort.key,
        asc: state.sort.asc,
        bucket,
      });
    } catch (e) {
      toast(`failed to load ${day || state.year}: ${e}`, true);
    }
    if (token !== state.scope) return;

    state.books = rows;
    const total = rows.reduce((a, r) => a + r.seconds, 0);
    q("#rl-books-total").textContent = rows.length ? fmtDuration(total) : "nothing read";
    // A band per day means every card already sits under its own date, so the
    // cards say the time of day instead of repeating it — the same thing they
    // do when a single day is selected.
    const daily = !!day || bucket === "day";
    const banded = bucket === "month" || bucket === "day";
    const list = q("#rl-book-list");
    list.className = banded ? "rl-bands" : "rl-cards";
    list.innerHTML = banded ? bandsHtml(rows, bucket, daily) : cardsHtml(rows, daily);
  }

  // A day's cards say when the sitting began; a year's say when the book was
  // last open, which is what the default order sorts on — so the sequence on
  // screen is legible rather than something you have to take on trust.
  function cardsHtml(rows, daily) {
    return rows
      .map((r) =>
        entryCard(
          r,
          daily
            ? `from ${r.first_at.slice(11, 16)}`
            : `${shortDay(r.last_at)} · ${r.sessions} sessions`,
        ),
      )
      .join("");
  }

  // Rows come back grouped and already in the asked-for direction, so one pass
  // over them keeps that order rather than re-deriving it.
  function bandsHtml(rows, bucket, daily) {
    const out = [];
    for (let i = 0; i < rows.length; ) {
      let j = i;
      while (j < rows.length && rows[j].bucket === rows[i].bucket) j++;
      const band = rows.slice(i, j);
      const secs = band.reduce((a, r) => a + r.seconds, 0);
      out.push(
        `<section class="rl-band"><header class="rl-band-head">` +
          `<strong>${esc(bandLabel(rows[i].bucket, bucket))}</strong>` +
          `<span class="rl-muted">${fmtDuration(secs)} · ` +
          `${band.length} book${band.length === 1 ? "" : "s"}</span>` +
          `</header><div class="rl-cards">${cardsHtml(band, daily)}</div></section>`,
      );
      i = j;
    }
    return out.join("");
  }

  // "August", or "August 9" — the year is in the header above every band, so
  // repeating it on each one says nothing.
  function bandLabel(key, bucket) {
    const d = parseDay(bucket === "month" ? `${key}-01` : key);
    if (!d) return key;
    const shape =
      bucket === "month" ? { month: "long" } : { month: "long", day: "numeric" };
    return d.toLocaleDateString(undefined, shape);
  }

  function renderBucketControl() {
    const seg = q("#rl-bucket-seg");
    seg.hidden = !!state.day;
    for (const b of seg.querySelectorAll(".seg-btn")) {
      const on = b.dataset.bucket === state.bucket;
      b.classList.toggle("active", on);
      b.setAttribute("aria-selected", String(on));
    }
  }

  function renderSortControl() {
    const [, name] = SORT_KEYS.find(([k]) => k === state.sort.key) || SORT_KEYS[0];
    q("#rl-sort-button .sort-label").textContent = `Sort: ${name}`;
    q("#rl-sort-button .sort-dir").textContent = state.sort.asc ? "↑" : "↓";

    const list = q("#rl-sort-keys");
    list.innerHTML = "";
    for (const [key, label, hint] of SORT_KEYS) {
      const li = document.createElement("li");
      li.dataset.key = key;
      li.title = hint;
      if (state.sort.key === key) li.classList.add("active");
      const radio = document.createElement("span");
      radio.className = "sort-radio";
      const text = document.createElement("span");
      text.textContent = label;
      li.append(radio, text);
      list.appendChild(li);
    }
    for (const b of document.querySelectorAll("#rl-sort-popover .sort-dir-toggle button")) {
      b.classList.toggle("active", b.dataset.dir === (state.sort.asc ? "asc" : "desc"));
    }
  }

  function closeSort() {
    q("#rl-sort-popover").hidden = true;
  }

  // ── Which book was this? ───────────────────────────────────────────────────
  //
  // A device's log names no book: it states the position its reading stopped at,
  // and a book is recognised by ending exactly there. Two books of identical
  // length end at the same position, so the reading fits both and the automatic
  // pass refuses to pick — the one question here that a person can answer, by
  // looking at two covers and remembering which they read.
  //
  // Reading whose position fits NO book is not this case and is never listed.
  // That book is not in the library, and nothing about the group says which book
  // it was: a duration, a date span and a word count identify nothing. It stays
  // where it is — counted nowhere, named on its own the day its book comes back.
  //
  // The backend sends only ties (`reading_log_ambiguous`), so everything drawn
  // here has candidates to draw.

  // Where reading stopped is the identity of a group, so it is what every action
  // is keyed by — never the sessions, which are just what accumulated there.
  // No early return on an empty list: hiding the section while leaving the last
  // question drawn inside it keeps a settled tie in the page, one `hidden` away
  // from being shown again.
  function renderAmbiguous() {
    const groups = state.ambiguous;
    const secs = groups.reduce((a, g) => a + g.seconds, 0);
    q("#rl-ambiguous").hidden = groups.length === 0;
    q("#rl-ambiguous-total").textContent = groups.length
      ? `${fmtDuration(secs)} · ${groups.length} to settle`
      : "";
    q("#rl-ambiguous-list").innerHTML = groups.map(groupRow).join("");
  }

  // "8m · 2 sessions · Jun 22 – Jun 23". Not an identification — nothing here
  // identifies a book — but the reading being claimed, which is what the choice
  // below it is about.
  function groupFacts(g) {
    const span =
      shortDay(g.first_at) === shortDay(g.last_at)
        ? shortDay(g.last_at)
        : `${shortDay(g.first_at)} – ${shortDay(g.last_at)}`;
    const parts = [
      fmtDuration(g.seconds),
      `${g.sessions} session${g.sessions === 1 ? "" : "s"}`,
      span,
    ];
    if (g.devices.length) parts.push(g.devices.join(", "));
    return parts.join(" · ");
  }

  // The candidates are the whole of the question, so they are on the row from
  // the start — there is nothing to expand and no step between seeing the tie
  // and settling it.
  function groupRow(g) {
    const cands = g.candidates || [];
    return (
      `<li class="rl-ambiguous-row" data-position="${g.end_position}">` +
      `<div class="rl-ambiguous-facts">${esc(groupFacts(g))}</div>` +
      `<p class="rl-pick-note">${cands.length === 2 ? "Both" : `All ${cands.length}`} ` +
      `of these end at exactly this position. Which one did you read?</p>` +
      `<div class="rl-pick-books">${cands.map(bookOption).join("")}</div>` +
      `</li>`
    );
  }

  // A candidate: the cover at a size you can recognise a book by, its title and
  // author under it. The cover is the point — two same-length books are told
  // apart by looking, not by reading a figure.
  function bookOption(b) {
    const url = coverUrlFor(b, { thumb: true });
    return (
      `<button type="button" class="rl-pick-book" data-book="${b.id}" ` +
      `title="${esc(b.title)}${b.author ? `\n${esc(b.author)}` : ""}">` +
      `<span class="cover rl-pick-cover${url ? " has-image" : ""}">` +
      coverInner(url, b.title) +
      `</span><span class="t">${esc(b.title)}</span>` +
      `<span class="a">${esc(b.author || "Unknown author")}</span></button>`
    );
  }

  // Settle one. The choice is the answer, not a proposal: the row goes, the
  // reading belongs to that book, and the page reloads because the totals, the
  // heatmap and the grid all change — this is reading that was in none of them
  // a moment ago.
  async function nameBook(position, bookId) {
    const group = state.ambiguous.find((g) => g.end_position === position);
    const book = (group?.candidates || []).find((b) => b.id === bookId);
    let moved;
    try {
      moved = await api.invoke("reading_log_attribute", { endPosition: position, bookId });
    } catch (e) {
      toast(`could not name that reading: ${e}`, true);
      return;
    }
    toast(
      `${fmtDuration(group?.seconds || 0)} across ${moved} session` +
        `${moved === 1 ? "" : "s"} → ${book ? book.title : "that book"}`,
    );
    await refresh();
  }

  // ── One book ───────────────────────────────────────────────────────────────

  async function openBook(bookId) {
    // Read at the click, not after the reply: this is where the list was when
    // the card the user pressed was on screen.
    const from = scroller().scrollTop;
    try {
      // The annotations are the reader's own query — one book's highlights and
      // notes, already grouped — so this page and the reader's sidebar can never
      // disagree about what the book carries. A failure there is reported but
      // does not hold up the reading history, which is what the page is for.
      let failed = false;
      const [data, notes] = await Promise.all([
        api.invoke("reading_log_book", { bookId }),
        api.invoke("annotations_for_book", { bookId }).catch((e) => {
          toast(`failed to load highlights: ${e}`, true);
          failed = true;
          return [];
        }),
      ]);
      state.overviewScroll = from;
      state.book = { id: bookId, ...data };
      state.notes = notes || [];
      state.notesFailed = failed;
      const last = data.days.length ? parseDay(data.days[data.days.length - 1].day) : new Date();
      state.month = new Date(last.getFullYear(), last.getMonth(), 1);
      render();
      scroller().scrollTop = 0;
    } catch (e) {
      toast(`failed to load book: ${e}`, true);
    }
  }

  // Into the book itself, from the cover. Goes through the gallery's own open
  // path (`openReader`, library.js) rather than calling the reader directly, so
  // a slow KFX load says "Opening …" in the status bar exactly as it does from
  // the Books grid — the alternative is a click that looks like it did nothing
  // for several seconds. The reader is a full overlay: closing it comes back to
  // this page, untouched.
  function openInReader() {
    const { id, entry } = state.book || {};
    if (!entry) return;
    openReader({ id, title: entry.title });
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
      // The cover opens the book. It carries the affordance only while there is
      // a book behind it — an entry-less page draws an empty frame, and a
      // control that opens nothing has no business being focusable. `role` is
      // also what the click and key handlers match on, so the thing that looks
      // pressable and the thing that responds are the same thing.
      box.setAttribute("role", "button");
      box.setAttribute("tabindex", "0");
      box.setAttribute("aria-label", `Open ${entry.title} in the reader`);
      box.title = "Open in the reader";

      const span = spanDays(days);
      const perDay = days.length ? Math.round(entry.seconds / days.length) : 0;
      // Pace comes from the device's own word counts, which is why it can be
      // shown at all — nothing here is inferred from page geometry.
      const wpm = entry.seconds > 0 ? Math.round((entry.words * 60) / entry.seconds) : 0;
      q("#rl-book-stats").innerHTML = [
        factHtml("Total", `${esc(fmtDuration(entry.seconds))}${estimateMark(entry)}`),
        fact("Days read", days.length),
        fact("Per day", fmtDuration(perDay)),
        fact("Days elapsed", span, "First to last day read"),
        fact("Sessions", entry.sessions),
        fact("Words", fmtWords(entry.words)),
        wpm ? fact("Words / min", wpm) : "",
        // Deliberately not called "pages": a converted book has no pagination,
        // and this counts forward taps at whatever font size the device was on.
        fact(
          "Page turns",
          entry.page_turns,
          "Forward page turns on the device — depends on font size, not a page count",
        ),
        fact("First read", shortDay(entry.first_at)),
        fact("Last read", shortDay(entry.last_at)),
        ...paceFacts(entry, days),
        // Blank for sessions imported before Sidle was told which Kindle wrote
        // them; a row saying nothing beats a row inventing a device.
        entry.devices.length ? fact("Read on", entry.devices.join(", ")) : "",
      ].join("");
    } else {
      for (const a of ["role", "tabindex", "aria-label", "title"]) box.removeAttribute(a);
      q("#rl-book-stats").innerHTML = "";
    }
    renderProgress();
    renderMonth();
    renderNotes();
  }

  // The fraction of the axis read, or null where either half is unstored.
  // `linear_pos` past `max_position` comes of a stored file differing from the
  // build the device read, and clamps.
  function readFraction() {
    const p = state.book?.progress;
    if (!p || !p.max_position) return null;
    return Math.min(1, Math.max(0, p.linear_pos / p.max_position));
  }

  function renderProgress() {
    const frac = readFraction();
    const box = q("#rl-book-progress");
    box.hidden = frac === null;
    if (frac === null) return;
    const pct = Math.round(frac * 100);
    q("#rl-book-progress-fill").style.width = `${(frac * 100).toFixed(1)}%`;
    q("#rl-book-progress-label").textContent =
      frac >= 1 ? "At the end" : `${pct}% of the way in`;
    q("#rl-book-progress-label").title =
      `Position ${state.book.progress.linear_pos} of ${state.book.progress.max_position}` +
      ` (${state.book.progress.source})`;
  }

  // Time left and a finish date, at this book's own measured pace. Both are
  // absent without a position, and at the end of the book.
  function paceFacts(entry, days) {
    const frac = readFraction();
    if (frac === null || frac <= 0 || frac >= 1 || !entry.seconds || !days.length) return [];
    // The seconds behind `frac` of the axis, scaled to what is left of it.
    const left = Math.round((entry.seconds * (1 - frac)) / frac);
    const perDay = entry.seconds / days.length;
    const daysLeft = perDay > 0 ? Math.ceil(left / perDay) : 0;
    const out = [
      fact("Time left", fmtDuration(left), `At ${fmtDuration(Math.round(perDay))} a reading day`),
    ];
    if (daysLeft > 0) {
      const end = new Date();
      end.setDate(end.getDate() + daysLeft);
      out.push(
        fact(
          "Finish by",
          shortDay(dayKey(end)),
          `${daysLeft} reading day${daysLeft === 1 ? "" : "s"} at the current pace`,
        ),
      );
    }
    return out;
  }

  // One figure as a label/value row. A vertical list beside the calendar reads
  // better than a strip of boxes across the top, and scales as figures are
  // added without pushing anything off the edge.
  function fact(label, value, hint) {
    return factHtml(label, esc(value), hint);
  }

  // The same, for a value that carries markup of its own — a figure with the
  // estimate mark on it, say.
  function factHtml(label, valueHtml, hint) {
    const t = hint ? ` title="${esc(hint)}"` : "";
    return `<div class="rl-fact"${t}><dt>${esc(label)}</dt><dd>${valueHtml}</dd></div>`;
  }

  function spanDays(days) {
    if (days.length < 2) return days.length;
    const a = parseDay(days[0].day);
    const b = parseDay(days[days.length - 1].day);
    return Math.round((b - a) / 86400000) + 1;
  }

  // Months as a single ordinal, so "the month before" is arithmetic and the
  // bounds compare without date objects.
  function monthIndex(y, m) {
    return y * 12 + m;
  }

  function monthFromIndex(i) {
    return new Date(Math.floor(i / 12), i % 12, 1);
  }

  // The calendar goes no further than the book's own reading. Unlike the year
  // arrows it steps one month at a time and does not skip: a gap between two
  // reading months is itself worth seeing.
  function monthBounds(days) {
    const idx = days.map((d) => {
      const p = parseDay(d.day);
      return monthIndex(p.getFullYear(), p.getMonth());
    });
    return [Math.min(...idx), Math.max(...idx)];
  }

  function renderMonth() {
    const { days } = state.book;
    const totals = new Map(days.map((d) => [d.day, d.seconds]));
    const anchor = state.month;
    q("#rl-month-label").textContent = anchor.toLocaleDateString(undefined, {
      month: "long",
      year: "numeric",
    });

    const here = monthIndex(anchor.getFullYear(), anchor.getMonth());
    const [lo, hi] = days.length ? monthBounds(days) : [here, here];
    const label = (i) =>
      monthFromIndex(i).toLocaleDateString(undefined, { month: "long", year: "numeric" });
    q("#rl-month-nav").classList.toggle("rl-nav-fixed", lo === hi);
    setStep("#rl-prev", here > lo ? here - 1 : null, (i) => `Go to ${label(i)}`);
    setStep("#rl-next", here < hi ? here + 1 : null, (i) => `Go to ${label(i)}`);

    const first = new Date(anchor.getFullYear(), anchor.getMonth(), 1);
    const lead = first.getDay();
    const len = new Date(anchor.getFullYear(), anchor.getMonth() + 1, 0).getDate();
    // This book's own busiest day sets the scale, so its calendar shades the
    // same way the year heatmap does — a day read is a day that looks read.
    const level = levelScale(days);

    const cells = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"].map(
      (d) => `<span class="rl-mday-head">${d}</span>`,
    );
    // Lead-in blanks carry no level class, which is the whole difference
    // between "before the month started" and "a day with no reading".
    for (let i = 0; i < lead; i++) cells.push(`<span class="rl-mday"></span>`);
    for (let day = 1; day <= len; day++) {
      const key = dayKey(new Date(anchor.getFullYear(), anchor.getMonth(), day));
      const secs = totals.get(key) || 0;
      cells.push(
        `<span class="rl-mday rl-l${level(secs)}" title="${secs ? fmtDuration(secs) : "nothing"}">` +
          `<b>${day}</b>${secs ? `<em>${fmtDuration(secs)}</em>` : ""}</span>`,
      );
    }
    q("#rl-month-grid").innerHTML = cells.join("");
  }

  // ── Highlights and notes ───────────────────────────────────────────────────
  //
  // Everything the book carries, on one page, the way a Kindle's own notebook
  // page shows it: the passages in reading order, each note under the passage it
  // annotates. Nothing here is editable — the reader owns that, and a record of
  // what was read has no business rewriting it.

  // The edge colour of a row: the colour the reader paints that mark, resolved
  // by the reader itself so the two can't disagree — including the yellow it
  // gives a highlight the device left colourless. Only a hex value is passed on,
  // because it lands in a `style` attribute and `color` arrives from a file the
  // device wrote; every colour a Kindle names is one, so the guard costs
  // nothing and an exotic literal simply goes unswatched.
  function noteColor(name) {
    const css = window.sidleReader?.highlightColor?.(name);
    return /^#[0-9a-f]{3,8}$/i.test(css || "") ? css : null;
  }

  // "Yellow highlight" — the colour is named only when the device named it, so a
  // literal or missing colour reads as a plain "Highlight" rather than echoing a
  // hex value at the reader.
  function noteKind(a) {
    if (a.kind === "highlight") {
      const named = a.color && window.sidleReader?.highlightColors?.[a.color];
      return named ? `${a.color[0].toUpperCase()}${a.color.slice(1)} highlight` : "Highlight";
    }
    if (a.kind === "note") return "Note";
    if (a.kind === "bookmark") return "Bookmark";
    return a.kind;
  }

  // When the Kindle says the mark was made. The year is shown only when it is
  // not this one, so the common case stays short. Blank for a row imported
  // before Sidle kept the stamp — no date beats the import date pretending to
  // be one.
  function noteWhen(iso) {
    if (!iso) return "";
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return "";
    const sameYear = d.getFullYear() === new Date().getFullYear();
    return d.toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      ...(sameYear ? {} : { year: "numeric" }),
    });
  }

  // The notes written against one highlight. A highlight can carry several —
  // one added in Sidle, others on the Kindle — and they belong under the passage
  // rather than as entries of their own. A hidden one is left out here as it is
  // anywhere else; the count beside the heading is what says it exists.
  function notesOn(a) {
    return state.notes.filter((n) => n.attached_to === a.id && !n.hidden);
  }

  // "43 highlights · 2 notes". Bookmarks are counted separately and only when
  // there are any: on most books the word would just be a zero.
  function noteCounts(rows, hidden) {
    const parts = [];
    for (const kind of ["highlight", "note", "bookmark"]) {
      const n = rows.filter((a) => a.kind === kind).length;
      if (n) parts.push(`${n} ${kind}${n === 1 ? "" : "s"}`);
    }
    // Hidden rows are curated out of the reader, so they are curated out here
    // too — but silently dropping them would make the count contradict the book.
    if (hidden) parts.push(`${hidden} hidden`);
    return parts.join(" · ");
  }

  function noteRow(a) {
    const color = a.kind === "highlight" ? noteColor(a.color) : null;
    const style = color ? ` style="--rl-note-color: ${esc(color)}"` : "";
    const when = noteWhen(a.added_at);
    // The row's own body, then every note hanging off it.
    const bodies = [a.note_body, ...notesOn(a).map((n) => n.note_body)].filter(Boolean);
    return (
      `<li class="rl-note rl-note-${esc(a.kind)}"${style}>` +
      `<div class="rl-note-head"><span class="rl-note-kind">${esc(noteKind(a))}</span>` +
      `<span>${esc(when)}</span></div>` +
      // A bookmark marks a place and quotes nothing; so does a highlight whose
      // text the sidecar could not resolve. Either way there is no quote to draw.
      (a.text ? `<p class="rl-note-text">${esc(a.text)}</p>` : "") +
      bodies.map((b) => `<div class="rl-note-body">${esc(b)}</div>`).join("") +
      `</li>`
    );
  }

  function renderNotes() {
    const rows = state.notes.filter((a) => !a.hidden);
    // A note attached to a highlight is drawn inside that highlight's row, so it
    // is not also a row of its own.
    const listed = rows.filter((a) => a.attached_to == null);
    const hidden = state.notes.length - rows.length;
    q("#rl-notes-count").textContent = noteCounts(rows, hidden);
    // The hint says how highlights get here, so it belongs only to a book that
    // has none at all — a book whose every mark is hidden has them, and the
    // count beside the heading already says so. A failed query leaves the
    // section blank rather than telling the user a book carries nothing when
    // what actually happened was that nothing could be read.
    q("#rl-notes-empty").hidden = state.notes.length > 0 || state.notesFailed;
    q("#rl-notes-list").innerHTML = listed.map(noteRow).join("");
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

  // Erasing the whole log is not undoable, and mostly not recoverable either: a
  // Kindle sends only what is newer than the newest session stored, so what it
  // has already pushed it will not push again. The dialog says so and states
  // exactly what goes.
  async function doPurge() {
    const o = state.overview;
    const what = o
      ? `${fmtDuration(o.total_seconds)} across ${o.books_total} books and ${o.days_read} days`
      : "the whole reading log";
    if (
      !confirm(
        `Delete every reading session Sidle has stored?\n\nThis erases ${what}, ` +
          `for every Kindle.\n\nThis cannot be undone. A Kindle sends only what is ` +
          `newer than the newest session stored and clears its own copy at that ` +
          `mark, so reading that arrived from the device is gone with these rows. ` +
          `Only days a logbackup file still covers can be read again.`,
      )
    ) {
      return;
    }
    try {
      const gone = await api.invoke("reading_log_clear");
      toast(`reading log cleared — ${gone} sessions deleted`);
      state.day = null;
      state.book = null;
      state.year = null;
      invalidate();
      await refresh();
    } catch (e) {
      toast(`could not clear the reading log: ${e}`, true);
    }
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
      const r = await api.invoke("reading_log_import", { paths });
      // Appended to a success line rather than replacing one: a short file still
      // yields its prefix, so the import worked *and* something was incomplete.
      // Deliberately not "re-import to fix" — the usual cause is a backup the
      // Kindle itself never finished writing, which no re-import repairs. The
      // overlap between dumps normally covers the loss; saying so would be a
      // promise, and this is a note.
      const cut = r.truncated
        ? ` · ${r.truncated} file${r.truncated > 1 ? "s were" : " was"} incomplete on the Kindle`
        : "";
      if (r.conflict) {
        // The archive names a Kindle other than the one it was being filed
        // under. Nothing was stored — misfiled reading is indistinguishable
        // from correct reading once it is in.
        toast(`these logs are from ${r.conflict} — nothing imported`, true);
      } else if (r.cancelled) {
        // Both phases commit as they go, so a cancel keeps its work — say so,
        // or the user re-runs from scratch expecting to have lost it.
        toast("import stopped — what finished was kept, run it again to continue");
      } else if (!r.files && r.skipped) {
        // Every file was recognised and skipped unopened. Saying so is the
        // difference between "nothing happened" and "there was nothing to do".
        toast(`already imported — all ${r.skipped} files skipped`);
      } else if (!r.events) {
        toast("no reading events in those files — is this a logbackup folder?", true);
      } else if (!r.added) {
        toast(`already imported: ${r.sessions} sessions in ${r.files} files${cut}`);
      } else if (!r.attributed) {
        // Everything found is on books the library doesn't hold, so nothing was
        // counted — say so, or a successful import looks like a broken page.
        toast(`${r.added} sessions found, none on books in the library`, true);
      } else {
        // `attributed`, not `added`: time on a missing book is stored inert and
        // appears nowhere, so counting it here would promise rows that never show.
        const orphans = Math.max(0, r.added - r.attributed);
        const tail = orphans ? ` · ${orphans} on books not in the library` : "";
        const reused = r.skipped ? `, ${r.skipped} already imported` : "";
        toast(`${r.attributed} sessions added from ${r.files} files${reused}${tail}${cut}`);
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
    q("#rl-purge").addEventListener("click", doPurge);
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
    // Both navigations read their destination off the button, which the
    // renderer set from the data — so an arrow can only ever go somewhere that
    // exists, and a disabled one has nothing to go to.
    for (const sel of ["#rl-year-prev", "#rl-year-next"]) {
      q(sel).addEventListener("click", (e) => {
        const target = e.currentTarget.dataset.target;
        if (!target) return;
        state.year = Number(target);
        state.day = null;
        render();
      });
    }
    q("#rl-day-clear").addEventListener("click", () => {
      state.day = null;
      render();
    });

    // Sets `state.calView`. `state.year` and `state.day` are left as they are.
    q("#rl-cal-seg").addEventListener("click", (e) => {
      const btn = e.target.closest(".seg-btn[data-cal]");
      if (!btn || btn.dataset.cal === state.calView) return;
      state.calView = btn.dataset.cal;
      renderCalendar(state.overview);
    });

    for (const sel of ["#rl-cal-prev", "#rl-cal-next"]) {
      q(sel).addEventListener("click", (e) => {
        const target = e.currentTarget.dataset.target;
        if (!target) return;
        state.calMonth = new Date(+target.slice(0, 4), +target.slice(5, 7) - 1, 1);
        renderCalendar(state.overview);
      });
    }

    // Year / Month / Day. The window does not change — only how finely the
    // query cuts it — so the heatmap and the totals above stay put.
    q("#rl-bucket-seg").addEventListener("click", (e) => {
      const btn = e.target.closest(".seg-btn[data-bucket]");
      if (!btn || btn.dataset.bucket === state.bucket) return;
      state.bucket = btn.dataset.bucket;
      renderScope();
    });

    // Hour / Week / Month. Three cuts of one cube already in hand, so this
    // redraws the panel and asks the backend nothing.
    q("#rl-clock-seg").addEventListener("click", (e) => {
      const btn = e.target.closest(".seg-btn[data-clock]");
      if (!btn || btn.dataset.clock === state.clockView) return;
      state.clockView = btn.dataset.clock;
      if (state.overview) renderClock(state.overview);
    });

    // Sort: the gallery's control, over reading figures. Only the grid changes,
    // so this re-runs the scope query rather than the whole page.
    q("#rl-sort-button").addEventListener("click", () => {
      const pop = q("#rl-sort-popover");
      if (!pop.hidden) return closeSort();
      pop.hidden = false;
      positionPopover(pop, q("#rl-sort-button"));
    });
    q("#rl-sort-keys").addEventListener("click", (e) => {
      const li = e.target.closest("li[data-key]");
      if (!li) return;
      state.sort = { key: li.dataset.key, asc: state.sort.asc };
      closeSort();
      renderScope();
    });
    for (const btn of document.querySelectorAll("#rl-sort-popover .sort-dir-toggle button")) {
      btn.addEventListener("click", () => {
        state.sort = { ...state.sort, asc: btn.dataset.dir === "asc" };
        closeSort();
        renderScope();
      });
    }
    document.addEventListener("click", (e) => {
      if (
        !q("#rl-sort-popover").hidden &&
        !e.target.closest("#rl-sort-popover") &&
        !e.target.closest("#rl-sort-button")
      ) {
        closeSort();
      }
    });
    q("#rl-back").addEventListener("click", () => {
      state.book = null;
      render();
      // The grid keeps the height it had until `renderScope`'s reply replaces
      // it with the same rows, and a card's height is its cover's aspect ratio,
      // not a loaded image — so the offset is good to restore right now.
      scroller().scrollTop = state.overviewScroll;
    });
    for (const sel of ["#rl-prev", "#rl-next"]) {
      q(sel).addEventListener("click", (e) => {
        const target = e.currentTarget.dataset.target;
        if (!target) return;
        state.month = monthFromIndex(Number(target));
        renderMonth();
      });
    }

    // One delegated handler for the whole page: the heatmap and both lists are
    // re-rendered wholesale, so per-element listeners would leak on every draw.
    q("#reading-log").addEventListener("click", (e) => {
      // Matched before `.rl-cal-cell`, which a bar overlaps.
      const span = e.target.closest(".rl-cal-span[data-book], .rl-tl-block[data-book]");
      if (span) {
        openBook(Number(span.dataset.book));
        return;
      }
      const cell = e.target.closest(
        ".rl-cell[data-day], .rl-cal-cell[data-day], .rl-recent-bar[data-day]",
      );
      if (cell) {
        state.day = state.day === cell.dataset.day ? null : cell.dataset.day;
        render();
        return;
      }
      const row = e.target.closest(".rl-card[data-book]");
      if (row) {
        openBook(Number(row.dataset.book));
        return;
      }
      // `[role]` rather than the id alone: the renderer sets it only on a cover
      // that has a book behind it.
      if (e.target.closest("#rl-book-cover[role]")) {
        openInReader();
        return;
      }

      // Settling a tie: which candidate was clicked, in which row — the row
      // carries the position that is the group's whole identity.
      const tie = e.target.closest(".rl-ambiguous-row[data-position]");
      const pick = e.target.closest(".rl-pick-book[data-book]");
      if (tie && pick) nameBook(Number(tie.dataset.position), Number(pick.dataset.book));
    });

    q("#reading-log").addEventListener("keydown", (e) => {
      if (e.key !== "Enter" && e.key !== " ") return;
      const hit = e.target.closest(
        ".rl-cell[data-day], .rl-cal-cell[data-day], .rl-recent-bar[data-day], " +
          ".rl-cal-span[data-book], .rl-tl-block[data-book], " +
          ".rl-card[data-book], #rl-book-cover[role]",
      );
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
