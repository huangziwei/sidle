// kfx-book.js — adapt a `reader_open` DTO into the foliate paginator's "book
// interface": { dir, sections: [{ load, unload, linear, size }] }.
//
// boko returns each section as a complete XHTML string plus the non-spine
// resources (style.css, images) as base64. The paginator loads each section
// into a sandboxed iframe *by URL* (no base path), so before handing it a blob
// URL we rewrite the section's relative resource refs (`style.css`,
// `image_e9.jpg`, SVG `xlink:href`) to blob: URLs built from those resources.
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
`;

export function makeKfxBook(dto) {
  // href → blob: URL for every non-spine resource.
  const resourceUrls = new Map();
  for (const r of dto.resources) {
    const url = URL.createObjectURL(
      new Blob([b64ToBytes(r.data_base64)], { type: r.mime || "application/octet-stream" }),
    );
    resourceUrls.set(r.href, url);
  }

  const sectionBlobs = new Map(); // index → live section blob URL (for unload)

  const rewriteRefs = (html) => {
    const doc = new DOMParser().parseFromString(html, "text/html");
    const fix = (el, attr) => {
      const v = el.getAttribute(attr);
      if (v && resourceUrls.has(v)) el.setAttribute(attr, resourceUrls.get(v));
    };
    doc.querySelectorAll("link[href]").forEach((el) => fix(el, "href"));
    doc.querySelectorAll("img[src]").forEach((el) => fix(el, "src"));
    // SVG cover wrapper references the cover via xlink:href (and/or href).
    doc.querySelectorAll("image").forEach((el) => {
      fix(el, "href");
      const xl = el.getAttributeNS(XLINK, "href") || el.getAttribute("xlink:href");
      if (xl && resourceUrls.has(xl)) el.setAttributeNS(XLINK, "href", resourceUrls.get(xl));
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

    // Free every blob URL (resources + any live section) when closing.
    destroy() {
      for (const url of resourceUrls.values()) URL.revokeObjectURL(url);
      for (const url of sectionBlobs.values()) URL.revokeObjectURL(url);
      resourceUrls.clear();
      sectionBlobs.clear();
    },
  };
}
