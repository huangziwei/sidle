// Book editor: a full-screen surface over `#editor-view`, one panel per rail item.

// --- text: highlighter, line stamps, search ------------------------------

const NAME_START = /[A-Za-z_:À-￿]/;

function langOf(path) {
  const ext = path.split(".").pop().toLowerCase();
  if (ext === "css") return "css";
  if (["xhtml", "html", "htm", "xml", "opf", "ncx", "svg", "smil"].includes(ext)) return "xml";
  return "text";
}

function esc(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function tokenize(text, lang) {
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

function renderLines(text, lang) {
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

function stampLines(text) {
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

function lineAt(text, offset) {
  let line = 1;
  const end = Math.min(offset, text.length);
  for (let k = 0; k < end; k++) if (text.charCodeAt(k) === 10) line++;
  return line;
}

function lineStart(text, line) {
  let pos = 0;
  for (let l = 1; l < line; l++) {
    const nl = text.indexOf("\n", pos);
    if (nl < 0) return text.length;
    pos = nl + 1;
  }
  return pos;
}

function elementAddressAt(text, offset) {
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

function findAll(text, query, opts = {}) {
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

function replaceAll(text, query, replacement, opts = {}) {
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

function snippetAround(text, start, end, radius = 40) {
  const from = Math.max(0, text.lastIndexOf("\n", start) + 1, start - radius);
  let to = text.indexOf("\n", end);
  if (to < 0) to = text.length;
  to = Math.min(to, end + radius);
  return { before: text.slice(from, start), match: text.slice(start, end), after: text.slice(end, to) };
}

function relativePath(fromPath, toPath) {
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

function classAtCursor(text, pos) {
  const lt = text.lastIndexOf("<", pos);
  if (lt < 0) return "";
  const gt = text.indexOf(">", lt);
  if (gt < 0 || text.lastIndexOf(">", pos - 1) > lt) return "";
  const tag = text.slice(lt, gt + 1);
  const m = /\sclass\s*=\s*["']([^"']*)["']/.exec(tag);
  return m ? m[1].trim().split(/\s+/)[0] || "" : "";
}

// --- text: computed-style inspector ---------------------------------------

const INHERITED = new Set([
  "border-collapse", "border-spacing", "caption-side", "color", "cursor", "direction",
  "empty-cells", "font", "font-family", "font-feature-settings", "font-kerning", "font-size",
  "font-size-adjust", "font-stretch", "font-style", "font-variant", "font-variant-east-asian",
  "font-variant-ligatures", "font-weight", "hanging-punctuation", "hyphens", "letter-spacing",
  "line-break", "line-height", "list-style", "list-style-image", "list-style-position",
  "list-style-type", "orphans", "overflow-wrap", "quotes", "ruby-align", "ruby-position",
  "tab-size", "text-align", "text-align-last", "text-combine-upright", "text-emphasis",
  "text-emphasis-color", "text-emphasis-position", "text-emphasis-style", "text-indent",
  "text-justify", "text-orientation", "text-rendering", "text-shadow", "text-transform",
  "text-underline-position", "visibility", "white-space", "widows", "word-break",
  "word-spacing", "word-wrap", "writing-mode", "-webkit-text-combine", "-webkit-writing-mode",
  "-epub-writing-mode", "-webkit-text-emphasis", "-webkit-text-emphasis-style",
  "-webkit-text-emphasis-position", "-webkit-text-emphasis-color", "-webkit-ruby-position",
  "-webkit-text-orientation", "-webkit-line-break",
]);

const WATCHED = [
  "display", "writing-mode", "font-family", "font-size", "font-weight", "font-style",
  "line-height", "color", "background-color", "text-align", "text-indent", "margin-top",
  "margin-bottom", "margin-left", "margin-right", "padding-top", "padding-bottom",
  "padding-left", "padding-right", "width", "height", "max-width", "max-height",
  "letter-spacing", "text-combine-upright", "text-orientation", "position", "float",
  "page-break-before", "page-break-after", "break-before", "break-after",
];

function matches(el, selector) {
  try {
    return el.matches(selector);
  } catch {
    if (selector.includes("|")) {
      try {
        return el.matches(selector.replace(/[a-z]+\|/gi, "*|"));
      } catch {
        return false;
      }
    }
    return false;
  }
}

function declarations(style, ancestor) {
  const out = [];
  for (let i = 0; i < style.length; i++) {
    const prop = style.item(i);
    if (ancestor && !INHERITED.has(prop)) continue;
    out.push({ prop, value: style.getPropertyValue(prop), important: style.getPropertyPriority(prop) === "important" });
  }
  return out;
}

function sheetName(sheet) {
  const node = sheet.ownerNode;
  if (!node) return { member: null, inline: null };
  if (node.getAttribute("data-sidle-member")) return { member: node.getAttribute("data-sidle-member"), inline: null };
  return { member: null, inline: Number(node.getAttribute("data-lnum")) || null };
}

function walkRules(el, rules, origin, ancestor, out) {
  if (!rules) return;
  for (const rule of rules) {
    if (rule.type === CSSRule.MEDIA_RULE || rule.type === CSSRule.SUPPORTS_RULE) {
      if (rule.type === CSSRule.MEDIA_RULE && rule.media?.mediaText) {
        const win = el.ownerDocument.defaultView;
        if (win && !win.matchMedia(rule.media.mediaText).matches) continue;
      }
      walkRules(el, rule.cssRules, origin, ancestor, out);
      continue;
    }
    if (rule.type !== CSSRule.STYLE_RULE) continue;
    let selector = rule.selectorText;
    if (!matches(el, selector)) continue;
    const parts = selector.split(",").map((s) => s.trim());
    if (parts.length > 1) selector = parts.find((p) => matches(el, p)) || selector;
    const decls = declarations(rule.style, ancestor);
    if (!decls.length) continue;
    out.push({ selector, ...origin, declarations: decls });
  }
}

function matchedRules(el, ancestor) {
  const doc = el.ownerDocument;
  const out = [];
  for (const sheet of doc.styleSheets) {
    if (sheet.disabled) continue;
    let rules = null;
    try {
      rules = sheet.cssRules;
    } catch {
      continue;
    }
    walkRules(el, rules, sheetName(sheet), ancestor, out);
  }
  if (el.getAttribute && el.getAttribute("style")) {
    const decls = declarations(el.style, ancestor);
    if (decls.length) out.push({ selector: null, member: null, inline: Number(el.getAttribute("data-lnum")) || null, declarations: decls });
  }
  return out.reverse();
}

function inspect(el) {
  if (!el) return null;
  const win = el.ownerDocument.defaultView;
  const cs = win.getComputedStyle(el);
  const computed = WATCHED.map((p) => [p, cs.getPropertyValue(p)]).filter(([, v]) => v);
  const nodes = [];
  let target = el;
  let depth = 0;
  while (target && target.nodeType === 1) {
    const rules = matchedRules(target, depth > 0);
    if (rules.length || depth === 0) {
      nodes.push({
        tag: target.localName,
        id: target.id || null,
        classes: target.getAttribute("class") || "",
        lnum: Number(target.getAttribute("data-lnum")) || null,
        depth,
        rules,
      });
    }
    target = target.parentNode;
    depth++;
  }
  return { nodes, computed };
}

// --- text: preview iframe -------------------------------------------------

const XLINK = "http://www.w3.org/1999/xlink";
const CSS_URL = /url\(\s*([^)]*?)\s*\)/gi;
const EXTERNAL = /^(?:[a-z][a-z0-9+.-]*:|\/\/|#)/i;

const EDITOR_CSS = `
.sidle-editor-target { outline: 2px solid #c83e3e !important; outline-offset: 1px; }
img, image, svg, video { max-width: 100%; }
`;

function dirOf(path) {
  const i = path.lastIndexOf("/");
  return i < 0 ? "" : path.slice(0, i + 1);
}

function resolvePath(baseDir, href) {
  const hash = href.indexOf("#");
  const path = hash < 0 ? href : href.slice(0, hash);
  if (!path) return null;
  let decoded = path;
  try {
    decoded = decodeURIComponent(path);
  } catch {
    decoded = path;
  }
  const stack = baseDir.split("/").filter(Boolean);
  for (const seg of decoded.split("/")) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") stack.pop();
    else stack.push(seg);
  }
  return stack.join("/");
}

function mimeOf(path) {
  const ext = path.split(".").pop().toLowerCase();
  return {
    jpg: "image/jpeg",
    jpeg: "image/jpeg",
    png: "image/png",
    gif: "image/gif",
    webp: "image/webp",
    svg: "image/svg+xml",
    bmp: "image/bmp",
    ttf: "font/ttf",
    otf: "font/otf",
    woff: "font/woff",
    woff2: "font/woff2",
    css: "text/css",
    mp3: "audio/mpeg",
    mp4: "video/mp4",
  }[ext] || "application/octet-stream";
}

function makePreview({ iframe, readBytes, readText, onClick }) {
  const blobs = new Map();
  const sheetUrls = [];
  let current = null;
  let docUrl = null;
  let mode = "xml";

  const bytesUrl = (path) => {
    if (!blobs.has(path)) {
      blobs.set(
        path,
        readBytes(path)
          .then((bytes) => URL.createObjectURL(new Blob([bytes], { type: mimeOf(path) })))
          .catch(() => null),
      );
    }
    return blobs.get(path);
  };

  const cssUrl = async (path, seen) => {
    if (seen.has(path)) return null;
    seen.add(path);
    let text;
    try {
      text = await readText(path);
    } catch {
      return null;
    }
    const base = dirOf(path);
    const jobs = [];
    text.replace(CSS_URL, (whole, ref) => {
      const target = ref.trim().replace(/^["']|["']$/g, "");
      if (!target || EXTERNAL.test(target)) return whole;
      const abs = resolvePath(base, target);
      if (abs) jobs.push([whole, abs]);
      return whole;
    });
    const imports = [...text.matchAll(/@import\s+(?:url\()?\s*["']?([^"')\s;]+)["']?\s*\)?/gi)];
    const resolved = new Map();
    for (const [, abs] of jobs) {
      if (!resolved.has(abs)) resolved.set(abs, abs.endsWith(".css") ? await cssUrl(abs, seen) : await bytesUrl(abs));
    }
    for (const m of imports) {
      const abs = resolvePath(base, m[1]);
      if (abs && !resolved.has(abs)) resolved.set(abs, await cssUrl(abs, seen));
    }
    let out = text.replace(CSS_URL, (whole, ref) => {
      const target = ref.trim().replace(/^["']|["']$/g, "");
      if (!target || EXTERNAL.test(target)) return whole;
      const url = resolved.get(resolvePath(base, target));
      return url ? `url("${url}")` : whole;
    });
    out = out.replace(/@import\s+(?:url\()?\s*["']?([^"')\s;]+)["']?\s*\)?/gi, (whole, ref) => {
      const url = resolved.get(resolvePath(base, ref));
      return url ? `@import url("${url}")` : whole;
    });
    const url = URL.createObjectURL(new Blob([out], { type: "text/css" }));
    sheetUrls.push(url);
    return url;
  };

  const parse = (text) => {
    const stamped = stampLines(text);
    const xml = new DOMParser().parseFromString(stamped, "application/xhtml+xml");
    if (!xml.querySelector("parsererror")) return { doc: xml, mode: "xml" };
    return { doc: new DOMParser().parseFromString(stamped, "text/html"), mode: "html" };
  };

  const build = async (member, text) => {
    const parsed = parse(text);
    const doc = parsed.doc;
    const base = dirOf(member);
    const seen = new Set();
    const tasks = [];
    for (const link of doc.querySelectorAll("link[href]")) {
      const rel = (link.getAttribute("rel") || "").toLowerCase();
      const href = link.getAttribute("href");
      if (!rel.includes("stylesheet") || EXTERNAL.test(href)) continue;
      const abs = resolvePath(base, href);
      if (!abs) continue;
      link.setAttribute("data-sidle-member", abs);
      tasks.push(cssUrl(abs, seen).then((url) => {
        if (url) link.setAttribute("href", url);
        else link.removeAttribute("href");
      }));
    }
    const bind = (el, attr, ns) => {
      const v = ns ? el.getAttributeNS(ns, "href") || el.getAttribute("xlink:href") || el.getAttribute("href") : el.getAttribute(attr);
      if (!v || EXTERNAL.test(v)) return;
      const abs = resolvePath(base, v);
      if (!abs) return;
      tasks.push(bytesUrl(abs).then((url) => {
        if (!url) return;
        if (ns) {
          el.setAttributeNS(XLINK, "href", url);
          el.setAttribute("href", url);
        } else el.setAttribute(attr, url);
      }));
    };
    doc.querySelectorAll("img[src], video[src], audio[src], source[src], iframe[src]").forEach((el) => bind(el, "src"));
    doc.querySelectorAll("image").forEach((el) => bind(el, "href", XLINK));
    for (const el of doc.querySelectorAll("[style]")) {
      const style = el.getAttribute("style");
      if (!/url\(/i.test(style)) continue;
      const refs = [...style.matchAll(CSS_URL)].map((m) => m[1].trim().replace(/^["']|["']$/g, ""));
      tasks.push(Promise.all(refs.map((r) => EXTERNAL.test(r) ? null : bytesUrl(resolvePath(base, r) || ""))).then((urls) => {
        let k = 0;
        el.setAttribute("style", style.replace(CSS_URL, (whole) => (urls[k] ? `url("${urls[k++]}")` : (k++, whole))));
      }));
    }
    for (const a of doc.querySelectorAll("a[href]")) {
      const href = a.getAttribute("href");
      if (!EXTERNAL.test(href)) a.setAttribute("data-sidle-href", resolvePath(base, href) + (href.includes("#") ? href.slice(href.indexOf("#")) : ""));
    }
    await Promise.all(tasks);
    const style = doc.createElementNS("http://www.w3.org/1999/xhtml", "style");
    style.textContent = EDITOR_CSS;
    const head = doc.querySelector("head") || doc.documentElement;
    head.appendChild(style);
    if (parsed.mode === "xml") {
      return new Blob([new XMLSerializer().serializeToString(doc)], { type: "application/xhtml+xml" });
    }
    return new Blob(["<!DOCTYPE html>\n" + doc.documentElement.outerHTML], { type: "text/html" });
  };

  const attach = () => {
    const doc = iframe.contentDocument;
    if (!doc) return;
    doc.addEventListener("click", (e) => {
      const a = e.target.closest && e.target.closest("a[data-sidle-href]");
      const el = e.target.closest && e.target.closest("[data-lnum]");
      e.preventDefault();
      if (!el || !onClick) return;
      const line = Number(el.getAttribute("data-lnum"));
      const same = [...doc.querySelectorAll(`[data-lnum="${line}"]`)];
      onClick({ line, index: Math.max(0, same.indexOf(el)), href: a ? a.getAttribute("data-sidle-href") : null });
    }, true);
  };

  let generation = 0;

  return {
    async render(member, text) {
      const gen = ++generation;
      const blob = await build(member, text);
      if (gen !== generation) return;
      const keepScroll = current === member ? iframe.contentWindow?.scrollY || 0 : 0;
      const url = URL.createObjectURL(blob);
      await new Promise((resolve) => {
        iframe.onload = () => {
          attach();
          if (keepScroll) iframe.contentWindow?.scrollTo(0, keepScroll);
          resolve();
        };
        iframe.src = url;
      });
      if (docUrl) URL.revokeObjectURL(docUrl);
      while (sheetUrls.length > 8) URL.revokeObjectURL(sheetUrls.shift());
      docUrl = url;
      current = member;
      mode = parse(text).mode;
    },
    member: () => current,
    mode: () => mode,
    document: () => iframe.contentDocument,
    elementAt(line, index) {
      const doc = iframe.contentDocument;
      if (!doc) return null;
      const same = doc.querySelectorAll(`[data-lnum="${line}"]`);
      return same[index] || same[0] || null;
    },
    scrollToLine(line, index) {
      const doc = iframe.contentDocument;
      if (!doc) return;
      for (const old of doc.querySelectorAll(".sidle-editor-target")) old.classList.remove("sidle-editor-target");
      const el = this.elementAt(line, index);
      if (!el) return;
      el.classList.add("sidle-editor-target");
      el.scrollIntoView({ block: "center" });
    },
    forget(path) {
      const p = blobs.get(path);
      if (p) {
        p.then((u) => u && URL.revokeObjectURL(u));
        blobs.delete(path);
      }
    },
    destroy() {
      generation++;
      for (const p of blobs.values()) p.then((u) => u && URL.revokeObjectURL(u));
      blobs.clear();
      for (const u of sheetUrls) URL.revokeObjectURL(u);
      sheetUrls.length = 0;
      if (docUrl) URL.revokeObjectURL(docUrl);
      docUrl = null;
      iframe.onload = null;
      iframe.removeAttribute("src");
    },
  };
}

// --- text panel -----------------------------------------------------------

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

function b64ToBytes(b64) {
  return Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
}

function mountTextPanel({ bookId, center, toast, onDirty, onSaved, showPanel }) {
  const api = window.api;
  const st = {
    members: [],
    opf: null,
    buffers: new Map(),
    removed: new Set(),
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
  const toolsBtn = button("btn btn-small", "Tools ▾", openTools, "Book-wide operations");
  toolbar.append(pathLabel, posLabel, toolsBtn, findBtn, gotoBtn);
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
        b.append(el("span", "tx-file-size", formatBytes(buf ? new TextEncoder().encode(buf.text).length : m.size)));
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
    onDirty(st.removed.size > 0 || [...st.buffers.values()].some((b) => b.dirty));
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
      imageView.replaceChildren(img, el("div", "editor-muted", `${path} · ${formatBytes(m.size)}`));
    } else {
      imageView.replaceChildren(el("div", "editor-muted", `${path} · ${m.media_type || m.role} · ${formatBytes(m.size)}`));
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
        const fix = fixAction(f);
        if (fix) body.append(fix);
      }
    }
  }

  function fixAction(f) {
    switch (f.fix_action) {
      case "restore-styles":
        return restoreStylesAction();
      case "upgrade-epub3":
        return fixButton("Upgrade to EPUB 3", upgradeBook);
      case "rebuild-toc":
        return showPanel ? fixButton("Open the Table of Contents panel", () => showPanel("toc")) : null;
      case "reorder-spine":
        return showPanel ? fixButton("Open the Reading Order panel", () => showPanel("spine")) : null;
      default:
        return null;
    }
  }

  function fixButton(label, fn) {
    const box = el("div", "tx-fix");
    box.append(button("btn btn-small", label, fn));
    return box;
  }

  function openTools(e) {
    const menu = window.ActionMenu;
    if (!menu?.openChoices) return;
    const cur = st.members.find((m) => m.path === st.current);
    const isDoc = cur && (cur.role === "text" || cur.role === "nav");
    const isText = cur && (isDoc || cur.role === "style");
    const items = [
      ["Rename class…", renameClass],
      ["Remove unused CSS", () => runOp({ kind: "remove-unused-css" })],
    ];
    if (isText) items.push([`Beautify ${basename(cur.path)}`, () => runOp({ kind: "beautify", member: cur.path })]);
    items.push(["Beautify all text files", () => runOp({ kind: "beautify", member: null })]);
    if (isDoc) {
      items.push(["Split at cursor", splitAtCursor]);
      if (cur.spine_index != null) items.push(["Merge with next file", () => runOp({ kind: "merge-with-next", member: cur.path })]);
      items.push(["Insert image…", insertImage], ["Insert link…", insertLink]);
    }
    items.push(["Upgrade to EPUB 3", upgradeBook]);
    const r = e.currentTarget.getBoundingClientRect();
    menu.openChoices(items, { x: r.left, y: r.bottom + 4 });
  }

  function dirtyEdits() {
    return [...st.buffers.entries()].filter(([, b]) => b.dirty).map(([member, b]) => ({ member, text: b.text, media_type: null }));
  }

  async function runOp(op) {
    let out;
    try {
      out = await api.invoke("editor_text_op", { bookId, edits: dirtyEdits(), removed: [...st.removed], op });
    } catch (err) {
      toast(`${err}`, true);
      return null;
    }
    if (st.destroyed) return null;
    await applyOutcome(out);
    return out;
  }

  async function applyOutcome(out) {
    const touched = new Set();
    for (const m of out.changed) {
      let buf;
      try {
        buf = await buffer(m.path);
      } catch {
        buf = { text: m.text, saved: null, dirty: true, isNew: false };
        st.buffers.set(m.path, buf);
      }
      buf.text = m.text;
      buf.dirty = buf.saved == null || m.text !== buf.saved;
      touched.add(m.path);
      preview.forget(m.path);
    }
    for (const m of out.added) {
      const ext = m.path.split(".").pop().toLowerCase();
      if (!st.members.some((x) => x.path === m.path)) {
        st.members.push({ path: m.path, id: null, media_type: m.media_type, role: ROLE_BY_EXT[ext] || "other", spine_index: null, label: null, size: 0, text: true });
      }
      st.buffers.set(m.path, { text: m.text, saved: null, dirty: true, isNew: true });
      st.removed.delete(m.path);
      touched.add(m.path);
    }
    for (const path of out.removed) {
      st.removed.add(path);
      st.buffers.delete(path);
      st.members = st.members.filter((x) => x.path !== path);
      preview.forget(path);
    }
    renderFiles();
    reportDirty();
    const current = st.current;
    if (current && out.removed.includes(current)) {
      const next = out.changed[0]?.path || spineMembers()[0]?.path;
      if (next) await open(next);
    } else if (current && touched.has(current)) {
      const buf = st.buffers.get(current);
      const pos = ta.selectionStart;
      ta.value = buf.text;
      ta.setSelectionRange(Math.min(pos, buf.text.length), Math.min(pos, buf.text.length));
      highlightNow();
      if (affectsPreview(current)) schedule("preview", 0, renderPreview);
    } else if ([...touched].some((p) => affectsPreview(p))) {
      schedule("preview", 0, renderPreview);
    }
    const n = out.changed.length + out.added.length + out.removed.length;
    toast(`${out.notes.join("; ") || out.operation}${n ? ` — ${n} file${n === 1 ? "" : "s"} changed, save to keep` : ""}`);
  }

  function renameClass() {
    const guess = st.current && !editor.hidden ? classAtCursor(ta.value, ta.selectionStart) : "";
    const from = prompt("Class to rename", guess);
    if (!from) return;
    const to = prompt(`Rename “${from.trim()}” to`, from.trim());
    if (!to || to.trim() === from.trim()) return;
    runOp({ kind: "rename-class", from: from.trim(), to: to.trim() });
  }

  function splitAtCursor() {
    if (!st.current || editor.hidden) return;
    const pos = ta.selectionStart;
    const line = lineAt(ta.value, pos);
    const col = pos - lineStart(ta.value, line) + 1;
    runOp({ kind: "split-document", member: st.current, line, col }).then((out) => {
      const added = out?.added?.[0];
      if (added) open(added.path);
    });
  }

  function upgradeBook() {
    if (!confirm("Upgrade this book to EPUB 3? The package, metadata, navigation document and DOCTYPEs are rewritten; bodies and styles stay as they are.")) return;
    runOp({ kind: "upgrade-epub3" });
  }

  function pickMember(list, at, onPick) {
    const menu = window.ActionMenu;
    if (!menu?.openChoices || !list.length) {
      toast("Nothing to pick from.", true);
      return;
    }
    menu.openChoices(list.map((m) => [m.label ? `${m.label} · ${basename(m.path)}` : basename(m.path), () => onPick(m)]), at);
  }

  function menuAt() {
    const r = toolsBtn.getBoundingClientRect();
    return { x: r.left, y: r.bottom + 4 };
  }

  function insertImage() {
    if (!st.current || editor.hidden) return;
    const here = st.current;
    pickMember(st.members.filter((m) => m.role === "image"), menuAt(), (m) => {
      const rel = relativePath(here, m.path);
      ta.focus();
      insertAtCursor(`<img src="${rel}" alt=""/>`);
    });
  }

  function insertLink() {
    if (!st.current || editor.hidden) return;
    const here = st.current;
    const targets = spineMembers().concat(st.members.filter((m) => m.role === "nav" && m.spine_index == null));
    pickMember(targets, menuAt(), (m) => {
      const rel = m.path === here ? "" : relativePath(here, m.path);
      const s = ta.selectionStart;
      const e = ta.selectionEnd;
      const label = ta.value.slice(s, e) || m.label || basename(m.path);
      ta.focus();
      insertAtCursor(`<a href="${rel}">${label}</a>`);
    });
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
    const edits = dirtyEdits();
    const removed = [...st.removed];
    if (!edits.length && !removed.length) return;
    let res;
    try {
      res = await api.invoke("editor_text_save", { bookId, edits, removed });
    } catch (err) {
      toast(`Save failed: ${err}`, true);
      return;
    }
    if (st.destroyed) return;
    st.removed.clear();
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
    isDirty: () => st.removed.size > 0 || [...st.buffers.values()].some((b) => b.dirty),
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

// --- editor shell ---------------------------------------------------------

const $ = (sel) => document.querySelector(sel);
const toast = (msg, isError) => window.showToast?.(msg, isError);

let textPanel = null;

// Live editor session, or null when closed. `open` snapshots the opening
// values; Revert restores them and Save diffs against them.
let session = null;

const view = () => $("#editor-view");

// --- open / close ----------------------------------------------------------

async function open(bookId) {
  if (!close()) return; // tear down any prior session (aborts if user keeps edits)
  let data;
  try {
    data = await window.api.invoke("editor_open", { bookId });
  } catch (err) {
    toast(`Couldn't open editor: ${err}`, true);
    return;
  }

  session = { bookId, data, panel: "metadata", dirty: false, tocDetail: null };

  $("#editor-title").textContent = data.metadata.title || "Untitled";
  renderTocChip(data.toc);
  configureRail();
  selectPanel("metadata");

  view().hidden = false;
  installKeys();
  // Focus the first field.
  requestAnimationFrame(() => $("#editor-center input")?.focus());
}

// Returns true if the editor is closed, false if the user kept unsaved edits.
function close() {
  if (!session) return true;
  if (session.dirty && !confirm("Discard unsaved changes?")) return false;
  removeKeys();
  unmountTextPanel();
  view().hidden = true;
  $("#editor-center").replaceChildren();
  session = null;
  return true;
}

function unmountTextPanel() {
  if (!textPanel) return;
  textPanel.destroy();
  textPanel = null;
}

function isOpen() {
  return !!session && !view().hidden;
}

// --- keyboard --------------------------------------------------------------

let keyHandler = null;

function installKeys() {
  keyHandler = (e) => {
    if (textPanel?.onKey(e)) return;
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    } else if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "s") {
      e.preventDefault();
      if (!$("#editor-save").disabled) saveCurrentPanel();
    }
  };
  document.addEventListener("keydown", keyHandler, true);
}

function removeKeys() {
  if (keyHandler) {
    document.removeEventListener("keydown", keyHandler, true);
    keyHandler = null;
  }
}

// --- left rail -------------------------------------------------------------

// Enable the rail items `session.data.panels` names; every other item stays disabled.
function configureRail() {
  const editable = session.data.editable;
  // `panels` is the backend's list of what this source format can back.
  const panels = new Set(session.data.panels || []);
  for (const item of document.querySelectorAll(".editor-rail-item")) {
    const p = item.dataset.panel;
    item.disabled = !(editable && panels.has(p));
    if (!editable || panels.has(p)) {
      item.title = "";
    } else if (p === "text") {
      item.title = "Text and style editing writes to an EPUB source.";
    } else if (p === "spine") {
      item.title =
        session.data.format === "pdf"
          ? "A PDF's reading order is its page order, which this editor doesn't rearrange."
          : "A Kindle file's reading order carries every reading position with it, so reordering it is a rebuild rather than a reorder — not built yet.";
    } else {
      item.title = "";
    }
  }
}

function selectPanel(panel) {
  // Switching away from the metadata panel drops its unsaved edits.
  if (
    session.panel !== panel &&
    session.dirty &&
    !confirm("Discard unsaved changes to this panel?")
  ) {
    return;
  }
  session.panel = panel;
  session.dirty = false;
  unmountTextPanel();
  for (const item of document.querySelectorAll(".editor-rail-item")) {
    item.classList.toggle("active", item.dataset.panel === panel);
  }
  if (panel === "text") {
    markDirty(false);
    textPanel = mountTextPanel({
      bookId: session.bookId,
      center: $("#editor-center"),
      toast,
      onDirty: (dirty) => markDirty(dirty),
      onSaved: (res) => {
        if (res.toc) renderTocChip(res.toc);
      },
      showPanel: selectPanel,
    });
    return;
  }
  // Every panel except Metadata commits via its own in-panel buttons; the
  // top-bar Save/Revert belong to the metadata panel.
  if (panel === "metadata") {
    renderMetadataPanel();
    markDirty(false);
  } else if (panel === "cover") {
    $("#editor-save").disabled = true;
    $("#editor-revert").disabled = true;
    renderCoverPanel();
  } else if (panel === "images") {
    $("#editor-save").disabled = true;
    $("#editor-revert").disabled = true;
    renderImagesPanel();
  } else if (panel === "toc") {
    $("#editor-save").disabled = true;
    $("#editor-revert").disabled = true;
    renderTocPanel();
  } else if (panel === "spine") {
    $("#editor-save").disabled = true;
    $("#editor-revert").disabled = true;
    renderSpinePanel();
  }
}

// --- validate chip ---------------------------------------------------------

function renderTocChip(toc) {
  const chip = $("#editor-toc-chip");
  if (!toc) {
    chip.hidden = true;
    return;
  }
  chip.hidden = false;
  chip.dataset.verdict = toc.verdict;
  chip.textContent = chipText(toc.verdict);
  let title = `Declared TOC: ${toc.nav_chapters} chapter entr${
    toc.nav_chapters === 1 ? "y" : "ies"
  } of ${toc.nav_count}.`;
  // The in-book signal counts are only in the fuller open-time verdict, not the
  // post-edit refresh — include them when present.
  if (toc.contents_links != null) {
    title +=
      ` In-book signals — contents links: ${toc.contents_links}, ` +
      `headings: ${toc.headings}, chapter starts: ${toc.section_heads}.`;
  }
  chip.title = title;
}

// --- dirty state -----------------------------------------------------------

function markDirty(dirty) {
  session.dirty = dirty;
  $("#editor-save").disabled = !dirty || !session.data.editable;
  $("#editor-revert").disabled = !dirty;
}

// --- metadata panel --------------------------------------------------------

function renderMetadataPanel() {
  const m = session.data.metadata;
  const editable = session.data.editable;
  const center = $("#editor-center");
  center.replaceChildren();

  // All three source formats are editable; the only way here is a missing
  // source file.
  const notice = editable
    ? ""
    : `<div class="editor-notice">This book's ${session.data.format.toUpperCase()} source
         file isn't on disk, so it can't be edited. These fields are read-only.</div>`;

  const form = document.createElement("form");
  form.className = "editor-panel metadata-fields";
  form.autocomplete = "off";
  form.innerHTML = `
    ${notice}
    <div class="field-group-title">Title &amp; author</div>
    <label class="field">
      <span>Title</span>
      <input name="title" required ${editable ? "" : "readonly"} />
    </label>
    <label class="field">
      <span>Authors</span>
      <input name="author" placeholder="Jane Smith & John Doe" ${editable ? "" : "readonly"} />
    </label>

    <div class="field-group-title">Publication</div>
    <div class="field-row">
      <label class="field">
        <span>Publisher</span>
        <input name="publisher" placeholder="新潮文庫" ${editable ? "" : "readonly"} />
      </label>
      <label class="field field-narrow">
        <span>Published</span>
        <input name="published_at" placeholder="2024" ${editable ? "" : "readonly"} />
      </label>
    </div>
    <label class="field">
      <span>Language</span>
      <input name="language" placeholder="en" ${editable ? "" : "readonly"} />
    </label>

    <div class="field-group-title">Identifiers</div>
    <label class="field">
      <span>Amazon ASIN</span>
      <input name="amazon_asin" placeholder="B01ABCDEFG" spellcheck="false"
             ${editable ? "" : "readonly"} />
    </label>
    <small class="field-hint">
      Names this book in Amazon's catalogue. Used only to fetch the color cover —
      it is never written into the file.
    </small>
    <label class="field">
      <span>Content ID</span>
      <input name="content_id" readonly tabindex="-1" spellcheck="false" />
    </label>
    <small class="field-hint">
      The id baked into this file. The Kindle keys its library entry, reading
      position and notebooks on it, so it's the file's to state, not yours to choose.
    </small>
  `;

  form.elements.title.value = m.title || "";
  form.elements.author.value = m.author || "";
  form.elements.publisher.value = m.publisher || "";
  form.elements.published_at.value = m.published_at || "";
  form.elements.language.value = m.language || "";
  form.elements.amazon_asin.value = m.amazon_asin || "";
  form.elements.content_id.value = m.content_id || "";

  form.addEventListener("submit", (e) => e.preventDefault());
  if (editable) {
    form.addEventListener("input", () => markDirty(true));
  }
  center.appendChild(form);
}

function metadataFormValues() {
  const f = $("#editor-center");
  const val = (name) => f.querySelector(`[name="${name}"]`).value.trim();
  return {
    title: val("title"),
    author: val("author"),
    language: val("language"),
    publisher: val("publisher") || null,
    published_at: val("published_at") || null,
    // The content id is shown, not submitted: it belongs to the file.
    amazon_asin: val("amazon_asin") || null,
  };
}

async function saveMetadata() {
  const form = metadataFormValues();
  if (!form.title) {
    toast("Title can't be empty.", true);
    return;
  }
  const save = $("#editor-save");
  save.disabled = true;
  save.textContent = "Saving…";
  try {
    const row = await window.api.invoke("editor_save_metadata", {
      bookId: session.bookId,
      form,
    });
    // Reflect the canonicalized values the backend settled on (author flip,
    // language code harmonize) back into the fields and the snapshot.
    session.data.metadata = {
      title: row.title,
      author: row.author,
      language: row.language,
      publisher: row.publisher,
      published_at: row.published_at,
      amazon_asin: row.amazon_asin,
      content_id: row.asin,
    };
    $("#editor-title").textContent = row.title || "Untitled";
    renderMetadataPanel();
    markDirty(false);
    toast("Saved — regenerating the derived EPUB…");
  } catch (err) {
    toast(`Save failed: ${err}`, true);
  } finally {
    save.textContent = "Save";
    save.disabled = !session.dirty;
  }
}

// --- cover panel -----------------------------------------------------------

// The cover flow runs on the library's cover commands (`library_set_cover`, fetch, clear).
async function renderCoverPanel() {
  const center = $("#editor-center");
  center.replaceChildren(el("div", "editor-panel editor-muted", "Loading cover…"));
  let path;
  try {
    path = await window.api.invoke("library_cover_path", { bookId: session.bookId });
  } catch (err) {
    center.replaceChildren(wrapPanel(el("div", "editor-notice", `Couldn't load the cover: ${err}`)));
    return;
  }
  if (session.panel !== "cover") return; // navigated away while loading
  paintCoverPanel(path);
}

function paintCoverPanel(coverPath) {
  session.coverPath = coverPath;
  const pdfCover = session.data.format === "pdf";
  const panel = el("div", "editor-panel");
  // For a PDF this preview is the book's first page, which the library tile
  // renders; the heading names it as the first page.
  panel.append(
    el("div", "field-group-title", pdfCover ? "Current first page" : "Cover image"),
  );

  const preview = el("div", "cover-preview");
  if (coverPath) {
    const img = el("img", "cover-art");
    // Cache-bust: the sidecar path is stable across swaps.
    img.src = `${window.api.fileUrl(coverPath)}?v=${Date.now()}`;
    img.alt = pdfCover ? "The book's current first page" : "Current cover";
    preview.append(img);
  } else {
    preview.append(el("div", "cover-empty", pdfCover ? "No preview" : "No cover set"));
  }
  panel.append(preview);

  // A PDF's cover is the book's first page, not an embeddable resource; the
  // choice is which page edit to make.
  const pdf = session.data.format === "pdf";
  panel.append(
    el(
      "p",
      "editor-muted",
      pdf
        ? "A PDF's cover is its first page. Replace that page with an image, or insert " +
          "a new first page if the book opens straight onto its text. The library tile " +
          "and the Kindle file follow from it."
        : "The cover is embedded in the KFX — the on-device home tile and sleep-screen art — " +
          "and written to the library, keeping the same on-device identity.",
    ),
  );

  const actions = el("div", "cover-actions");
  if (pdf) {
    const replace = el("button", "btn btn-primary", "Replace first page…");
    replace.type = "button";
    replace.title = "Overwrite the book's existing first page with an image";
    replace.addEventListener("click", () => setPdfCover("replace"));
    const insert = el("button", "btn", "Insert cover page…");
    insert.type = "button";
    insert.title = "Add a new first page in front of the book (page count grows by one)";
    insert.addEventListener("click", () => setPdfCover("insert"));
    actions.append(replace, insert);
  } else {
    const change = el("button", "btn btn-primary", "Change cover…");
    change.type = "button";
    change.addEventListener("click", changeCover);
    actions.append(change);

    // Fetching is by catalogue id: the file's own identity names nothing on
    // Amazon.
    const asin = session.data.metadata.amazon_asin;
    if (asin) {
      const refetch = el("button", "btn", "Re-fetch from Amazon");
      refetch.type = "button";
      refetch.title = `Fetch the cover by ASIN ${asin}`;
      refetch.addEventListener("click", refetchCover);
      actions.append(refetch);
    }
  }
  panel.append(actions);
  $("#editor-center").replaceChildren(panel);
}

// Pick an image and write it into the PDF as its cover page. `mode` is
// "replace" (overwrite page 1) or "insert" (add a new page 1).
async function setPdfCover(mode) {
  let src;
  try {
    src = await window.api.invoke("library_pick_image");
  } catch (err) {
    toast(`Couldn't open the picker: ${err}`, true);
    return;
  }
  if (!src) return; // cancelled
  await runCoverWrite(() =>
    window.api.invoke("editor_set_pdf_cover", {
      bookId: session.bookId,
      srcPath: src,
      mode,
    }),
  );
}

async function changeCover() {
  let src;
  try {
    src = await window.api.invoke("library_pick_image");
  } catch (err) {
    toast(`Couldn't pick an image: ${err}`, true);
    return;
  }
  if (!src) return; // cancelled
  await runCoverWrite(() =>
    window.api.invoke("library_set_cover", { bookId: session.bookId, srcPath: src }),
  );
}

async function refetchCover() {
  await runCoverWrite(() =>
    window.api.invoke("library_recrawl_cover", { bookId: session.bookId }),
  );
}

// Shared commit wrapper for both cover write paths. Both return a tagged result:
// {kind:"updated", cover_path} | {kind:"no_asin"} | {kind:"failed", error}.
async function runCoverWrite(invoke) {
  const buttons = $("#editor-center").querySelectorAll(".cover-actions button");
  buttons.forEach((b) => (b.disabled = true));
  try {
    const res = await invoke();
    if (res.kind === "updated") {
      session.data.has_cover = true;
      paintCoverPanel(res.cover_path); // repaint re-enables the buttons
      toast("Cover updated.");
      return;
    }
    if (res.kind === "no_asin") {
      toast("This book has no Amazon ASIN to fetch from.", true);
    } else {
      toast(`Couldn't update the cover: ${res.error}`, true);
    }
  } catch (err) {
    toast(`Couldn't update the cover: ${err}`, true);
  }
  buttons.forEach((b) => (b.disabled = false));
}

// --- images panel ----------------------------------------------------------

// KFX and EPUB carry a list of embedded images. A PDF's pages are its images:
// `renderPdfPagesPanel` shows a page grid to select from and export.

async function renderImagesPanel() {
  if (session.data.format === "pdf") return renderPdfPagesPanel();
  const center = $("#editor-center");
  center.replaceChildren(el("div", "editor-panel editor-muted", "Extracting images…"));
  let images;
  try {
    images = await window.api.invoke("editor_images", { bookId: session.bookId });
  } catch (err) {
    center.replaceChildren(wrapPanel(el("div", "editor-notice", `Couldn't read images: ${err}`)));
    return;
  }
  if (session.panel !== "images") return; // navigated away while loading
  session.images = images;
  paintImagesPanel(images);
}

function paintImagesPanel(images) {
  const panel = el("div", "editor-panel");
  panel.append(
    el("div", "field-group-title", `Embedded images${images.length ? ` (${images.length})` : ""}`),
  );
  if (!images.length) {
    panel.append(el("p", "editor-muted", "This book has no embedded images."));
    $("#editor-center").replaceChildren(panel);
    return;
  }

  const actions = el("div", "images-actions");
  const exportAll = el("button", "btn btn-primary", "Export all…");
  exportAll.type = "button";
  exportAll.addEventListener("click", exportAllImages);
  actions.append(exportAll);
  panel.append(actions);

  const grid = el("div", "images-grid");
  for (const img of images) grid.append(imageCard(img));
  panel.append(grid);
  $("#editor-center").replaceChildren(panel);
}

function imageCard(img) {
  const card = el("div", "image-card");

  const thumb = el("div", "image-thumb");
  const preview = el("img");
  preview.src = window.api.fileUrl(img.preview_path);
  preview.alt = img.resource_name;
  preview.loading = "lazy";
  thumb.append(preview);
  if (img.is_cover) thumb.append(el("span", "image-badge", "Cover"));
  card.append(thumb);

  const meta = el("div", "image-meta");
  meta.append(el("div", "image-name", img.resource_name));
  const dims = img.width && img.height ? `${img.width}×${img.height} · ` : "";
  meta.append(
    el("div", "image-dims", `${dims}${img.ext.toUpperCase()} · ${formatBytes(img.byte_len)}`),
  );
  card.append(meta);

  const save = el("button", "btn btn-small", "Save…");
  save.type = "button";
  save.addEventListener("click", () => exportOneImage(img, save));
  card.append(save);
  return card;
}

function formatBytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

async function exportOneImage(img, button) {
  button.disabled = true;
  try {
    const saved = await window.api.invoke("editor_export_image", {
      bookId: session.bookId,
      index: img.index,
    });
    if (saved) toast(`Saved ${img.resource_name}.`);
  } catch (err) {
    toast(`Couldn't save the image: ${err}`, true);
  } finally {
    button.disabled = false;
  }
}

async function exportAllImages() {
  let dir;
  try {
    dir = await window.api.invoke("library_pick_folder");
  } catch (err) {
    toast(`Couldn't pick a folder: ${err}`, true);
    return;
  }
  if (!dir) return; // cancelled
  try {
    const res = await window.api.invoke("editor_export_images", {
      bookId: session.bookId,
      dir,
    });
    toast(`Exported ${res.count} image${res.count === 1 ? "" : "s"}.`);
  } catch (err) {
    toast(`Couldn't export images: ${err}`, true);
  }
}

// --- PDF pages panel -------------------------------------------------------

// The PDF arm of Images: a grid of pages to select and export; thumbnails
// load as their cards near the viewport.

const PDF_EXPORT_DPI = [150, 300, 600];
const PDF_THUMB_WIDTH = 200;

async function renderPdfPagesPanel() {
  const center = $("#editor-center");
  center.replaceChildren(el("div", "editor-panel editor-muted", "Reading pages…"));
  let pages;
  try {
    pages = await window.api.invoke("editor_pdf_pages", { bookId: session.bookId });
  } catch (err) {
    center.replaceChildren(wrapPanel(el("div", "editor-notice", `Couldn't read pages: ${err}`)));
    return;
  }
  if (session.panel !== "images") return; // navigated away while loading
  session.pdfPages = { pages, selected: new Set(), dpi: 300, format: "jpeg" };
  paintPdfPagesPanel();
}

// Painted once. Selection and DPI changes patch the affected nodes in place;
// a repaint drops every rendered thumbnail.
function paintPdfPagesPanel() {
  const st = session.pdfPages;
  const panel = el("div", "editor-panel");
  panel.append(el("div", "field-group-title", `Pages (${st.pages.length})`));
  panel.append(
    el("p", "editor-muted", "A PDF page is an image. Pick the pages you want and export them."),
  );

  const actions = el("div", "images-actions page-export-bar");
  actions.append(
    labeledSelect(
      "Resolution",
      PDF_EXPORT_DPI.map((d) => [d, `${d} DPI`]),
      st.dpi,
      (v) => {
        st.dpi = Number(v);
        for (const p of st.pages) st.dims.get(p.page).textContent = pageDims(p, st.dpi);
      },
    ),
  );
  actions.append(
    labeledSelect(
      "Format",
      [
        ["jpeg", "JPEG (smaller)"],
        ["png", "PNG (lossless)"],
      ],
      st.format,
      (v) => {
        st.format = v;
      },
    ),
  );

  const selectAll = el("button", "btn btn-small", "Select all");
  selectAll.type = "button";
  selectAll.addEventListener("click", () => setAllPdfPagesSelected(true));
  const clear = el("button", "btn btn-small", "Clear");
  clear.type = "button";
  clear.addEventListener("click", () => setAllPdfPagesSelected(false));

  const exportBtn = el("button", "btn btn-primary", "Export…");
  exportBtn.type = "button";
  exportBtn.addEventListener("click", exportSelectedPdfPages);

  actions.append(selectAll, clear, exportBtn);
  panel.append(actions);

  const grid = el("div", "images-grid");
  st.dims = new Map(); // page -> the dims node its DPI label lives in
  st.checks = new Map(); // page -> its checkbox
  for (const p of st.pages) grid.append(pdfPageCard(p, st));
  panel.append(grid);

  st.el = { clear, exportBtn };
  syncPdfExportBar();
  $("#editor-center").replaceChildren(panel);
  observePdfThumbs(grid);
}

// The two controls that depend on how many pages are ticked.
function syncPdfExportBar() {
  const st = session.pdfPages;
  const n = st.selected.size;
  st.el.clear.disabled = n === 0;
  st.el.exportBtn.disabled = n === 0;
  st.el.exportBtn.textContent = n ? `Export ${n} page${n === 1 ? "" : "s"}…` : "Export…";
}

function setAllPdfPagesSelected(on) {
  const st = session.pdfPages;
  st.selected.clear();
  if (on) st.pages.forEach((p) => st.selected.add(p.page));
  for (const check of st.checks.values()) check.checked = on;
  syncPdfExportBar();
}

// What one page exports as at `dpi` — points are 1/72", same maths as the
// backend's `render_page`.
function pageDims(p, dpi) {
  const px = (pt) => Math.round((pt * dpi) / 72);
  return `${px(p.width_pt)}×${px(p.height_pt)}`;
}

// A <label>Text <select>…</select></label> pair for the export bar.
function labeledSelect(label, options, value, onChange) {
  const wrap = el("label", "page-export-field");
  wrap.append(el("span", null, label));
  const sel = el("select", "input-small");
  for (const [v, text] of options) {
    const opt = el("option", null, text);
    opt.value = v;
    if (String(v) === String(value)) opt.selected = true;
    sel.append(opt);
  }
  sel.addEventListener("change", () => onChange(sel.value));
  wrap.append(sel);
  return wrap;
}

function pdfPageCard(p, st) {
  const card = el("div", "image-card");

  const thumb = el("div", "image-thumb");
  const img = el("img");
  img.alt = `Page ${p.page}`;
  img.dataset.page = p.page; // the lazy loader's handle
  thumb.append(img);
  if (p.page === 1) thumb.append(el("span", "image-badge", "Cover"));
  card.append(thumb);

  const meta = el("div", "image-meta");
  const name = el("label", "image-name page-card-name");
  const check = el("input");
  check.type = "checkbox";
  check.checked = st.selected.has(p.page);
  check.addEventListener("change", () => {
    if (check.checked) st.selected.add(p.page);
    else st.selected.delete(p.page);
    syncPdfExportBar();
  });
  st.checks.set(p.page, check);
  name.append(check, el("span", null, `Page ${p.page}`));
  meta.append(name);

  const dims = el("div", "image-dims", pageDims(p, st.dpi));
  st.dims.set(p.page, dims);
  meta.append(dims);
  card.append(meta);

  const save = el("button", "btn btn-small", "Save…");
  save.type = "button";
  save.addEventListener("click", () => savePdfPage(p.page, save));
  card.append(save);
  return card;
}

// Render each page's thumbnail once its card nears the viewport; each render
// is a ~15ms PDFKit call.
function observePdfThumbs(grid) {
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (!e.isIntersecting) continue;
        io.unobserve(e.target); // one render per card, ever
        loadPdfThumb(e.target);
      }
    },
    { root: null, rootMargin: "300px" },
  );
  for (const img of grid.querySelectorAll("img[data-page]")) io.observe(img);
}

async function loadPdfThumb(img) {
  const page = Number(img.dataset.page);
  try {
    // `reader_pdf_page` is 0-based and stateless: it re-resolves the PDF per call.
    const b64 = await window.api.invoke("reader_pdf_page", {
      bookId: session.bookId,
      page: page - 1,
      width: PDF_THUMB_WIDTH,
    });
    if (!img.isConnected) return; // panel repainted or closed mid-render
    img.src = `data:image/jpeg;base64,${b64}`;
  } catch {
    // A page that fails to render leaves an empty frame; the grid stands.
  }
}

async function savePdfPage(page, button) {
  const st = session.pdfPages;
  button.disabled = true;
  try {
    const saved = await window.api.invoke("editor_export_pdf_page", {
      bookId: session.bookId,
      page,
      dpi: st.dpi,
      format: st.format,
    });
    if (saved) toast(`Saved page ${page}.`);
  } catch (err) {
    toast(`Couldn't save page ${page}: ${err}`, true);
  }
  button.disabled = false;
}

async function exportSelectedPdfPages() {
  const st = session.pdfPages;
  const pages = [...st.selected].sort((a, b) => a - b);
  if (!pages.length) return;
  let dir;
  try {
    dir = await window.api.invoke("library_pick_folder");
  } catch (err) {
    toast(`Couldn't pick a folder: ${err}`, true);
    return;
  }
  if (!dir) return; // cancelled
  toast(`Exporting ${pages.length} page${pages.length === 1 ? "" : "s"}…`);
  try {
    const res = await window.api.invoke("editor_export_pdf_pages", {
      bookId: session.bookId,
      pages,
      dir,
      dpi: st.dpi,
      format: st.format,
    });
    toast(`Exported ${res.count} page${res.count === 1 ? "" : "s"}.`);
  } catch (err) {
    toast(`Couldn't export pages: ${err}`, true);
  }
}

// --- table-of-contents panel ----------------------------------------------

// Tiny DOM builder — avoids innerHTML for anything carrying a book's own text
// (chapter labels), which is untrusted.
function el(tag, className, text) {
  const n = document.createElement(tag);
  if (className) n.className = className;
  if (text != null) n.textContent = text;
  return n;
}

async function renderTocPanel() {
  const center = $("#editor-center");
  center.replaceChildren(el("div", "editor-panel editor-muted", "Analyzing the table of contents…"));
  let detail;
  try {
    detail = await window.api.invoke("editor_toc", { bookId: session.bookId });
  } catch (err) {
    center.replaceChildren(wrapPanel(el("div", "editor-notice", `Couldn't read the TOC: ${err}`)));
    return;
  }
  if (session.panel !== "toc") return; // navigated away while loading
  session.tocDetail = detail;
  paintTocPanel(detail);
}

function wrapPanel(...children) {
  const p = el("div", "editor-panel");
  p.append(...children);
  return p;
}

function paintTocPanel(detail) {
  // The editable model: a deep copy of the cached detail.
  session.tocTree = JSON.parse(JSON.stringify(detail.proposed || []));

  // PDF has no proposer: its panel is a hand-authoring surface; each row
  // targets a typed page number, and rows can be added.
  const pdf = detail.page_count != null;
  const pageCount = detail.page_count || 0;

  const panel = el("div", "editor-panel");

  const summary = pdf
    ? detail.nav_count
      ? `This PDF declares ${detail.nav_count} bookmark${detail.nav_count === 1 ? "" : "s"}. Edit them below, or add more.`
      : `This PDF has no table of contents. Add entries below — each jumps to a page in 1–${pageCount}.`
    : detail.verdict === "OK"
      ? "This book's table of contents already lists its chapters."
      : detail.verdict === "SUSPECT"
        ? `The declared table of contents looks deficient — ${detail.nav_count} entr${
            detail.nav_count === 1 ? "y" : "ies"
          }, none of them chapters, though the book itself carries a chapter list.`
        : detail.verdict === "FLATTENED"
          ? `This book's ${detail.flattened_volumes} volumes and their chapters are all listed at one depth. Rebuilding nests ${detail.flattened_entries} entr${
              detail.flattened_entries === 1 ? "y" : "ies"
            } under the volume each belongs to.`
          : "No machine-readable chapter list was found in the book.";
  const head = el("div", "toc-summary");
  head.append(el("span", "editor-chip", chipText(detail.verdict)));
  head.querySelector(".editor-chip").dataset.verdict = detail.verdict;
  head.append(el("p", "editor-muted", summary));
  panel.append(head);

  // Current declared TOC, with the levels the book declares.
  panel.append(el("div", "field-group-title", "Currently declared"));
  if (detail.current.length) {
    panel.append(declaredList(detail.current));
  } else {
    panel.append(el("p", "editor-muted", "No table of contents is declared."));
  }

  const total = countEntries(session.tocTree);
  panel.append(
    el(
      "div",
      "field-group-title",
      pdf
        ? `Table of contents${total ? ` (${total})` : ""}`
        : `Proposed${total ? ` (${total})` : ""}`,
    ),
  );
  if (!pdf && total) {
    panel.append(el("p", "editor-muted", proposalSummary(detail, session.tocTree)));
  }

  // PDF always gets the editable surface (that's how an absent TOC gets
  // authored); KFX/EPUB only when there's a proposal to review.
  if (total || pdf) {
    if (!total && detail.note) {
      panel.append(el("div", "editor-notice", detail.note));
    }
    const tree = el("div", "toc-proposed");
    tree.id = "toc-tree";
    panel.append(tree);

    const actions = el("div", "toc-actions");
    if (pdf) {
      const add = el("button", "btn", "Add entry");
      add.type = "button";
      add.title = "Append a new top-level entry";
      add.addEventListener("click", () => {
        session.tocTree.push({ label: "", eid: 0, href: "", page: 1, children: [] });
        // Must pass pageCount — a bare re-render drops every row's page input.
        renderProposedTree(pageCount);
        // Focus the row just added.
        const rows = $("#toc-tree").querySelectorAll(".toc-label");
        rows[rows.length - 1]?.focus();
      });
      actions.append(add);
    }
    const apply = el("button", "btn btn-primary", "Apply table of contents");
    apply.type = "button";
    apply.addEventListener("click", applyToc);
    actions.append(apply);
    if (detail.can_auto_repair) {
      const auto = el("button", "btn", "Repair automatically");
      auto.type = "button";
      auto.title = "Re-derive the TOC and write it in one step, ignoring edits above";
      auto.addEventListener("click", autoRepairToc);
      actions.append(auto);
    }
    const reset = el("button", "btn", "Reset");
    reset.type = "button";
    reset.addEventListener("click", () => paintTocPanel(session.tocDetail));
    actions.append(reset);
    panel.append(actions);

    $("#editor-center").replaceChildren(panel);
    renderProposedTree(pageCount);
  } else {
    panel.append(el("div", "editor-notice", detail.note || "No table of contents could be proposed."));
    $("#editor-center").replaceChildren(panel);
  }
}

function chipText(verdict) {
  if (verdict === "OK") return "TOC OK";
  if (verdict === "SUSPECT") return "TOC deficient";
  // A multi-work book (合本版) whose volumes and their chapters are all listed
  // at one depth — the TOC lists everything, just without its levels.
  if (verdict === "FLATTENED") return "TOC flattened";
  return "TOC sparse";
}

// The declared TOC as a nested list, read-only.
function declaredList(nodes) {
  const ul = el("ul", "toc-current");
  for (const node of nodes) {
    const li = el("li", null, node.label || "(untitled)");
    if (node.children && node.children.length) li.append(declaredList(node.children));
    ul.append(li);
  }
  return ul;
}

// What the proposal changes, in one line: added entries and added levels;
// every declared entry stays.
function proposalSummary(detail, tree) {
  const added = countEntries(tree) - detail.nav_count;
  const levels = treeDepth(tree);
  const declaredLevels = treeDepth(detail.current);
  const parts = [];
  if (added > 0) parts.push(`adds ${added} entr${added === 1 ? "y" : "ies"} the book links but doesn't declare`);
  if (levels > declaredLevels) parts.push(`nests them ${levels} levels deep`);
  if (!parts.length) return "Identical to what the book declares — nothing to change.";
  return `Keeps every declared entry and ${parts.join(", ")}.`;
}

function treeDepth(nodes) {
  return (nodes || []).reduce((d, n) => Math.max(d, 1 + treeDepth(n.children)), 0);
}

// The tree as a flat run of `{node, depth}` in reading order.
function flattenTree(nodes, depth = 0, out = []) {
  for (const node of nodes) {
    out.push({ node, depth });
    flattenTree(node.children || [], depth + 1, out);
  }
  return out;
}

// Rebuild a tree from `{node, depth}` rows. Each row attaches under the nearest
// preceding row with a shallower depth; a depth deeper than one below its
// predecessor clamps.
function rebuildTree(rows) {
  const roots = [];
  const open = []; // ancestors of the current row, outermost first
  for (const { node, depth } of rows) {
    node.children = [];
    while (open.length > depth) open.pop();
    if (open.length) open[open.length - 1].children.push(node);
    else roots.push(node);
    open.push(node);
  }
  return roots;
}

// The rows of `index`'s subtree: itself plus everything after it that is deeper.
function subtreeEnd(rows, index) {
  let end = index + 1;
  while (end < rows.length && rows[end].depth > rows[index].depth) end++;
  return end;
}

// Move entry `index` (and its sub-entries) one level in or out.
function shiftTocDepth(index, delta, pageCount) {
  const rows = flattenTree(session.tocTree);
  const row = rows[index];
  if (!row || !canShift(rows, index, delta)) return;
  const end = subtreeEnd(rows, index);
  for (let i = index; i < end; i++) rows[i].depth += delta;
  session.tocTree = rebuildTree(rows);
  renderProposedTree(pageCount, index);
}

// An entry can only indent under something, and can only outdent while it has a
// level to give up.
function canShift(rows, index, delta) {
  if (delta < 0) return rows[index].depth > 0;
  return index > 0 && rows[index].depth <= rows[index - 1].depth;
}

// Where `index`'s previous sibling starts, or -1 when it is the first child.
function prevSibling(rows, index) {
  const depth = rows[index].depth;
  for (let j = index - 1; j >= 0; j--) {
    if (rows[j].depth === depth) return j;
    if (rows[j].depth < depth) return -1;
  }
  return -1;
}

// Where `index`'s next sibling starts, or -1 when it is the last child. The row
// after this subtree is either the next sibling (equal depth) or an ancestor's
// continuation (shallower) — `subtreeEnd` guarantees it can't be deeper.
function nextSibling(rows, index) {
  const end = subtreeEnd(rows, index);
  return end < rows.length && rows[end].depth === rows[index].depth ? end : -1;
}

// Move entry `index` (and its sub-entries) past the sibling above or below it.
function moveTocEntry(index, delta, pageCount) {
  const rows = flattenTree(session.tocTree);
  if (!rows[index]) return;
  const target = delta < 0 ? prevSibling(rows, index) : nextSibling(rows, index);
  if (target < 0) return;
  const block = rows.splice(index, subtreeEnd(rows, index) - index);
  // Moving up, the target sits above the cut at an unchanged index. Moving
  // down, the next sibling slides into the vacated slot; the block belongs
  // after the whole of it, sub-entries included.
  const at = delta < 0 ? target : subtreeEnd(rows, index);
  rows.splice(at, 0, ...block);
  session.tocTree = rebuildTree(rows);
  renderProposedTree(pageCount, at);
}

// Render #toc-tree from session.tocTree, indented by depth.
function renderProposedTree(pageCount = 0, focusIndex = -1) {
  const tree = $("#toc-tree");
  if (!tree) return;
  const rows = flattenTree(session.tocTree);
  tree.replaceChildren(
    ...rows.map(({ node, depth }, i) =>
      tocRow(node, depth, pageCount, {
        canIndent: canShift(rows, i, +1),
        canOutdent: canShift(rows, i, -1),
        canMoveUp: prevSibling(rows, i) >= 0,
        canMoveDown: nextSibling(rows, i) >= 0,
        onIndent: () => shiftTocDepth(i, +1, pageCount),
        onOutdent: () => shiftTocDepth(i, -1, pageCount),
        onMoveUp: () => moveTocEntry(i, -1, pageCount),
        onMoveDown: () => moveTocEntry(i, +1, pageCount),
        onRemove: () => {
          rows.splice(i, subtreeEnd(rows, i) - i);
          session.tocTree = rebuildTree(rows);
          renderProposedTree(pageCount);
        },
      }),
    ),
  );
  if (focusIndex >= 0) tree.querySelectorAll(".toc-label")[focusIndex]?.focus();
}

// One editable row bound to its model node: a label input, a page input for PDF, the move controls.
function tocRow(node, depth, pageCount, ops) {
  const row = el("div", "toc-row");
  if (depth) {
    row.style.marginLeft = `${depth * 20}px`;
    row.classList.add("toc-child");
  }
  const input = el("input", "toc-label");
  input.value = node.label;
  input.placeholder = "Chapter title";
  input.addEventListener("input", () => {
    node.label = input.value;
  });
  // Tab / Shift+Tab change an entry's level, Alt+↑ / Alt+↓ its order among its siblings.
  input.addEventListener("keydown", (e) => {
    if (e.key === "Tab") {
      const shift = e.shiftKey ? ops.onOutdent : ops.onIndent;
      const allowed = e.shiftKey ? ops.canOutdent : ops.canIndent;
      if (!allowed) return; // fall through to normal focus movement
      e.preventDefault();
      shift();
      return;
    }
    if (!e.altKey || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
    const up = e.key === "ArrowUp";
    if (!(up ? ops.canMoveUp : ops.canMoveDown)) return;
    e.preventDefault();
    (up ? ops.onMoveUp : ops.onMoveDown)();
  });
  row.append(input);

  if (pageCount > 0) {
    const page = el("input", "toc-page");
    page.type = "number";
    page.min = "1";
    page.max = String(pageCount);
    page.value = String(node.page || 1);
    page.title = `Page this entry jumps to (1–${pageCount})`;
    page.addEventListener("input", () => {
      // Clamp here: the writer rejects the whole TOC over one out-of-range page.
      const n = Math.min(Math.max(parseInt(page.value, 10) || 1, 1), pageCount);
      node.page = n;
    });
    page.addEventListener("blur", () => {
      page.value = String(node.page || 1);
    });
    row.append(el("span", "toc-page-label", "p."), page);
  }

  const sub = node.children && node.children.length ? " and its sub-entries" : "";
  const moveUp = el("button", "toc-nudge", "↑");
  moveUp.type = "button";
  moveUp.title = `Move this entry${sub} up (Alt+↑)`;
  moveUp.disabled = !ops.canMoveUp;
  moveUp.addEventListener("click", ops.onMoveUp);

  const moveDown = el("button", "toc-nudge", "↓");
  moveDown.type = "button";
  moveDown.title = `Move this entry${sub} down (Alt+↓)`;
  moveDown.disabled = !ops.canMoveDown;
  moveDown.addEventListener("click", ops.onMoveDown);

  const outdent = el("button", "toc-nudge", "⇤");
  outdent.type = "button";
  outdent.title = "Move out one level (Shift+Tab)";
  outdent.disabled = !ops.canOutdent;
  outdent.addEventListener("click", ops.onOutdent);

  const indent = el("button", "toc-nudge", "⇥");
  indent.type = "button";
  indent.title = "Make this a sub-entry of the one above (Tab)";
  indent.disabled = !ops.canIndent;
  indent.addEventListener("click", ops.onIndent);

  const remove = el("button", "toc-remove", "×");
  remove.type = "button";
  remove.title = `Remove this entry${sub}`;
  remove.addEventListener("click", ops.onRemove);

  // One cluster: five controls as the row's toolbar.
  const toolbar = el("div", "toc-ops");
  toolbar.append(moveUp, moveDown, outdent, indent, remove);
  row.append(toolbar);
  return row;
}

function countEntries(nodes) {
  return nodes.reduce((n, e) => n + 1 + countEntries(e.children || []), 0);
}

function anyBlankLabel(nodes) {
  return nodes.some((e) => !e.label.trim() || anyBlankLabel(e.children || []));
}

async function applyToc() {
  if (!session.tocTree.length) {
    toast("Add at least one entry.", true);
    return;
  }
  if (anyBlankLabel(session.tocTree)) {
    toast("Every entry needs a label.", true);
    return;
  }
  await runTocWrite(() =>
    window.api.invoke("editor_set_toc", {
      bookId: session.bookId,
      entries: session.tocTree,
    }),
  );
}

async function autoRepairToc() {
  await runTocWrite(() =>
    window.api.invoke("editor_repair_toc", { bookId: session.bookId }),
  );
}

// Shared commit wrapper for both TOC write paths: disable the buttons, invoke,
// then repaint from the fresh verdict + refresh the top-bar chip.
async function runTocWrite(invoke) {
  const buttons = $("#editor-center").querySelectorAll(".toc-actions button");
  buttons.forEach((b) => (b.disabled = true));
  try {
    const detail = await invoke();
    session.tocDetail = detail;
    renderTocChip({
      verdict: detail.verdict,
      nav_count: detail.nav_count,
      nav_chapters: detail.nav_chapters,
    });
    paintTocPanel(detail);
    // Every source format re-derives its counterpart on save; name the right one.
    const derived = session.data.format === "kfx" ? "EPUB" : "Kindle file";
    toast(
      detail.verdict === "OK"
        ? `Table of contents written — regenerating the derived ${derived}…`
        : "Table of contents written.",
    );
  } catch (err) {
    toast(`Couldn't write the TOC: ${err}`, true);
    buttons.forEach((b) => (b.disabled = false));
  }
}

// --- reading order (spine) panel -------------------------------------------

async function renderSpinePanel() {
  const center = $("#editor-center");
  center.replaceChildren(
    el("div", "editor-panel editor-muted", "Reading the book's reading order…"),
  );
  let detail;
  try {
    detail = await window.api.invoke("editor_spine", { bookId: session.bookId });
  } catch (err) {
    center.replaceChildren(
      wrapPanel(el("div", "editor-notice", `Couldn't read the reading order: ${err}`)),
    );
    return;
  }
  if (session.panel !== "spine") return; // navigated away while loading
  session.spineDetail = detail;
  paintSpinePanel(detail);
}

function paintSpinePanel(detail) {
  // A deep copy of the cached detail.
  session.spineOrder = JSON.parse(JSON.stringify(detail.proposed || []));

  const panel = el("div", "editor-panel");
  const wrong = detail.verdict === "MISORDERED";

  const head = el("div", "toc-summary");
  const chip = el("span", "editor-chip", wrong ? "Order wrong" : "Order OK");
  chip.dataset.verdict = wrong ? "SUSPECT" : "OK";
  head.append(chip);
  head.append(el("p", "editor-muted", spineSummary(detail)));
  panel.append(head);

  // The consequence, stated before the controls.
  panel.append(
    el(
      "div",
      "editor-notice",
      "Reading positions are numbered along the reading order, so changing it " +
        "renumbers them. Bookmarks, highlights and how far through the book you " +
        "are will shift for this book.",
    ),
  );

  panel.append(el("div", "field-group-title", "Reading order"));
  panel.append(
    el(
      "p",
      "editor-muted",
      "Entries the table of contents names are listed by their chapter title. " +
        "The rest — plates, blanks, pages no chapter list mentions — show their " +
        "filename, and travel with the document above them unless you move them.",
    ),
  );
  const list = el("div", "toc-proposed");
  list.id = "spine-list";
  panel.append(list);

  const actions = el("div", "toc-actions");
  const apply = el("button", "btn btn-primary", "Apply reading order");
  apply.type = "button";
  apply.id = "spine-apply";
  apply.addEventListener("click", applySpine);
  const reset = el("button", "btn", "Reset");
  reset.type = "button";
  reset.title = "Back to the order the book's navigation implies";
  reset.addEventListener("click", () => paintSpinePanel(session.spineDetail));
  actions.append(apply, reset);
  panel.append(actions);

  $("#editor-center").replaceChildren(panel);
  renderSpineList();
}

function spineSummary(detail) {
  if (detail.verdict !== "MISORDERED") {
    return "This book reads its chapters in the order its table of contents lists them.";
  }
  const late = detail.first_out_of_order
    ? `, starting with ${detail.first_out_of_order}`
    : "";
  const sorted = detail.machine_sorted
    ? " Its reading order is its own file list in alphabetical order — a packaging" +
      " artifact rather than an order anyone chose."
    : "";
  return (
    `This book reads its chapters out of the order its table of contents lists` +
    ` them, in ${detail.descents} place${detail.descents === 1 ? "" : "s"}${late}.` +
    ` The order below moves ${detail.moved} document${detail.moved === 1 ? "" : "s"}.` +
    sorted
  );
}

// Full re-render after every move; each row's closure binds its index, the
// same rule the TOC tree follows.
function renderSpineList(focusIndex = -1) {
  const list = $("#spine-list");
  if (!list) return;
  const order = session.spineOrder;
  list.replaceChildren(
    ...order.map((doc, i) =>
      spineRow(doc, {
        canMoveUp: i > 0,
        canMoveDown: i < order.length - 1,
        onMoveUp: () => moveSpineDoc(i, -1),
        onMoveDown: () => moveSpineDoc(i, +1),
      }),
    ),
  );
  const apply = $("#spine-apply");
  if (apply) {
    // The backend refuses an order the book declares; the button stays
    // disabled for it.
    const current = (session.spineDetail.current || []).map((d) => d.idref);
    apply.disabled = order.every((d, i) => d.idref === current[i]);
    apply.title = apply.disabled
      ? "This is the order the book already reads in"
      : "";
  }
  if (focusIndex >= 0) {
    list.querySelectorAll(".toc-nudge")[focusIndex * 2]?.focus();
  }
}

// One row: the document's name (not editable — a document's identity isn't a
// label the reader gets to change) and the two controls that move it.
function spineRow(doc, ops) {
  const row = el("div", "toc-row");
  const name = el("span", doc.named ? "spine-label" : "spine-label spine-unnamed", doc.label);
  name.title = doc.named
    ? `Listed in the table of contents as “${doc.label}”`
    : `${doc.label} — no table-of-contents entry names this document`;
  row.append(name);

  const moveUp = el("button", "toc-nudge", "↑");
  moveUp.type = "button";
  moveUp.title = "Move earlier in the book";
  moveUp.disabled = !ops.canMoveUp;
  moveUp.addEventListener("click", ops.onMoveUp);

  const moveDown = el("button", "toc-nudge", "↓");
  moveDown.type = "button";
  moveDown.title = "Move later in the book";
  moveDown.disabled = !ops.canMoveDown;
  moveDown.addEventListener("click", ops.onMoveDown);

  const ops_ = el("div", "toc-ops");
  ops_.append(moveUp, moveDown);
  row.append(ops_);
  return row;
}

// A spine is flat: a move is a swap with the neighbour.
function moveSpineDoc(index, delta) {
  const order = session.spineOrder;
  const to = index + delta;
  if (to < 0 || to >= order.length) return;
  [order[index], order[to]] = [order[to], order[index]];
  renderSpineList(to);
}

async function applySpine() {
  const order = session.spineOrder.map((d) => d.idref);
  const buttons = $("#editor-center").querySelectorAll(".toc-actions button");
  buttons.forEach((b) => (b.disabled = true));
  try {
    const detail = await window.api.invoke("editor_set_spine", {
      bookId: session.bookId,
      order,
    });
    session.spineDetail = detail;
    paintSpinePanel(detail);
    const derived = session.data.format === "kfx" ? "EPUB" : "Kindle file";
    toast(`Reading order written — regenerating the derived ${derived}…`);
  } catch (err) {
    toast(`Couldn't write the reading order: ${err}`, true);
    buttons.forEach((b) => (b.disabled = false));
  }
}

// --- panel dispatch --------------------------------------------------------

function saveCurrentPanel() {
  if (session.panel === "metadata") return saveMetadata();
  if (session.panel === "text") return textPanel?.save();
}

function revertCurrentPanel() {
  if (session.panel === "metadata") renderMetadataPanel();
  if (session.panel === "text") {
    textPanel?.revert();
    markDirty(textPanel?.isDirty() || false);
    return;
  }
  markDirty(false);
}

// --- wiring ----------------------------------------------------------------

function wire() {
  $("#editor-close").addEventListener("click", () => close());
  $("#editor-save").addEventListener("click", () => saveCurrentPanel());
  $("#editor-revert").addEventListener("click", () => revertCurrentPanel());
  for (const item of document.querySelectorAll(".editor-rail-item")) {
    item.addEventListener("click", () => {
      if (!item.disabled) selectPanel(item.dataset.panel);
    });
  }
}

wire();

window.sidleEditor = { open, close, isOpen };
