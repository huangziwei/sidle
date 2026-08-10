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
    day: null, // selected YYYY-MM-DD within that year, or null
    books: [], // the grid: books of the selected day, else of the year
    bucket: "year", // how finely the grid cuts the year up: year | month | day
    sort: { key: "last", asc: false }, // most recently read first
    scope: 0, // guards the async grid fetch against a stale reply
    book: null, // { id, days, entry } when the book page is open
    notes: [], // that book's annotations, as `annotations_for_book` returns them
    notesFailed: false, // that query failed, so an empty list means "unknown"
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
    renderYearNav(o, years);
    renderHeatmap(o);
    renderScope();
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

  function renderStats(o) {
    const perDay = o.days_read ? Math.round(o.total_seconds / o.days_read) : 0;
    const tiles = [
      statTile(fmtDuration(o.total_seconds), "total"),
      statTile(o.days_read, "days read"),
      statTile(fmtDuration(perDay), "per reading day"),
      statTile(`${o.current_streak}d`, "streak", "Consecutive days up to today"),
      statTile(`${o.longest_streak}d`, "longest streak"),
      statTile(o.books_total, "books"),
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
        fact("Total", fmtDuration(entry.seconds)),
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
        // Blank for sessions imported before Sidle was told which Kindle wrote
        // them; a row saying nothing beats a row inventing a device.
        entry.devices.length ? fact("Read on", entry.devices.join(", ")) : "",
      ].join("");
    } else {
      q("#rl-book-stats").innerHTML = "";
    }
    renderMonth();
    renderNotes();
  }

  // One figure as a label/value row. A vertical list beside the calendar reads
  // better than a strip of boxes across the top, and scales as figures are
  // added without pushing anything off the edge.
  function fact(label, value, hint) {
    const t = hint ? ` title="${esc(hint)}"` : "";
    return `<div class="rl-fact"${t}><dt>${esc(label)}</dt><dd>${esc(value)}</dd></div>`;
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

  // Erasing the whole log is not undoable from inside Sidle — the way back is
  // re-importing the archives — so the dialog states exactly what goes.
  async function doPurge() {
    const o = state.overview;
    const what = o
      ? `${fmtDuration(o.total_seconds)} across ${o.books_total} books and ${o.days_read} days`
      : "the whole reading log";
    if (
      !confirm(
        `Delete every reading session Sidle has stored?\n\nThis erases ${what}, ` +
          `for every Kindle.\n\nThe logbackup files on disk are untouched — importing ` +
          `them again brings this back.`,
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

    // Year / Month / Day. The window does not change — only how finely the
    // query cuts it — so the heatmap and the totals above stay put.
    q("#rl-bucket-seg").addEventListener("click", (e) => {
      const btn = e.target.closest(".seg-btn[data-bucket]");
      if (!btn || btn.dataset.bucket === state.bucket) return;
      state.bucket = btn.dataset.bucket;
      renderScope();
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
