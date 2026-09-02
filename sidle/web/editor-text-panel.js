import {
  elementAddressAt,
  findAll,
  langOf,
  lineAt,
  lineStart,
  renderLines,
  replaceAll,
  snippetAround,
} from "./editor-text.js";
import { makePreview, mimeOf } from "./editor-preview.js";
import { inspect } from "./editor-style.js";

const GROUPS = [
  ["Text", ["text", "nav"]],
  ["Styles", ["style"]],
  ["Images", ["image"]],
  ["Fonts", ["font"]],
  ["Other", ["opf", "ncx", "container", "audio", "video", "other"]],
];

const ROLE_BY_EXT = {
  xhtml: "text", html: "text", htm: "text", css: "style", jpg: "image", jpeg: "image", png: "image",
  gif: "image", webp: "image", svg: "image", ttf: "font", otf: "font", woff: "font", woff2: "font",
};

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
}

function button(cls, text, onClick, title) {
  const b = el("button", cls, text);
  b.type = "button";
  if (title) b.title = title;
  b.addEventListener("click", onClick);
  return b;
}

function basename(path) {
  return path.slice(path.lastIndexOf("/") + 1);
}

function fmtBytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

function b64ToBytes(b64) {
  return Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
}

export function mountTextPanel({ bookId, center, toast, onDirty, onSaved }) {
  const api = window.api;
  const st = {
    members: [],
    opf: null,
    buffers: new Map(),
    current: null,
    previewDoc: null,
    tab: "preview",
    findings: null,
    imageUrls: new Map(),
    timers: {},
    destroyed: false,
  };

  const root = el("div", "tx-panel");
  const files = el("aside", "tx-files");
  const filesHead = el("div", "tx-files-head");
  const filter = el("input", "tx-filter");
  filter.placeholder = "Filter files";
  filter.addEventListener("input", renderFiles);
  filesHead.append(filter, button("btn btn-small", "+", newFile, "New file"));
  const filesList = el("div", "tx-files-list");
  files.append(filesHead, filesList);

  const edit = el("section", "tx-edit");
  const toolbar = el("div", "tx-toolbar");
  const pathLabel = el("span", "tx-path");
  const posLabel = el("span", "tx-pos");
  const gotoBtn = button("btn btn-small", "Go to line…", promptGoToLine, "⌘G");
  const findBtn = button("btn btn-small", "Find…", () => showTab("search", true), "⌘F");
  toolbar.append(pathLabel, posLabel, findBtn, gotoBtn);
  const editor = el("div", "tx-editor");
  const hl = el("pre", "tx-hl");
  hl.setAttribute("aria-hidden", "true");
  const ta = el("textarea", "tx-ta");
  ta.spellcheck = false;
  ta.autocapitalize = "off";
  ta.autocomplete = "off";
  ta.wrap = "soft";
  editor.append(hl, ta);
  const imageView = el("div", "tx-image");
  imageView.hidden = true;
  const empty = el("div", "tx-empty editor-muted", "Loading the book…");
  edit.append(toolbar, editor, imageView, empty);

  const side = el("section", "tx-side");
  const tabs = el("div", "tx-tabs");
  const tabBodies = {};
  for (const [id, label] of [["preview", "Preview"], ["style", "Style"], ["findings", "Findings"], ["search", "Search"]]) {
    const b = button("tx-tab", label, () => showTab(id));
    b.dataset.tab = id;
    tabs.append(b);
    tabBodies[id] = el("div", `tx-tab-body tx-${id}`);
    tabBodies[id].hidden = true;
  }
  const iframe = document.createElement("iframe");
  iframe.className = "tx-preview-frame";
  iframe.title = "Preview";
  tabBodies.preview.append(iframe);
  side.append(tabs, ...Object.values(tabBodies));

  root.append(files, edit, side);
  center.replaceChildren(root);
  center.classList.add("editor-center-text");

  const preview = makePreview({
    iframe,
    readBytes: (path) => api.invoke("editor_text_read_bytes", { bookId, member: path }).then(b64ToBytes),
    readText: async (path) => (await buffer(path)).text,
    onClick: ({ line, index, href }) => {
      if (href) {
        const target = href.split("#")[0];
        if (target && st.members.some((m) => m.path === target)) {
          open(target);
          return;
        }
      }
      if (st.previewDoc && st.current !== st.previewDoc) open(st.previewDoc, line);
      else goToLine(line, index);
    },
  });

  buildSearchTab();
  showTab("preview");

  ta.addEventListener("input", () => {
    const buf = st.buffers.get(st.current);
    if (!buf) return;
    buf.text = ta.value;
    buf.dirty = buf.text !== buf.saved;
    reportDirty();
    schedule("hl", 30, highlightNow);
    if (affectsPreview(st.current)) schedule("preview", 250, renderPreview);
    if (buf.dirty) markFileDirty(st.current);
  });
  ta.addEventListener("scroll", syncScroll);
  ta.addEventListener("keydown", (e) => {
    if (e.key === "Tab" && !e.metaKey && !e.ctrlKey) {
      e.preventDefault();
      insertAtCursor("  ");
    } else if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "g") {
      e.preventDefault();
      promptGoToLine();
    }
  });
  for (const ev of ["keyup", "click", "select"]) ta.addEventListener(ev, () => schedule("cursor", 120, cursorMoved));

  load();

  function schedule(key, ms, fn) {
    clearTimeout(st.timers[key]);
    st.timers[key] = setTimeout(() => {
      if (!st.destroyed) fn();
    }, ms);
  }

  async function load() {
    let data;
    try {
      data = await api.invoke("editor_text_open", { bookId });
    } catch (err) {
      empty.textContent = `Couldn't open the book's files: ${err}`;
      return;
    }
    if (st.destroyed) return;
    st.members = data.members;
    st.opf = data.opf_path;
    renderFiles();
    const first = spineMembers()[0] || st.members.find((m) => m.text);
    if (first) open(first.path);
    else empty.textContent = "This book has no text members.";
    validate();
  }

  function spineMembers() {
    return st.members.filter((m) => m.spine_index != null).sort((a, b) => a.spine_index - b.spine_index);
  }

  function renderFiles() {
    const q = filter.value.trim().toLowerCase();
    filesList.replaceChildren();
    const spine = spineMembers();
    for (const [title, roles] of GROUPS) {
      let list = title === "Text"
        ? spine.concat(st.members.filter((m) => roles.includes(m.role) && m.spine_index == null))
        : st.members.filter((m) => roles.includes(m.role) && m.spine_index == null);
      if (q) list = list.filter((m) => m.path.toLowerCase().includes(q) || (m.label || "").toLowerCase().includes(q));
      if (!list.length) continue;
      filesList.append(el("div", "tx-files-group", `${title} (${list.length})`));
      for (const m of list) {
        const b = button("tx-file", null, () => open(m.path));
        b.dataset.path = m.path;
        b.classList.toggle("active", m.path === st.current);
        const buf = st.buffers.get(m.path);
        if (buf?.dirty) b.classList.add("dirty");
        const name = el("span", "tx-file-name", m.label || basename(m.path));
        b.append(name);
        if (m.label) b.append(el("span", "tx-file-sub", basename(m.path)));
        b.append(el("span", "tx-file-size", fmtBytes(buf ? new TextEncoder().encode(buf.text).length : m.size)));
        b.title = m.path;
        filesList.append(b);
      }
    }
  }

  function markFileDirty(path) {
    const b = filesList.querySelector(`.tx-file[data-path="${CSS.escape(path)}"]`);
    if (b) b.classList.add("dirty");
  }

  function reportDirty() {
    onDirty([...st.buffers.values()].some((b) => b.dirty));
  }

  async function buffer(path) {
    let buf = st.buffers.get(path);
    if (buf) return buf;
    const text = await api.invoke("editor_text_read", { bookId, member: path });
    buf = st.buffers.get(path);
    if (buf) return buf;
    buf = { text, saved: text, dirty: false, isNew: false };
    st.buffers.set(path, buf);
    return buf;
  }

  async function open(path, line, col) {
    const m = st.members.find((x) => x.path === path);
    if (!m) return;
    st.current = path;
    pathLabel.textContent = path;
    for (const b of filesList.querySelectorAll(".tx-file")) b.classList.toggle("active", b.dataset.path === path);
    empty.hidden = true;
    if (!m.text) {
      editor.hidden = true;
      imageView.hidden = false;
      await showImage(path);
      return;
    }
    imageView.hidden = true;
    editor.hidden = false;
    let buf;
    try {
      buf = await buffer(path);
    } catch (err) {
      toast(`Couldn't read ${basename(path)}: ${err}`, true);
      return;
    }
    if (st.current !== path || st.destroyed) return;
    ta.value = buf.text;
    highlightNow();
    ta.scrollTop = 0;
    syncScroll();
    if (langOf(path) === "xml" && m.role !== "opf" && m.role !== "ncx") {
      st.previewDoc = path;
      schedule("preview", 0, renderPreview);
    }
    if (line) goToLine(line, 0, col);
    else {
      ta.setSelectionRange(0, 0);
      cursorMoved();
    }
    if (st.tab === "style") schedule("cursor", 0, cursorMoved);
  }

  async function showImage(path) {
    imageView.replaceChildren(el("div", "editor-muted", "Loading…"));
    let url = st.imageUrls.get(path);
    if (!url) {
      try {
        const bytes = b64ToBytes(await api.invoke("editor_text_read_bytes", { bookId, member: path }));
        url = URL.createObjectURL(new Blob([bytes], { type: mimeOf(path) }));
        st.imageUrls.set(path, url);
      } catch (err) {
        imageView.replaceChildren(el("div", "editor-notice", `Couldn't read ${basename(path)}: ${err}`));
        return;
      }
    }
    if (st.current !== path) return;
    const m = st.members.find((x) => x.path === path);
    if (m.role === "image") {
      const img = el("img");
      img.src = url;
      img.alt = basename(path);
      imageView.replaceChildren(img, el("div", "editor-muted", `${path} · ${fmtBytes(m.size)}`));
    } else {
      imageView.replaceChildren(el("div", "editor-muted", `${path} · ${m.media_type || m.role} · ${fmtBytes(m.size)}`));
    }
  }

  function highlightNow() {
    hl.innerHTML = renderLines(ta.value, langOf(st.current || ""));
    syncScroll();
  }

  function syncScroll() {
    hl.scrollTop = ta.scrollTop;
    hl.scrollLeft = ta.scrollLeft;
  }

  function insertAtCursor(text) {
    const s = ta.selectionStart;
    const e = ta.selectionEnd;
    if (!document.execCommand || !document.execCommand("insertText", false, text)) {
      ta.setRangeText(text, s, e, "end");
      ta.dispatchEvent(new Event("input"));
    }
  }

  function cursorMoved() {
    if (!st.current || editor.hidden) return;
    const pos = ta.selectionStart;
    const line = lineAt(ta.value, pos);
    const col = pos - lineStart(ta.value, line) + 1;
    posLabel.textContent = `Ln ${line}, Col ${col}`;
    hl.querySelector(".line.current")?.classList.remove("current");
    hl.children[line - 1]?.classList.add("current");
    if (st.current !== st.previewDoc) return;
    const addr = elementAddressAt(ta.value, pos);
    if (st.tab === "preview") preview.scrollToLine(addr.line, addr.index);
    if (st.tab === "style") renderStyle(addr);
  }

  function goToLine(line, index = 0, col) {
    const text = ta.value;
    const start = lineStart(text, line);
    const pos = col ? Math.min(start + col - 1, text.length) : start;
    ta.focus();
    ta.setSelectionRange(pos, pos);
    scrollToLine(line);
    posLabel.textContent = `Ln ${line}, Col ${pos - start + 1}`;
    hl.querySelector(".line.current")?.classList.remove("current");
    hl.children[line - 1]?.classList.add("current");
    if (st.current === st.previewDoc && st.tab === "preview") preview.scrollToLine(line, index);
    if (st.tab === "style") renderStyle({ line, index });
  }

  function scrollToLine(line) {
    const row = hl.children[line - 1];
    if (!row) return;
    ta.scrollTop = Math.max(0, row.offsetTop - ta.clientHeight / 3);
    syncScroll();
  }

  function selectRange(start, end) {
    ta.focus();
    ta.setSelectionRange(start, end);
    scrollToLine(lineAt(ta.value, start));
    cursorMoved();
  }

  function promptGoToLine() {
    const v = prompt("Go to line");
    const n = parseInt(v, 10);
    if (n > 0) goToLine(n);
  }

  function affectsPreview(path) {
    if (!st.previewDoc) return false;
    return path === st.previewDoc || langOf(path) === "css";
  }

  async function renderPreview() {
    if (!st.previewDoc) return;
    const buf = st.buffers.get(st.previewDoc);
    if (!buf) return;
    try {
      await preview.render(st.previewDoc, buf.text);
    } catch (err) {
      console.error("preview", err);
      return;
    }
    if (st.destroyed) return;
    if (st.current === st.previewDoc) {
      const addr = elementAddressAt(ta.value, ta.selectionStart);
      if (st.tab === "preview") preview.scrollToLine(addr.line, addr.index);
      if (st.tab === "style") renderStyle(addr);
    }
  }

  function showTab(id, focusSearch) {
    st.tab = id;
    for (const b of tabs.children) b.classList.toggle("active", b.dataset.tab === id);
    for (const [k, body] of Object.entries(tabBodies)) body.hidden = k !== id;
    if (id === "findings") renderFindings();
    if (id === "style" || id === "preview") schedule("cursor", 0, cursorMoved);
    if (id === "search" && focusSearch) tabBodies.search.querySelector("input")?.focus();
  }

  function renderStyle(addr) {
    const body = tabBodies.style;
    if (st.current !== st.previewDoc) {
      body.replaceChildren(el("div", "editor-muted", "Open a content document to inspect its styles."));
      return;
    }
    const target = preview.elementAt(addr.line, addr.index);
    const info = inspect(target);
    if (!info) {
      body.replaceChildren(el("div", "editor-muted", `No element on line ${addr.line} in the preview.`));
      return;
    }
    body.replaceChildren();
    for (const node of info.nodes) {
      const box = el("div", "tx-style-node");
      const head = el("div", "tx-style-tag");
      const sel = node.tag + (node.id ? `#${node.id}` : "") + (node.classes ? "." + node.classes.trim().split(/\s+/).join(".") : "");
      head.append(el("span", null, sel));
      if (node.lnum) {
        head.append(button("tx-link", `line ${node.lnum}`, () => goToLine(node.lnum)));
      }
      if (node.depth) head.append(el("span", "tx-style-depth", "inherited"));
      box.append(head);
      if (!node.rules.length) box.append(el("div", "editor-muted", "No matching rules."));
      for (const r of node.rules) {
        const rule = el("div", "tx-rule");
        const top = el("div", "tx-rule-head");
        const src = r.member ? basename(r.member) : r.inline ? `inline, line ${r.inline}` : "style element";
        const selBtn = button("tx-rule-sel", r.selector || "style=\"…\"", () => openRule(r));
        top.append(selBtn, el("span", "tx-rule-src", src));
        rule.append(top);
        const decls = el("div", "tx-decls");
        for (const d of r.declarations) {
          decls.append(el("div", "tx-decl", `${d.prop}: ${d.value}${d.important ? " !important" : ""};`));
        }
        rule.append(decls);
        box.append(rule);
      }
      body.append(box);
    }
    const computed = el("details", "tx-computed");
    computed.append(el("summary", null, "Computed"));
    const table = el("div", "tx-decls");
    for (const [p, v] of info.computed) table.append(el("div", "tx-decl", `${p}: ${v}`));
    computed.append(table);
    body.append(computed);
  }

  async function openRule(r) {
    if (r.member) {
      const buf = await buffer(r.member);
      await open(r.member);
      const hit = r.selector ? findAll(buf.text, r.selector.replace(/\s+/g, " "), {})[0] || findAll(buf.text, r.selector.split(/\s*[>+~]\s*|\s+/).pop(), {})[0] : null;
      if (hit) selectRange(hit.start, hit.end);
    } else if (r.inline && st.previewDoc) {
      await open(st.previewDoc, r.inline);
    }
  }

  async function validate() {
    try {
      st.findings = await api.invoke("editor_text_validate", { bookId });
    } catch (err) {
      st.findings = [{ severity: "error", rule: "validator", message: String(err), location: "", member: null }];
    }
    if (st.destroyed) return;
    updateFindingsTab();
    if (st.tab === "findings") renderFindings();
  }

  function updateFindingsTab() {
    const b = tabs.querySelector('[data-tab="findings"]');
    if (!st.findings) {
      b.textContent = "Findings";
      return;
    }
    const errors = st.findings.filter((f) => f.severity === "error").length;
    const warnings = st.findings.filter((f) => f.severity === "warning").length;
    b.textContent = errors || warnings ? `Findings (${errors ? `${errors}!` : ""}${errors && warnings ? " " : ""}${warnings ? `${warnings}?` : ""})` : "Findings ✓";
  }

  function renderFindings() {
    const body = tabBodies.findings;
    body.replaceChildren();
    if (!st.findings) {
      body.append(el("div", "editor-muted", "Validating…"));
      return;
    }
    const head = el("div", "tx-findings-head");
    head.append(el("span", "editor-muted", `${st.findings.length} finding${st.findings.length === 1 ? "" : "s"}`));
    head.append(button("btn btn-small", "Re-check", () => {
      st.findings = null;
      renderFindings();
      validate();
    }));
    body.append(head);
    if (!st.findings.length) {
      body.append(el("div", "editor-muted", "The validator reports nothing."));
      return;
    }
    const groups = new Map();
    for (const f of st.findings) {
      const key = f.member || "Book";
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(f);
    }
    for (const [member, list] of groups) {
      body.append(el("div", "tx-files-group", member === "Book" ? "Book" : basename(member)));
      for (const f of list) {
        const row = button("tx-finding", null, () => {
          if (f.member) open(f.member, f.line || 1);
        });
        row.append(el("span", `tx-sev tx-sev-${f.severity}`, f.severity === "error" ? "E" : f.severity === "warning" ? "W" : "i"));
        const text = el("span", "tx-finding-text");
        text.append(el("span", "tx-finding-msg", f.message));
        text.append(el("span", "tx-finding-loc", `${f.rule}${f.line ? ` · line ${f.line}` : ""}${f.fix_detail ? ` · ${f.fix_detail}` : ""}`));
        row.append(text);
        body.append(row);
        if (f.fix_action === "restore-styles") body.append(restoreStylesAction());
      }
    }
  }

  function restoreStylesAction() {
    const box = el("div", "tx-fix");
    const select = el("select", "input-small");
    select.disabled = true;
    const go = button("btn btn-small", "Restore", () => restoreStyles(Number(select.value)));
    go.disabled = true;
    box.append(el("span", "editor-muted", "Reference:"), select, go);
    api.invoke("editor_style_candidates", { bookId }).then((list) => {
      if (st.destroyed) return;
      if (!list.length) {
        select.replaceChildren(el("option", null, "No sibling book keeps the publisher's stylesheets"));
        return;
      }
      for (const c of list) {
        const o = el("option", null, c.series_index != null ? `${c.title} (${c.series_name} ${c.series_index})` : c.title);
        o.value = String(c.id);
        select.append(o);
      }
      select.disabled = false;
      go.disabled = false;
    }).catch((err) => {
      select.replaceChildren(el("option", null, `Couldn't list siblings: ${err}`));
    });
    return box;
  }

  async function restoreStyles(referenceId, force = false) {
    if (!Number.isFinite(referenceId)) return;
    if ([...st.buffers.values()].some((b) => b.dirty)) {
      toast("Save or revert your edits first.", true);
      return;
    }
    let res;
    try {
      res = await api.invoke("editor_restore_styles", { bookId, referenceId, force });
    } catch (err) {
      toast(`Restore failed: ${err}`, true);
      return;
    }
    if (st.destroyed) return;
    if (!res.report.written) {
      const changes = res.report.diffs
        .filter((d) => d.text)
        .slice(0, 6)
        .map((d) => `${basename(d.document)}: ${d.property} ${d.before} → ${d.after} (×${d.count})`)
        .join("\n");
      if (confirm(`${res.report.blocked || "The restoration changes computed styles."}\n\n${changes}\n\nApply anyway?`)) {
        await restoreStyles(referenceId, true);
      }
      return;
    }
    for (const m of st.members) preview.forget(m.path);
    st.buffers.clear();
    st.members = res.members;
    st.findings = res.findings;
    updateFindingsTab();
    renderFiles();
    reportDirty();
    onSaved(res);
    const current = st.members.some((m) => m.path === st.current) ? st.current : spineMembers()[0]?.path;
    if (current) await open(current);
    if (st.tab === "findings") renderFindings();
    const r = res.report;
    const diffs = r.diffs.reduce((n, d) => n + d.count, 0);
    toast(`Restored ${r.documents.length} file${r.documents.length === 1 ? "" : "s"} from “${r.reference}”` +
      `${r.residual.length ? `, ${r.residual.length} class${r.residual.length === 1 ? "" : "es"} kept with a residual rule` : ""}` +
      `${diffs ? `, ${diffs} computed-style change${diffs === 1 ? "" : "s"}` : ""} — regenerating the Kindle file…`);
  }

  function buildSearchTab() {
    const body = tabBodies.search;
    const form = el("div", "tx-search-form");
    const q = el("input", "tx-search-q");
    q.placeholder = "Find";
    const r = el("input", "tx-search-r");
    r.placeholder = "Replace with";
    const opts = el("div", "tx-search-opts");
    const scope = el("select", "input-small");
    for (const [v, t] of [["file", "This file"], ["all", "All files"]]) {
      const o = el("option", null, t);
      o.value = v;
      scope.append(o);
    }
    const regex = el("label", "tx-check");
    const regexBox = el("input");
    regexBox.type = "checkbox";
    regex.append(regexBox, el("span", null, "Regex"));
    const cs = el("label", "tx-check");
    const csBox = el("input");
    csBox.type = "checkbox";
    cs.append(csBox, el("span", null, "Match case"));
    opts.append(scope, regex, cs);
    const actions = el("div", "tx-search-actions");
    const results = el("div", "tx-search-results");
    const options = () => ({ regex: regexBox.checked, caseSensitive: csBox.checked });

    const next = () => {
      if (!st.current || editor.hidden) return;
      const hits = findAll(ta.value, q.value, options());
      if (!hits.length) {
        toast("No matches in this file.");
        return;
      }
      const from = ta.selectionEnd;
      let i = hits.findIndex((h) => h.start >= from);
      if (i < 0) i = 0;
      selectRange(hits[i].start, hits[i].end);
    };
    const replaceOne = () => {
      if (!st.current || editor.hidden) return;
      const hits = findAll(ta.value, q.value, options());
      const sel = hits.find((h) => h.start === ta.selectionStart && h.end === ta.selectionEnd);
      if (!sel) {
        next();
        return;
      }
      const rep = options().regex ? ta.value.slice(sel.start, sel.end).replace(new RegExp(q.value, csBox.checked ? "" : "i"), r.value) : r.value;
      ta.focus();
      ta.setSelectionRange(sel.start, sel.end);
      insertAtCursor(rep);
      next();
    };
    const replaceEverywhere = async () => {
      const targets = scope.value === "all" ? st.members.filter((m) => m.text) : st.members.filter((m) => m.path === st.current);
      let total = 0;
      let filesChanged = 0;
      for (const m of targets) {
        const buf = await buffer(m.path);
        const { text, count } = replaceAll(buf.text, q.value, r.value, options());
        if (!count) continue;
        total += count;
        filesChanged++;
        buf.text = text;
        buf.dirty = text !== buf.saved;
        markFileDirty(m.path);
        if (m.path === st.current) {
          const pos = ta.selectionStart;
          ta.value = text;
          ta.setSelectionRange(Math.min(pos, text.length), Math.min(pos, text.length));
          highlightNow();
        }
      }
      reportDirty();
      if (affectsPreview(st.current) || targets.some((m) => affectsPreview(m.path))) schedule("preview", 0, renderPreview);
      toast(total ? `Replaced ${total} match${total === 1 ? "" : "es"} in ${filesChanged} file${filesChanged === 1 ? "" : "s"}.` : "No matches.");
      runSearch();
    };
    const runSearch = async () => {
      results.replaceChildren();
      if (!q.value) return;
      const targets = scope.value === "all" ? st.members.filter((m) => m.text) : st.members.filter((m) => m.path === st.current);
      let total = 0;
      for (const m of targets) {
        let buf;
        try {
          buf = await buffer(m.path);
        } catch {
          continue;
        }
        const hits = findAll(buf.text, q.value, options());
        if (!hits.length) continue;
        total += hits.length;
        results.append(el("div", "tx-files-group", `${m.label || basename(m.path)} (${hits.length})`));
        for (const h of hits.slice(0, 200)) {
          const snip = snippetAround(buf.text, h.start, h.end);
          const row = button("tx-hit", null, async () => {
            if (st.current !== m.path) await open(m.path);
            selectRange(h.start, h.end);
          });
          row.append(el("span", "tx-hit-line", String(lineAt(buf.text, h.start))));
          const t = el("span", "tx-hit-text");
          t.append(el("span", null, snip.before), el("mark", null, snip.match), el("span", null, snip.after));
          row.append(t);
          results.append(row);
        }
      }
      results.prepend(el("div", "editor-muted", total ? `${total} match${total === 1 ? "" : "es"}` : "No matches."));
    };

    q.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        if (scope.value === "file") next();
        else runSearch();
      }
    });
    q.addEventListener("input", () => schedule("search", 300, runSearch));
    scope.addEventListener("change", runSearch);
    regexBox.addEventListener("change", runSearch);
    csBox.addEventListener("change", runSearch);
    actions.append(
      button("btn btn-small", "Next", next),
      button("btn btn-small", "Replace", replaceOne),
      button("btn btn-small", "Replace all", replaceEverywhere),
    );
    form.append(q, r, opts, actions);
    body.append(form, results);
  }

  function newFile() {
    const dir = st.opf ? st.opf.slice(0, st.opf.lastIndexOf("/") + 1) : "";
    const v = prompt("Path of the new file inside the book", `${dir}styles/new.css`);
    if (!v) return;
    const path = v.trim().replace(/^\/+/, "");
    if (!path || path.includes("..")) {
      toast("That is not a valid member path.", true);
      return;
    }
    if (st.members.some((m) => m.path === path)) {
      open(path);
      return;
    }
    const ext = path.split(".").pop().toLowerCase();
    const role = ROLE_BY_EXT[ext] || "other";
    if (role === "image" || role === "font") {
      toast("Only text files can be created here.", true);
      return;
    }
    st.members.push({ path, id: null, media_type: null, role, spine_index: null, label: null, size: 0, text: true });
    st.buffers.set(path, { text: "", saved: null, dirty: true, isNew: true });
    reportDirty();
    renderFiles();
    open(path);
  }

  async function save() {
    const edits = [...st.buffers.entries()].filter(([, b]) => b.dirty).map(([member, b]) => ({ member, text: b.text, media_type: null }));
    if (!edits.length) return;
    let res;
    try {
      res = await api.invoke("editor_text_save", { bookId, edits });
    } catch (err) {
      toast(`Save failed: ${err}`, true);
      return;
    }
    if (st.destroyed) return;
    for (const [, b] of st.buffers) {
      if (b.dirty) {
        b.saved = b.text;
        b.dirty = false;
        b.isNew = false;
      }
    }
    st.members = res.members;
    st.findings = res.findings;
    updateFindingsTab();
    if (st.tab === "findings") renderFindings();
    renderFiles();
    reportDirty();
    onSaved(res);
    toast(`Saved ${res.written.length} file${res.written.length === 1 ? "" : "s"} — regenerating the Kindle file…`);
  }

  function revert() {
    const buf = st.buffers.get(st.current);
    if (!buf) return;
    if (buf.isNew) {
      st.buffers.delete(st.current);
      st.members = st.members.filter((m) => m.path !== st.current);
      renderFiles();
      reportDirty();
      const next = spineMembers()[0];
      if (next) open(next.path);
      return;
    }
    buf.text = buf.saved;
    buf.dirty = false;
    ta.value = buf.text;
    highlightNow();
    renderFiles();
    reportDirty();
    if (affectsPreview(st.current)) schedule("preview", 0, renderPreview);
  }

  function destroy() {
    st.destroyed = true;
    for (const t of Object.values(st.timers)) clearTimeout(t);
    preview.destroy();
    for (const u of st.imageUrls.values()) URL.revokeObjectURL(u);
    st.imageUrls.clear();
    center.classList.remove("editor-center-text");
  }

  return {
    save,
    revert,
    destroy,
    isDirty: () => [...st.buffers.values()].some((b) => b.dirty),
    onKey(e) {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "f") {
        e.preventDefault();
        showTab("search", true);
        return true;
      }
      return false;
    },
  };
}
