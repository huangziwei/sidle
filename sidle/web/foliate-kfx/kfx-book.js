// kfx-book.js — adapt a `reader_open` DTO into the foliate paginator's "book
// interface": { dir, sections: [{ load, unload, linear, size }] }.
//
// boko returns each section as a complete XHTML string plus the non-spine
// resources. `style.css` arrives inline (base64) with the DTO; images arrive
// *lazily* — the DTO carries only their manifest (href/mime/size), and the
// reader's resource loader streams the bytes in around the reading position.
// The paginator loads each section into a sandboxed iframe *by URL* (no base
// path), so before handing it a blob URL we rewrite the section's relative
// resource refs (`style.css`, `image_e9.jpg`, SVG `xlink:href`) to blob: URLs.
// An image whose bytes haven't landed yet gets a same-size SVG placeholder
// (so pagination doesn't shift when the real pixels swap in) and a
// `data-kfx-src` marker; `patchPendingImages` swaps arrivals into live docs.
// Internal chapter links (`c5.xhtml#x`) are left alone — they're navigation.

const b64ToBytes = (b64) => Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));

const XLINK = "http://www.w3.org/1999/xlink";

// Injected into every section: fit images/SVG to the page so illustrations and
// the cover aren't squeezed, stretched, or cropped. Appended last so it wins
// over the book's own stylesheet. Page COLORS, font, and color-scheme are NOT
// set here — the reader's per-book style settings own those via the paginator's
// `setStyles` hook (a later <style>, so it overrides), which lets them change
// live as the user drags a slider.
const READER_CSS = `
img, image, svg, video {
  max-width: 100% !important;
  max-height: 100% !important;
  width: auto;
  height: auto;
  object-fit: contain;
}
img[data-kfx-src], image[data-kfx-src] { opacity: 0.15; }
`;

// A transparent SVG data URI with the manifest's intrinsic size, so the layout
// box an unloaded image reserves matches the real image under the contain
// rules above (no pagination jump on swap). 600×800 when the KFX carries no
// dimensions.
function placeholderUrl(dims) {
  const w = dims?.width || 600;
  const h = dims?.height || 800;
  return `data:image/svg+xml,${encodeURIComponent(
    `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${h}"/>`,
  )}`;
}

// Swap every pending image in `doc` whose bytes have since arrived.
// `resolve(href)` → blob URL or null. Safe on dead/detached docs (no-op).
export function patchPendingImages(doc, resolve) {
  for (const el of doc.querySelectorAll("[data-kfx-src]")) {
    const url = resolve(el.getAttribute("data-kfx-src"));
    if (!url) continue;
    if (el.localName === "image") {
      el.setAttributeNS(XLINK, "href", url);
      el.setAttribute("href", url);
    } else {
      el.setAttribute("src", url);
    }
    el.removeAttribute("data-kfx-src");
  }
}

// `loader` (optional): the reader's resource loader — { resolve(href) → url |
// null, known: Map<href, {mime,width,height}> }. Without one, only the DTO's
// inline resources resolve (offline/legacy use).
export function makeKfxBook(dto, loader) {
  // href → blob: URL for every eagerly-shipped non-spine resource (style.css).
  const resourceUrls = new Map();
  for (const r of dto.resources) {
    const url = URL.createObjectURL(
      new Blob([b64ToBytes(r.data_base64)], { type: r.mime || "application/octet-stream" }),
    );
    resourceUrls.set(r.href, url);
  }

  const sectionBlobs = new Map(); // index → live section blob URL (for unload)

  // Ready URL for href, from the inline resources or the lazy loader.
  const urlFor = (v) =>
    resourceUrls.get(v) || (loader ? loader.resolve(v) : null) || null;

  const rewriteRefs = (html) => {
    const doc = new DOMParser().parseFromString(html, "text/html");
    const fix = (el, attr) => {
      const v = el.getAttribute(attr);
      if (!v) return;
      const url = urlFor(v);
      if (url) {
        el.setAttribute(attr, url);
      } else if (loader?.known.has(v)) {
        // Bytes still in flight: reserve the box, mark for patching.
        el.setAttribute("data-kfx-src", v);
        el.setAttribute(attr, placeholderUrl(loader.known.get(v)));
      }
    };
    doc.querySelectorAll("link[href]").forEach((el) => fix(el, "href"));
    doc.querySelectorAll("img[src]").forEach((el) => fix(el, "src"));
    // SVG cover wrapper references the cover via xlink:href (and/or href).
    doc.querySelectorAll("image").forEach((el) => {
      const xl = el.getAttributeNS(XLINK, "href") || el.getAttribute("xlink:href");
      const v = el.getAttribute("href") || xl;
      if (!v) return;
      const url = urlFor(v);
      if (url) {
        el.setAttributeNS(XLINK, "href", url);
        el.setAttribute("href", url);
      } else if (loader?.known.has(v)) {
        el.setAttribute("data-kfx-src", v);
        const ph = placeholderUrl(loader.known.get(v));
        el.setAttributeNS(XLINK, "href", ph);
        el.setAttribute("href", ph);
      }
    });
    // Reader stylesheet, appended last so it overrides the book's own styles.
    const style = doc.createElement("style");
    style.textContent = READER_CSS;
    doc.head.appendChild(style);
    // A full-page-image section (cover, full-bleed art) has no text. Force
    // horizontal single-block flow so the paginator lays the image across the
    // whole page — in a vertical-rl book the default columnar flow would trap a
    // single image in one column pinned to the right edge instead of filling.
    // Read by `getDirection` (which runs after this CSS applies), so the whole
    // section paginates as horizontal; the reader pairs this with a zero-margin
    // single-column layout (see `applyLayout`).
    const text = (doc.body?.textContent || "").replace(/\s+/g, "");
    if (!text && doc.body?.querySelector("img, image, svg")) {
      const fb = doc.createElement("style");
      fb.textContent =
        "html, body { writing-mode: horizontal-tb !important; direction: ltr !important; }" +
        "body { margin: 0 !important; text-align: center !important; }" +
        "img, image, svg { display: block !important; margin: 0 auto !important; }";
      doc.head.appendChild(fb);
    }
    return "<!DOCTYPE html>\n" + doc.documentElement.outerHTML;
  };

  const sections = dto.sections.map((s, index) => ({
    id: s.href,
    linear: "yes",
    // Rough byte weight so the paginator's progress fraction is meaningful.
    size: s.html.length || 1,
    load: () => {
      const url = URL.createObjectURL(
        new Blob([rewriteRefs(s.html)], { type: "text/html" }),
      );
      sectionBlobs.set(index, url);
      return url;
    },
    unload: () => {
      const url = sectionBlobs.get(index);
      if (url) {
        URL.revokeObjectURL(url);
        sectionBlobs.delete(index);
      }
    },
  }));

  return {
    dir: dto.page_progression_direction === "rtl" ? "rtl" : "ltr",
    sections,
    toc: dto.toc || [],
    metadata: { title: dto.title, authors: dto.authors || [], language: dto.language },
    writingMode: dto.writing_mode,
    ppd: dto.page_progression_direction,
    // index ↔ href, for resolving TOC targets and annotation sections.
    hrefs: dto.sections.map((s) => s.href),

    // Free every blob URL (inline resources + any live section) when closing.
    // Lazily-fetched image blobs belong to the resource loader, not us.
    destroy() {
      for (const url of resourceUrls.values()) URL.revokeObjectURL(url);
      for (const url of sectionBlobs.values()) URL.revokeObjectURL(url);
      resourceUrls.clear();
      sectionBlobs.clear();
    },
  };
}
