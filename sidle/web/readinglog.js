// Reading Log: what was read on which day, and for how long. An IIFE exposing
// `window.ReadingLog` ({ refresh, show, hide, invalidate, handleKey }), over
// commands/reading_log.rs and `annotations_for_book`.
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
    finishedOnly: false, // the grid keeps only books read to the end
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

  // `#main`, the one scroll container every section shares.
  const scroller = () => q("#main");

  function toast(msg, isError = false) {
    if (typeof window.showToast === "function") window.showToast(msg, isError);
    else if (isError) console.error(msg);
  }

  // ── Formatting ─────────────────────────────────────────────────────────────

  // Durations as "4h 12m" / "37m" / "2m".
  function fmtDuration(secs) {
    if (!secs || secs < 60) return "<1m";
    const h = Math.floor(secs / 3600);
    const m = Math.round((secs % 3600) / 60);
    if (h && m) return `${h}h ${m}m`;
    if (h) return `${h}h`;
    return `${m}m`;
  }

  // A "~" on a figure `dwell_seconds` or `awake_seconds` contributed to.
  // `dwell_seconds` is a measurement, `awake_seconds` a bound.
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

  // `measure` as the word a block's `title` carries. See
  // `library::reading_log::Measure`.
  function measureVerb(measure) {
    if (measure === "awake") return "awake with the book open";
    if (measure === "dwell") return "read, timed page by page";
    return "read";
  }

  // Word counts from the device's own `TotalWords` counter.
  function fmtWords(n) {
    if (!n) return "0";
    if (n >= 1000000) return `${(n / 1000000).toFixed(1)}M`;
    if (n >= 1000) return `${Math.round(n / 1000)}k`;
    return String(n);
  }

  // "Aug 9", from a full timestamp or a bare day.
  function shortDay(iso) {
    const d = parseDay((iso || "").slice(0, 10));
    return d ? d.toLocaleDateString(undefined, { month: "short", day: "numeric" }) : "";
  }

  // `shortDay` carrying the year, for a date outside the current one.
  function shortDayYear(iso) {
    const d = parseDay((iso || "").slice(0, 10));
    return d
      ? d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" })
      : "";
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

  // `YYYY-MM-DD` → local Date, built component-wise. `new Date(iso)` reads a
  // bare date as UTC.
  function parseDay(iso) {
    const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(iso || "");
    return m ? new Date(+m[1], +m[2] - 1, +m[3]) : null;
  }

  function dayKey(d) {
    const p = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
  }

  // Escapes text interpolated into the HTML strings this file builds.
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
    // `doPurge` and `nameBook` both reach `refresh`; `calRows` is
    // dropped for every one of them.
    state.calRows.clear();
    state.calShapes.clear();
    try {
      // `overview` and `ambiguous`: the named reading, and the ties.
      [state.overview, state.ambiguous] = await Promise.all([
        api.invoke("reading_log_overview"),
        api.invoke("reading_log_ambiguous"),
      ]);
      state.loaded = true;
    } catch (e) {
      // `state.loaded` is false on a throw, and `show()` calls `refresh()`.
      toast(`failed to load reading log: ${e}`, true);
      state.overview = null;
      state.ambiguous = [];
    }
    render();
  }

  function show() {
    if (!state.loaded) refresh();
    else render();
  }

  // A popover left open floats over the section replacing this one.
  function hide() {
    closeSort();
  }

  function invalidate() {
    state.loaded = false;
    if (!q("#reading-log").hidden) refresh();
  }

  // `handleKey` takes the reading log's bare keys, returning true where the key is
  // consumed. library.js routes here before its own bare keys.
  function handleKey(e) {
    if (!q("#rl-sort-popover").hidden) {
      if (e.key !== "Escape") return false;
      closeSort();
      return true;
    }
    switch (e.key) {
      case "r":
      case "R":
        refresh();
        return true;
      case "f":
      case "F":
        return press("#rl-finished");
      case "b":
      case "B":
        return cycleSeg("#rl-bucket-seg", "bucket", state.bucket);
      case "c":
      case "C":
        return cycleSeg("#rl-cal-seg", "cal", state.calView);
      case "s":
      case "S":
        return press("#rl-sort-button");
      case "ArrowLeft":
        return stepNav(-1);
      case "ArrowRight":
        return stepNav(1);
      case "Backspace":
        if (!state.book) return false;
        closeBook();
        return true;
      case "Escape":
        if (state.book) {
          closeBook();
          return true;
        }
        if (!state.day) return false;
        state.day = null;
        render();
        return true;
    }
    return false;
  }

  // `press` clicks the element at `sel`. A hidden or disabled `el` consumes the key and
  // clicks nothing.
  function press(sel) {
    const el = q(sel);
    if (!el.disabled && el.offsetParent !== null) el.click();
    return true;
  }

  // `cycleSeg` clicks the `.seg-btn` after `current`, wrapping at the end.
  function cycleSeg(sel, key, current) {
    const seg = q(sel);
    if (seg.offsetParent === null) return true;
    const btns = [...seg.querySelectorAll(`.seg-btn[data-${key}]`)];
    const i = btns.findIndex((b) => b.dataset[key] === current);
    btns[(i + 1) % btns.length]?.click();
    return true;
  }

  // `stepNav` picks the nav pair the page carries: a book's months, the month calendar,
  // else the year.
  function stepNav(dir) {
    const pair = state.book
      ? ["#rl-prev", "#rl-next"]
      : state.calView === "month"
        ? ["#rl-cal-prev", "#rl-cal-next"]
        : ["#rl-year-prev", "#rl-year-next"];
    return press(pair[dir < 0 ? 0 : 1]);
  }

  // `closeBook` restores `state.overviewScroll` once `render` rebuilds the overview.
  function closeBook() {
    state.book = null;
    render();
    scroller().scrollTop = state.overviewScroll;
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
    // `days` is empty for a library whose reading is all tied.
    renderAmbiguous();
    if (!has) {
      q("#rl-stats").innerHTML = "";
      return;
    }
    // The current year, falling back to the newest year with any reading.
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

  // The first three tiles read `state.year`. The rest name their window in
  // their `title`.
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
      statTile(o.finished_total, "finished", "Read to the end, or marked finished"),
    ];
    q("#rl-stats").innerHTML = tiles.join("");
  }

  // The arrows step to the next year in `years`.
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

  // A navigation arrow. `target` sits on the element for the click handler.
  function setStep(sel, target, title) {
    const btn = q(sel);
    btn.disabled = target === null;
    btn.dataset.target = target === null ? "" : String(target);
    btn.title = target === null ? "" : title(target);
  }

  // Scaled by the busiest day in `days`.
  function levelScale(days) {
    const peak = Math.max(...days.map((d) => d.seconds), 1);
    return (secs) => {
      if (!secs) return 0;
      const r = secs / peak;
      return r > 0.66 ? 4 : r > 0.4 ? 3 : r > 0.15 ? 2 : 1;
    };
  }

  // One column per week, Sunday at the top, from a fixed start.
  function renderHeatmap(o) {
    const totals = new Map(o.days.map((d) => [d.day, d.seconds]));
    const end = new Date(state.year, 11, 31);
    let start = new Date(state.year, 0, 1);
    start = new Date(start.getFullYear(), start.getMonth(), start.getDate() - start.getDay());

    // `levelScale` over every year.
    const level = levelScale(o.days);

    const cols = [];
    const months = [];
    // `d` walks a week at a time by mutation, and the binding itself never moves.
    for (const d = new Date(start); d <= end; d.setDate(d.getDate() + 7)) {
      const week = [];
      for (let i = 0; i < 7; i++) {
        const cur = new Date(d.getFullYear(), d.getMonth(), d.getDate() + i);
        // Only the days inside `state.year` belong to this grid.
        if (cur > end || cur.getFullYear() !== state.year) {
          week.push(`<i class="rl-cell rl-pad"></i>`);
          continue;
        }
        const key = dayKey(cur);
        const secs = totals.get(key) || 0;
        // `data-day` sits only on a day with reading.
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
  // The sittings of `state.day` on one 24-hour axis. A block runs `seconds`
  // from `started_at`. `ended_at` reaches the block's `title` and nothing else.

  const DAY_SECS = 86400;
  // Seconds held clear after a block in `packLanes`.
  const LANE_GAP_SECS = 60;

  // Seconds into the day, from a `YYYY-MM-DDTHH:MM:SS` stamp.
  function clockSecs(iso) {
    if (!iso || iso.length < 19) return null;
    return +iso.slice(11, 13) * 3600 + +iso.slice(14, 16) * 60 + +iso.slice(17, 19);
  }

  // `[start, end]` of `s`'s reading in seconds of its day: `seconds` from
  // `started_at`, ending at `DAY_SECS` at the latest.
  function sessionSpan(s) {
    const from = clockSecs(s.started_at);
    if (from == null) return null;
    return [from, Math.min(DAY_SECS, from + Math.max(s.seconds, 0))];
  }

  // Packs sittings into rows where no two overlap, earliest first.
  function packLanes(spans) {
    const lanes = [];
    for (const item of spans) {
      let lane = lanes.find((l) => l[l.length - 1].span[1] + LANE_GAP_SECS <= item.span[0]);
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
      .map((s) => ({ s, span: sessionSpan(s) }))
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
        const blocks = lane.map(
          ({ s, span }) =>
            `<span class="rl-tl-block" data-book="${s.book_id}" role="button" tabindex="0" ` +
            `style="left:${pct(span[0])}; width:${pct(span[1] - span[0])}; ` +
            `--fill:${bookFill(s.book_id)}; --ink:${bookInk(s.book_id)}" ` +
            `title="${esc(s.title)}\n${s.started_at.slice(11, 16)}–${s.ended_at.slice(11, 16)}` +
            ` open · ${fmtDuration(s.seconds)} ${measureVerb(s.measure)}">` +
            `${esc(s.title)}</span>`,
        );
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
  // `db::reading_clock` sends one (month, weekday, hour) cube for all time, and
  // every view here is a marginal of it.

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
    // `render` reaches this with days. A year whose every session predates the
    // stamps carries no cube.
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

  // "23:00–24:00" for hour 23.
  function hourSpan(h) {
    const p = (n) => String(n).padStart(2, "0");
    return `${p(h)}:00–${p(h + 1)}:00`;
  }

  // The year's hours as bars, each a fraction of the busiest hour.
  function clockBars(cells) {
    const hours = clockRows(cells, () => "").get("") || new Array(24).fill(0);
    const peak = Math.max(...hours, 1);
    const bars = hours
      .map((secs, h) => {
        // A zero bar draws nothing at all, down to the stub of colour.
        const v = secs / peak;
        return (
          `<div class="rl-bar" style="--v:${v.toFixed(4)}" ` +
          `title="${hourSpan(h)} — ${secs ? fmtDuration(secs) : "nothing"}">` +
          `<i></i></div>`
        );
      })
      .join("");
    // Every third hour: the axis stays legible at any panel width, each label
    // under the bar it names.
    const axis = hours
      .map((_, h) => `<span>${h % 3 === 0 ? String(h).padStart(2, "0") : ""}</span>`)
      .join("");
    return [
      `<div class="rl-bars">${bars}</div><div class="rl-bar-axis">${axis}</div>`,
      peakNote(hours.map((secs, h) => [hourSpan(h), secs])),
    ];
  }

  // Hour of the day against the weekday, or against the month: 24 columns on
  // the heatmap's own five-step ramp.
  function clockGrid(cells, view) {
    const byWeek = view === "week";
    const rows = clockRows(cells, (c) => (byWeek ? c.dow : c.month));
    const order = byWeek ? [...rows.keys()].sort((a, b) => a - b) : monthSpan(rows);
    const label = (k) => (byWeek ? DOW[k] : MONTHS[+k.slice(5, 7) - 1]);

    // `levelScale` over every cell of the grid, not per row.
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
        // A month inside the reading span with nothing in it draws as a row of
        // zeroes.
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

  // Every month from the first read to the last, gaps included.
  function monthSpan(rows) {
    const keys = [...rows.keys()].sort();
    const [lo, hi] = [+keys[0].slice(5, 7), +keys[keys.length - 1].slice(5, 7)];
    const year = keys[0].slice(0, 4);
    const out = [];
    for (let m = lo; m <= hi; m++) out.push(`${year}-${String(m).padStart(2, "0")}`);
    return out;
  }

  // "Most at Tue 22:00–23:00 · 4h 12m" — the figure behind the tallest bar.
  function peakNote(pairs) {
    let best = null;
    for (const [label, secs] of pairs) {
      if (secs > 0 && (!best || secs > best[1])) best = [label, secs];
    }
    return best ? `most at ${best[0]} · ${fmtDuration(best[1])}` : "";
  }

  // The gallery's cover markup. `coverUrlFor` (library.js) holds the
  // thumb-vs-full choice and the cache-busting token.
  function coverInner(url, title) {
    return url
      ? `<img src="${esc(url)}" alt="" loading="lazy" draggable="false">`
      : `<div class="cover-placeholder">${esc(title)}</div>`;
  }

  function coverHtml(e) {
    const url = coverUrlFor(e, { thumb: true });
    return `<div class="cover${url ? " has-image" : ""}">${coverInner(url, e.title)}</div>`;
  }

  // Every card is a book in the library, and every card opens its book page.
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

  // What the grid can be ordered by. Every one is a column of the aggregate,
  // ordered in SQL.
  const SORT_KEYS = [
    ["last", "Last read", "Most recently read first"],
    ["seconds", "Reading time", "Longest first"],
    ["sessions", "Sessions", "Most sittings first"],
    ["words", "Words", "Most words read first"],
  ];

  // The grid follows the heatmap: the whole year, or one clicked day. Figures
  // and the month/day bands both come from the windowed query.
  async function renderScope() {
    const day = state.day;
    const [from, to] = day ? [day, day] : [`${state.year}-01-01`, `${state.year}-12-31`];
    // A selected day is one band, which the header names.
    const bucket = day ? "total" : state.bucket;
    q("#rl-books-title").textContent = day ? fmtDay(day) : `Books in ${state.year}`;
    q("#rl-day-clear").hidden = !day;
    renderBucketControl();
    renderSortControl();

    // `state.gridPending` drops the reply of a superseded click.
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

    if (state.finishedOnly) rows = rows.filter((r) => r.finished);
    state.books = rows;
    const total = rows.reduce((a, r) => a + r.seconds, 0);
    q("#rl-finished").setAttribute("aria-pressed", String(state.finishedOnly));
    q("#rl-finished").classList.toggle("active", state.finishedOnly);
    q("#rl-books-total").textContent = rows.length
      ? fmtDuration(total)
      : state.finishedOnly
        ? "nothing finished"
        : "nothing read";
      // A band per day, with the time of day on the cards.
    const daily = !!day || bucket === "day";
    const banded = bucket === "month" || bucket === "day";
    const list = q("#rl-book-list");
    list.className = banded ? "rl-bands" : "rl-cards";
    list.innerHTML = banded ? bandsHtml(rows, bucket, daily) : cardsHtml(rows, daily);
  }

  // A day's cards carry `started_at`; a year's carry `last_read_at`, which the
  // default order sorts on.
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

  // Rows come back grouped in the asked-for direction, and one pass keeps that order.
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

  // "August", or "August 9". The header above every band carries the year.
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
  // Two books of identical length end at one position, and
  // `reading_log_ambiguous` sends only those ties.

  // Where reading stopped identifies a group and keys every action. An empty
  // list takes no early return, leaving a settled tie drawn.
  function renderAmbiguous() {
    const groups = state.ambiguous;
    const secs = groups.reduce((a, g) => a + g.seconds, 0);
    q("#rl-ambiguous").hidden = groups.length === 0;
    q("#rl-ambiguous-total").textContent = groups.length
      ? `${fmtDuration(secs)} · ${groups.length} to settle`
      : "";
    q("#rl-ambiguous-list").innerHTML = groups.map(groupRow).join("");
  }

  // "8m · 2 sessions · Jun 22 – Jun 23" — the reading a tie is claiming.
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

  // The candidates sit on the row from the start.
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

  // A candidate: the cover at a size a book is recognisable at, its title and
  // author under it.
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

  // Settles one tie and reloads the page.
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
    // `state.overviewScroll` is read at the click, ahead of the reply.
    const from = scroller().scrollTop;
    try {
      // `annotations_for_book` is the reader's own query. A failure is reported
      // and leaves the reading history drawn.
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

  // Into the book from the cover, through the gallery's `openReader`
  // (library.js).
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
      // `role` on the cover, matched by the click and key handlers.
      box.setAttribute("role", "button");
      box.setAttribute("tabindex", "0");
      box.setAttribute("aria-label", `Open ${entry.title} in the reader`);
      box.title = "Open in the reader";

      const span = spanDays(days);
      const perDay = days.length ? Math.round(entry.seconds / days.length) : 0;
      // Pace from the device's own word counts.
      const wpm = entry.seconds > 0 ? Math.round((entry.words * 60) / entry.seconds) : 0;
      q("#rl-book-stats").innerHTML = [
        factHtml("Total", `${esc(fmtDuration(entry.seconds))}${estimateMark(entry)}`),
        fact("Days read", days.length),
        fact("Per day", fmtDuration(perDay)),
        fact("Days elapsed", span, "First to last day read"),
        fact("Sessions", entry.sessions),
        fact("Words", fmtWords(entry.words)),
        wpm ? fact("Words / min", wpm) : "",
      // `page_turns` counts forward taps at whatever font size the device was
      // on, and is not a page count.
        fact(
          "Page turns",
          entry.page_turns,
          "Forward page turns on the device — depends on font size, not a page count",
        ),
        fact("First read", shortDay(entry.first_at)),
        fact("Last read", shortDay(entry.last_at)),
        ...paceFacts(entry, days),
        // Blank where `device_serial` is unset.
        entry.devices.length ? fact("Read on", entry.devices.join(", ")) : "",
      ].join("");
    } else {
      for (const a of ["role", "tabindex", "aria-label", "title"]) box.removeAttribute(a);
      q("#rl-book-stats").innerHTML = "";
    }
    renderProgress();
    renderFinishMark();
    renderMonth();
    renderNotes();
  }

  // A fraction rounding to 100%, the rounding `renderProgress` prints.
  function isFinished(frac) {
    return frac != null && Math.round(frac * 100) >= 100;
  }

  // `db::progress_fraction` of the open book, or null where either half is
  // unstored.
  function readFraction() {
    const p = state.book?.progress;
    return p ? p.fraction : null;
  }

  function renderProgress() {
    const frac = readFraction();
    const box = q("#rl-book-progress");
    box.hidden = frac === null;
    if (frac === null) return;
    const pct = Math.round(frac * 100);
    q("#rl-book-progress-fill").style.width = `${(frac * 100).toFixed(1)}%`;
    q("#rl-book-progress-label").textContent =
      isFinished(frac) ? "At the end" : `${pct}% of the way in`;
    q("#rl-book-progress-label").title =
      `Position ${state.book.progress.linear_pos} of ${state.book.progress.max_position}` +
      ` (${state.book.progress.source})`;
  }

  // `#rl-book-finished` shows where `readFraction` is short of the end.
  function renderFinishMark() {
    const btn = q("#rl-book-finished");
    const b = state.book;
    const atEnd = isFinished(readFraction());
    btn.hidden = !b || atEnd;
    if (btn.hidden) return;
    const marked = !!b.finished_at;
    btn.classList.toggle("active", marked);
    btn.setAttribute("aria-pressed", String(marked));
    btn.textContent = marked ? "Finished" : "Mark finished";
    btn.title = marked
      ? `Marked on ${shortDay(b.finished_at)} — click to unmark`
      : "Count this book as read, whatever the last position says";
  }

  async function toggleFinishMark() {
    const b = state.book;
    if (!b) return;
    const want = !b.finished_at;
    try {
      await api.invoke("reading_log_set_finished", { bookId: b.id, finished: want });
    } catch (e) {
      return toast(`could not mark the book: ${e}`, true);
    }
    b.finished_at = want ? new Date().toISOString() : null;
    b.finished = want || isFinished(readFraction());
    renderFinishMark();
  }

  // The `readFraction` floor `paceFacts` projects from.
  const PACE_FLOOR = 0.05;

  function paceFacts(entry, days) {
    const frac = readFraction();
    if (frac === null || frac < PACE_FLOOR || frac >= 1 || !entry.seconds || !days.length) {
      return [];
    }
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
          end.getFullYear() === new Date().getFullYear()
            ? shortDay(dayKey(end))
            : shortDayYear(dayKey(end)),
          `${daysLeft} reading day${daysLeft === 1 ? "" : "s"} at the current pace`,
        ),
      );
    }
    return out;
  }

  // One figure as a label/value row.
  function fact(label, value, hint) {
    return factHtml(label, esc(value), hint);
  }

  // The same, for a value carrying markup of its own.
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

  // Months as a single ordinal, comparable without date objects.
  function monthIndex(y, m) {
    return y * 12 + m;
  }

  function monthFromIndex(i) {
    return new Date(Math.floor(i / 12), i % 12, 1);
  }

  // The calendar reaches no further than `months`, one month at a time,
  // skipping none.
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
    // `levelScale` over this book's own `days`.
    const level = levelScale(days);

    const cells = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"].map(
      (d) => `<span class="rl-mday-head">${d}</span>`,
    );
    // A lead-in blank carries no level class; a day with no reading carries `rl-l0`.
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
  // The passages in reading order, each note under the passage it annotates.

  // The edge colour of a row, through `window.sidleReader.highlightColor`.
  // Only a hex value passes into the `style` attribute.
  function noteColor(name) {
    const css = window.sidleReader?.highlightColor?.(name);
    return /^#[0-9a-f]{3,8}$/i.test(css || "") ? css : null;
  }

  // "Yellow highlight". A literal or missing `color` reads as "Highlight".
  function noteKind(a) {
    if (a.kind === "highlight") {
      const named = a.color && window.sidleReader?.highlightColors?.[a.color];
      return named ? `${a.color[0].toUpperCase()}${a.color.slice(1)} highlight` : "Highlight";
    }
    if (a.kind === "note") return "Note";
    if (a.kind === "bookmark") return "Bookmark";
    return a.kind;
  }

  // `added_at` from the Kindle, carrying the year outside this one, else blank.
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

  // The notes against one highlight, under the passage. A hidden one is left
  // out of the list and held in the count.
  function notesOn(a) {
    return state.notes.filter((n) => n.attached_to === a.id && !n.hidden);
  }

  // "43 highlights · 2 notes". Bookmarks count separately, and only when a book
  // carries some.
  function noteCounts(rows, hidden) {
    const parts = [];
    for (const kind of ["highlight", "note", "bookmark"]) {
      const n = rows.filter((a) => a.kind === kind).length;
      if (n) parts.push(`${n} ${kind}${n === 1 ? "" : "s"}`);
    }
    // `hidden` rows are counted here and drawn nowhere.
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
      // A bookmark quotes nothing, as does a highlight whose `text` is empty.
      (a.text ? `<p class="rl-note-text">${esc(a.text)}</p>` : "") +
      bodies.map((b) => `<div class="rl-note-body">${esc(b)}</div>`).join("") +
      `</li>`
    );
  }

  function renderNotes() {
    const rows = state.notes.filter((a) => !a.hidden);
    // A row with `attached_to` set is drawn inside that highlight's row.
    const listed = rows.filter((a) => a.attached_to == null);
    const hidden = state.notes.length - rows.length;
    q("#rl-notes-count").textContent = noteCounts(rows, hidden);
    // `#rl-notes-empty` shows for a book with no rows at all. `state.notesFailed`
    // leaves the section blank.
    q("#rl-notes-empty").hidden = state.notes.length > 0 || state.notesFailed;
    q("#rl-notes-list").innerHTML = listed.map(noteRow).join("");
  }

  // A Kindle sends only what is newer than the newest session stored. The
  // dialog states what goes.
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

  // ── Wiring ─────────────────────────────────────────────────────────────────

  function init() {
    q("#rl-refresh").addEventListener("click", refresh);
    q("#rl-book-finished").addEventListener("click", toggleFinishMark);
    q("#rl-finished").addEventListener("click", () => {
      state.finishedOnly = !state.finishedOnly;
      renderScope();
    });
    q("#rl-purge").addEventListener("click", doPurge);
    // Both navigations read `target` off the button, which the renderer set
    // from the data.
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

    // Year / Month / Day: `state.bucket` alone, cutting the same window.
    q("#rl-bucket-seg").addEventListener("click", (e) => {
      const btn = e.target.closest(".seg-btn[data-bucket]");
      if (!btn || btn.dataset.bucket === state.bucket) return;
      state.bucket = btn.dataset.bucket;
      renderScope();
    });

    // Hour / Week / Month: three cuts of the cube in hand.
    q("#rl-clock-seg").addEventListener("click", (e) => {
      const btn = e.target.closest(".seg-btn[data-clock]");
      if (!btn || btn.dataset.clock === state.clockView) return;
      state.clockView = btn.dataset.clock;
      if (state.overview) renderClock(state.overview);
    });

    // Sort: re-runs the scope query alone.
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
    q("#rl-back").addEventListener("click", closeBook);
    for (const sel of ["#rl-prev", "#rl-next"]) {
      q(sel).addEventListener("click", (e) => {
        const target = e.currentTarget.dataset.target;
        if (!target) return;
        state.month = monthFromIndex(Number(target));
        renderMonth();
      });
    }

    // One delegated `click` handler for `#reading-log`.
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
      // `[role]` beside the id: set on a cover with a book behind it.
      if (e.target.closest("#rl-book-cover[role]")) {
        openInReader();
        return;
      }

      // Settling a tie: the clicked candidate, and the row's `data-position`.
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

  window.ReadingLog = { refresh, show, hide, invalidate, handleKey };
})();
