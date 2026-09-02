import { stampLines } from "./editor-text.js";

const XLINK = "http://www.w3.org/1999/xlink";
const CSS_URL = /url\(\s*([^)]*?)\s*\)/gi;
const EXTERNAL = /^(?:[a-z][a-z0-9+.-]*:|\/\/|#)/i;

const EDITOR_CSS = `
.sidle-editor-target { outline: 2px solid #c83e3e !important; outline-offset: 1px; }
img, image, svg, video { max-width: 100%; }
`;

export function dirOf(path) {
  const i = path.lastIndexOf("/");
  return i < 0 ? "" : path.slice(0, i + 1);
}

export function resolvePath(baseDir, href) {
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

export function mimeOf(path) {
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

export function makePreview({ iframe, readBytes, readText, onClick }) {
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
