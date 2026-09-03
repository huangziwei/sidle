const NAME_START = /[A-Za-z_:À-￿]/;

export function langOf(path) {
  const ext = path.split(".").pop().toLowerCase();
  if (ext === "css") return "css";
  if (["xhtml", "html", "htm", "xml", "opf", "ncx", "svg", "smil"].includes(ext)) return "xml";
  return "text";
}

function esc(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

export function tokenize(text, lang) {
  if (lang === "xml") return tokenizeXml(text);
  if (lang === "css") return tokenizeCss(text);
  return [];
}

function tokenizeXml(text) {
  const toks = [];
  const push = (s, e, cls) => {
    if (e > s) toks.push({ s, e, cls });
  };
  let i = 0;
  const n = text.length;
  while (i < n) {
    const lt = text.indexOf("<", i);
    if (lt < 0) break;
    i = lt;
    if (text.startsWith("<!--", i)) {
      const j = text.indexOf("-->", i + 4);
      const end = j < 0 ? n : j + 3;
      push(i, end, "hl-c");
      i = end;
      continue;
    }
    if (text.startsWith("<![CDATA[", i)) {
      const j = text.indexOf("]]>", i + 9);
      const end = j < 0 ? n : j + 3;
      push(i, end, "hl-d");
      i = end;
      continue;
    }
    if (text[i + 1] === "!" || text[i + 1] === "?") {
      const j = text.indexOf(">", i + 2);
      const end = j < 0 ? n : j + 1;
      push(i, end, "hl-p");
      i = end;
      continue;
    }
    const close = text[i + 1] === "/";
    let j = i + (close ? 2 : 1);
    if (!NAME_START.test(text[j] || "")) {
      i++;
      continue;
    }
    while (j < n && /[^\s/>]/.test(text[j])) j++;
    push(i, j, "hl-t");
    i = j;
    while (i < n) {
      const ch = text[i];
      if (ch === ">") {
        push(i, i + 1, "hl-t");
        i++;
        break;
      }
      if (ch === "/" && text[i + 1] === ">") {
        push(i, i + 2, "hl-t");
        i += 2;
        break;
      }
      if (/\s/.test(ch) || ch === "=") {
        i++;
        continue;
      }
      if (ch === '"' || ch === "'") {
        const k = text.indexOf(ch, i + 1);
        const end = k < 0 ? n : k + 1;
        push(i, end, "hl-s");
        i = end;
        continue;
      }
      let k = i;
      while (k < n && /[^\s=/>]/.test(text[k])) k++;
      if (k === i) k = i + 1;
      push(i, k, "hl-a");
      i = k;
    }
  }
  return toks;
}

function tokenizeCss(text) {
  const toks = [];
  const push = (s, e, cls) => {
    if (e > s) toks.push({ s, e, cls });
  };
  let i = 0;
  const n = text.length;
  let inBlock = false;
  while (i < n) {
    if (text.startsWith("/*", i)) {
      const j = text.indexOf("*/", i + 2);
      const end = j < 0 ? n : j + 2;
      push(i, end, "hl-c");
      i = end;
      continue;
    }
    const c = text[i];
    if (c === '"' || c === "'") {
      const k = text.indexOf(c, i + 1);
      const end = k < 0 ? n : k + 1;
      push(i, end, "hl-s");
      i = end;
      continue;
    }
    if (c === "{") {
      inBlock = true;
      i++;
      continue;
    }
    if (c === "}") {
      inBlock = false;
      i++;
      continue;
    }
    if (!inBlock) {
      let k = i;
      while (k < n && !/[{}"'/]/.test(text[k])) k++;
      if (k === i) k = i + 1;
      push(i, k, "hl-t");
      i = k;
      continue;
    }
    let k = i;
    while (k < n && !/[:;}"'/]/.test(text[k])) k++;
    if (k === i) {
      i++;
      continue;
    }
    if (text[k] === ":") {
      push(i, k, "hl-a");
      i = k + 1;
      let v = i;
      while (v < n && !/[;}"'/]/.test(text[v])) v++;
      push(i, v, "hl-v");
      i = v;
    } else {
      i = k;
    }
  }
  return toks;
}

function spans(text, toks, from, to) {
  let out = "";
  let pos = from;
  for (const t of toks) {
    if (t.e <= from) continue;
    if (t.s >= to) break;
    const s = Math.max(t.s, from);
    const e = Math.min(t.e, to);
    if (s > pos) out += esc(text.slice(pos, s));
    out += `<span class="${t.cls}">${esc(text.slice(s, e))}</span>`;
    pos = e;
  }
  if (pos < to) out += esc(text.slice(pos, to));
  return out;
}

export function highlight(text, lang) {
  return spans(text, tokenize(text, lang), 0, text.length);
}

export function renderLines(text, lang) {
  const toks = text.length > 1_500_000 ? [] : tokenize(text, lang);
  const parts = [];
  let line = 1;
  let pos = 0;
  let ti = 0;
  while (true) {
    const nl = text.indexOf("\n", pos);
    const end = nl < 0 ? text.length : nl;
    while (ti < toks.length && toks[ti].e <= pos) ti++;
    parts.push(`<div class="line"><span class="ln">${line}</span>${spans(text, toks.slice(ti), pos, end)}\n</div>`);
    if (nl < 0) break;
    pos = nl + 1;
    line++;
  }
  return parts.join("");
}

export function stampLines(text) {
  const out = [];
  let i = 0;
  let line = 1;
  let last = 0;
  const n = text.length;
  const countTo = (end) => {
    for (let k = i; k < end; k++) if (text.charCodeAt(k) === 10) line++;
  };
  while (i < n) {
    const lt = text.indexOf("<", i);
    if (lt < 0) break;
    countTo(lt);
    i = lt;
    if (text.startsWith("<!--", i)) {
      const j = text.indexOf("-->", i + 4);
      const end = j < 0 ? n : j + 3;
      countTo(end);
      i = end;
      continue;
    }
    if (text.startsWith("<![CDATA[", i)) {
      const j = text.indexOf("]]>", i + 9);
      const end = j < 0 ? n : j + 3;
      countTo(end);
      i = end;
      continue;
    }
    if (text[i + 1] === "!" || text[i + 1] === "?" || text[i + 1] === "/") {
      const j = text.indexOf(">", i + 2);
      const end = j < 0 ? n : j + 1;
      countTo(end);
      i = end;
      continue;
    }
    if (!NAME_START.test(text[i + 1] || "")) {
      i++;
      continue;
    }
    const tagLine = line;
    let j = i + 1;
    while (j < n) {
      const ch = text[j];
      if (ch === '"' || ch === "'") {
        const k = text.indexOf(ch, j + 1);
        j = k < 0 ? n : k + 1;
        continue;
      }
      if (ch === ">") break;
      j++;
    }
    if (j >= n) break;
    const selfClose = text[j - 1] === "/";
    const at = selfClose ? j - 1 : j;
    out.push(text.slice(last, at), ` data-lnum="${tagLine}"`);
    last = at;
    countTo(j + 1);
    i = j + 1;
  }
  out.push(text.slice(last));
  return out.join("");
}

export function lineAt(text, offset) {
  let line = 1;
  const end = Math.min(offset, text.length);
  for (let k = 0; k < end; k++) if (text.charCodeAt(k) === 10) line++;
  return line;
}

export function lineStart(text, line) {
  let pos = 0;
  for (let l = 1; l < line; l++) {
    const nl = text.indexOf("\n", pos);
    if (nl < 0) return text.length;
    pos = nl + 1;
  }
  return pos;
}

export function elementAddressAt(text, offset) {
  const line = lineAt(text, offset);
  const start = lineStart(text, line);
  const lineEnd = text.indexOf("\n", start);
  const lineText = text.slice(start, lineEnd < 0 ? text.length : lineEnd);
  const openings = [];
  const re = /<([A-Za-z_:À-￿][^\s/>]*)/g;
  let m;
  while ((m = re.exec(lineText))) openings.push({ at: start + m.index, tag: m[1] });
  if (!openings.length) return { line, index: 0, tag: null };
  let index = 0;
  for (let k = 0; k < openings.length; k++) {
    if (openings[k].at <= offset) index = k;
  }
  return { line, index, tag: openings[index].tag };
}

function pattern(query, opts) {
  try {
    return opts.regex
      ? new RegExp(query, opts.caseSensitive ? "g" : "gi")
      : new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), opts.caseSensitive ? "g" : "gi");
  } catch {
    return null;
  }
}

export function findAll(text, query, opts = {}) {
  if (!query) return [];
  const re = pattern(query, opts);
  if (!re) return [];
  const hits = [];
  let m;
  while ((m = re.exec(text))) {
    if (m[0].length === 0) {
      re.lastIndex++;
      continue;
    }
    hits.push({ start: m.index, end: m.index + m[0].length });
    if (hits.length > 5000) break;
  }
  return hits;
}

export function replaceAll(text, query, replacement, opts = {}) {
  const hits = findAll(text, query, opts);
  if (!hits.length) return { text, count: 0 };
  const re = opts.regex ? pattern(query, opts) : null;
  let out = "";
  let last = 0;
  for (const h of hits) {
    out += text.slice(last, h.start);
    out += re ? text.slice(h.start, h.end).replace(re, replacement) : replacement;
    last = h.end;
  }
  out += text.slice(last);
  return { text: out, count: hits.length };
}

export function snippetAround(text, start, end, radius = 40) {
  const from = Math.max(0, text.lastIndexOf("\n", start) + 1, start - radius);
  let to = text.indexOf("\n", end);
  if (to < 0) to = text.length;
  to = Math.min(to, end + radius);
  return { before: text.slice(from, start), match: text.slice(start, end), after: text.slice(end, to) };
}

export function relativePath(fromPath, toPath) {
  const from = fromPath.split("/").slice(0, -1);
  const to = toPath.split("/");
  const name = to.pop();
  let i = 0;
  while (i < from.length && i < to.length && from[i] === to[i]) i++;
  const parts = [];
  for (let k = i; k < from.length; k++) parts.push("..");
  parts.push(...to.slice(i), name);
  return parts.join("/").replace(/ /g, "%20");
}

export function classAtCursor(text, pos) {
  const lt = text.lastIndexOf("<", pos);
  if (lt < 0) return "";
  const gt = text.indexOf(">", lt);
  if (gt < 0 || text.lastIndexOf(">", pos - 1) > lt) return "";
  const tag = text.slice(lt, gt + 1);
  const m = /\sclass\s*=\s*["']([^"']*)["']/.exec(tag);
  return m ? m[1].trim().split(/\s+/)[0] || "" : "";
}
