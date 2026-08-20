// kfx-book.js — adapt a `reader_open` DTO into the foliate paginator's "book
// interface": { dir, sections: [{ load, unload, linear, size }] }.
//
// bokai returns each section as a complete XHTML string plus the non-spine
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

// `url(...)` in a stylesheet, capturing the target with its optional quotes.
const CSS_URL = /url\(\s*([^)]*?)\s*\)/gi;

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
/* Inline glyph images (rare hanzi drawn as a picture because they have no
   Unicode code point) are full-width ideographs sized to the font. Default
   baseline alignment sits the box bottom on the alphabetic baseline, so the
   image rides up above the hanzi; middle (x-height center) drops it too low.
   Align the box bottom to the font's descent line — where the surrounding
   hanzi bottoms sit — so the glyph shares their em-box bottom, font-agnostic. */
img[data-kfx-inline] { vertical-align: text-bottom; }
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

// `loader` (optional): the reader's deferred-content interface —
// { resolve(href) → blob url | null, known: Map<href, {mime,width,height}>,
//   requireSection(index) → Promise<html|null> }. `resolve`/`known` cover the
// lazily-streamed images; `requireSection` fetches a section whose HTML was
// withheld from the open DTO (large text books ship only the resume window —
// the paginator awaits `load()`, so a jump ahead of the stream just waits a
// beat). Without a loader, only the DTO's inline content resolves.
export function makeKfxBook(dto, loader) {
  // href → blob: URL for every eagerly-shipped non-spine resource.
  const resourceUrls = new Map();
  // Stylesheets are held as text, not published straight to a blob: a rule
  // like `hr.transition { background: url(image_e1.jpg) }` (how publishers
  // draw section-break ornaments) needs that href turned into a blob URL of
  // its own, because a sheet served from blob: has no base path to resolve
  // relative refs against. The image bytes stream in lazily, so the sheet is
  // republished as they land — see `restyle`.
  const sheets = new Map(); // href → { text, url, pending }
  for (const r of dto.resources) {
    const bytes = b64ToBytes(r.data_base64);
    const mime = r.mime || "application/octet-stream";
    if (mime.startsWith("text/css")) {
      sheets.set(r.href, { text: new TextDecoder().decode(bytes), url: null, pending: true });
      continue;
    }
    resourceUrls.set(r.href, URL.createObjectURL(new Blob([bytes], { type: mime })));
  }

  const sectionBlobs = new Map(); // index → live section blob URL (for unload)
  // Superseded stylesheet blobs. A document that has not yet swapped to the
  // republished sheet is still reading one of these, so they are only freed
  // when the book closes.
  const staleSheetUrls = [];

  // Ready URL for href, from the inline resources or the lazy loader.
  const urlFor = (v) => {
    const sheet = sheets.get(v);
    if (sheet) return sheetUrl(sheet);
    return resourceUrls.get(v) || (loader ? loader.resolve(v) : null) || null;
  };

  // Publish a stylesheet with its `url()` refs bound to blob URLs. While any
  // ref is still unresolved the sheet is rebuilt on each request, so a
  // section loading later picks up whatever arrived since; an unchanged
  // rebuild keeps the existing blob so nothing churns. Superseded blobs stay
  // alive until close — a live document may still be reading one.
  const sheetUrl = (sheet) => {
    if (sheet.url && !sheet.pending) return sheet.url;
    let pending = false;
    const text = sheet.text.replace(CSS_URL, (whole, ref) => {
      const target = ref.trim().replace(/^["']|["']$/g, "");
      // Absolute, protocol-relative and fragment refs address something
      // other than a bundled asset; they are left exactly as authored.
      if (!target || /^(?:[a-z][a-z0-9+.-]*:|\/\/|#)/i.test(target)) return whole;
      const url = resourceUrls.get(target) || (loader ? loader.resolve(target) : null);
      if (url) return `url("${url}")`;
      pending = true;
      return whole;
    });
    sheet.pending = pending;
    if (sheet.url && text === sheet.built) return sheet.url;
    if (sheet.url) staleSheetUrls.push(sheet.url);
    sheet.built = text;
    sheet.url = URL.createObjectURL(new Blob([text], { type: "text/css" }));
    return sheet.url;
  };

  // Re-point every live document at a rebuilt stylesheet, for images that
  // arrived after those sections rendered — the `patchPendingImages` of CSS
  // backgrounds. No-op once every ref in the sheet has resolved.
  const restyle = (docs) => {
    for (const [href, sheet] of sheets) {
      if (!sheet.pending) continue;
      const before = sheet.url;
      const url = sheetUrl(sheet);
      if (url === before) continue;
      for (const doc of docs) {
        for (const link of doc.querySelectorAll("link[data-kfx-sheet]")) {
          if (link.getAttribute("data-kfx-sheet") === href) link.setAttribute("href", url);
        }
      }
    }
  };

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
    doc.querySelectorAll("link[href]").forEach((el) => {
      // Remember which sheet this link is, so `restyle` can re-point it once
      // the images its rules paint have streamed in.
      const v = el.getAttribute("href");
      if (v && sheets.has(v)) el.setAttribute("data-kfx-sheet", v);
      fix(el, "href");
    });
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
    //
    // The reset must reach EVERY element, not just html/body: an official-Amazon
    // KFX can nest the image inside a block whose style carries the KFX
    // `box_align: center` + `writing_mode: vertical_rl` pair (kfx_to_epub renders
    // box_align as `text-align: center`, following calibre). In that vertical
    // wrapper the horizontal axis is the *block* axis, where neither
    // `text-align` nor `margin: 0 auto` can center — so the image stays pinned to
    // the right edge unless the wrapper itself is flattened to horizontal-tb.
    // Safe to blanket the whole subtree here because the section is text-free.
    //
    // The wrapper block-margins MUST go too: the book often boxes the image in a
    // <div> carrying a top/bottom margin (e.g. `.imagef { margin-top: 0.5em }`).
    // The paginator caps the image at the full column height, so image + that
    // margin exceeds the column by the margin — and since `break-inside: avoid`
    // can't be honored for a block taller than the fragmentainer, WebKit splits
    // the image, filling one page and spilling a thin sliver onto the next.
    // Zeroing the descendants' block margins/padding keeps the full-page image
    // truly full-bleed AND within one column. (Inline `margin: 0 auto` centering
    // on the image survives — we zero only the block axis.)
    const text = (doc.body?.textContent || "").replace(/\s+/g, "");
    if (!text && doc.body?.querySelector("img, image, svg")) {
      const fb = doc.createElement("style");
      fb.textContent =
        "html, body, body * { writing-mode: horizontal-tb !important; direction: ltr !important; }" +
        "body { margin: 0 !important; text-align: center !important; }" +
        "body * { margin-top: 0 !important; margin-bottom: 0 !important; padding-top: 0 !important; padding-bottom: 0 !important; }" +
        "img, image, svg { display: block !important; margin: 0 auto !important; }";
      doc.head.appendChild(fb);
    }
    return "<!DOCTYPE html>\n" + doc.documentElement.outerHTML;
  };

  const sections = dto.sections.map((s, index) => ({
    id: s.href,
    linear: "yes",
    // Byte weight so the paginator's progress fraction is meaningful — from
    // the manifest, valid whether or not the HTML shipped inline.
    size: s.size || s.html?.length || 1,
    // The paginator awaits load(), so a withheld section (html == null)
    // simply fetches before its first render. Arrived HTML is written back
    // to the DTO by the section loader, so later loads are synchronous.
    load: async () => {
      const html = s.html != null ? s.html : await loader?.requireSection?.(index);
      const url = URL.createObjectURL(
        new Blob([rewriteRefs(html ?? "<html><body></body></html>")], { type: "text/html" }),
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

    // Rebuild any stylesheet still waiting on images and re-point the given
    // live documents at it — the `patchPendingImages` of CSS backgrounds.
    restyle,

    // Free every blob URL (inline resources + any live section) when closing.
    // Lazily-fetched image blobs belong to the resource loader, not us.
    destroy() {
      for (const url of resourceUrls.values()) URL.revokeObjectURL(url);
      for (const url of sectionBlobs.values()) URL.revokeObjectURL(url);
      for (const sheet of sheets.values()) {
        if (sheet.url) URL.revokeObjectURL(sheet.url);
      }
      for (const url of staleSheetUrls) URL.revokeObjectURL(url);
      resourceUrls.clear();
      sectionBlobs.clear();
      sheets.clear();
      staleSheetUrls.length = 0;
    },
  };
}
