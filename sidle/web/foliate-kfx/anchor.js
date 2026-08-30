// anchor.js — resolve a KFX annotation's (eid, char-offset) anchor to a DOM
// Range inside a rendered section. Pure given the section `doc` + annotation,
// so it's unit-testable independent of the reader coordinator.

// Element's descendant text nodes in document order, skipping ruby annotation
// text (<rt>/<rp>) but keeping ruby base (<rb>) and ordinary text.
export function* baseTextNodes(el) {
  for (const child of el.childNodes) {
    if (child.nodeType === Node.TEXT_NODE) {
      yield child;
    } else if (child.nodeType === Node.ELEMENT_NODE) {
      const tag = child.nodeName.toLowerCase();
      if (tag === "rt" || tag === "rp") continue;
      yield* baseTextNodes(child);
    }
  }
}

// Map a character offset within an element's base text to a (textNode, offset)
// boundary. Clamps to the element's end if the offset overruns.
export function textBoundary(el, charIndex) {
  let remaining = charIndex;
  let last = null;
  for (const node of baseTextNodes(el)) {
    const len = node.nodeValue.length;
    last = node;
    if (remaining <= len) return { node, offset: remaining };
    remaining -= len;
  }
  if (last) return { node: last, offset: last.nodeValue.length };
  return null;
}

// Build a DOM Range for an annotation within `doc`, or null if its eids aren't
// in this section. Kindle end offsets are inclusive, so the end boundary is
// off_end + 1 (the Range itself is half-open).
export function rangeFor(doc, ann) {
  if (ann.eid_start == null) return null;
  const startEl = doc.querySelector(`[data-eid="${ann.eid_start}"]`);
  const endEl =
    ann.eid_end != null ? doc.querySelector(`[data-eid="${ann.eid_end}"]`) : startEl;
  if (!startEl || !endEl) return null;
  const start = textBoundary(startEl, ann.off_start ?? 0);
  const end = textBoundary(endEl, (ann.off_end ?? 0) + 1);
  if (!start || !end) return null;
  const range = doc.createRange();
  try {
    range.setStart(start.node, start.offset);
    range.setEnd(end.node, end.offset);
  } catch {
    return null;
  }
  return range;
}

// ---- reverse: DOM selection → KFX (eid, offset) anchor ----------------------

// Concatenated base text of an element (ruby <rt>/<rp> excluded).
function baseString(el) {
  let s = "";
  for (const t of baseTextNodes(el)) s += t.nodeValue;
  return s;
}

// Nearest enclosing [data-eid] element for a selection-boundary container (the
// container is a text node in the common case, an element when the boundary sits
// between child nodes).
function eidElementOf(container) {
  if (!container) return null;
  const el = container.nodeType === Node.TEXT_NODE ? container.parentElement : container;
  return el ? el.closest("[data-eid]") : null;
}

// Inverse of `textBoundary`: the base-text char offset of a (node, nodeOffset)
// DOM position within `eidEl`. Ruby isn't counted (it isn't a base text node).
export function charOffsetIn(eidEl, node, nodeOffset) {
  let count = 0;
  for (const n of baseTextNodes(eidEl)) {
    if (n === node) return count + nodeOffset;
    // A base node that follows the boundary node in document order means the
    // boundary fell in a non-base region (e.g. <rt>) before it → clamp here.
    if (node.compareDocumentPosition(n) & Node.DOCUMENT_POSITION_FOLLOWING) return count;
    count += n.nodeValue.length;
  }
  return count; // boundary at/after the element's last base char
}

// A DOM Range (a user selection) → { eid_start, off_start, eid_end, off_end }, or
export function anchorFromRange(doc, range) {
  if (!range || range.collapsed) return null;
  const startEl = eidElementOf(range.startContainer);
  let endEl = eidElementOf(range.endContainer);
  if (!startEl || !endEl) return null;

  const eid_start = Number(startEl.getAttribute("data-eid"));
  const off_start = charOffsetIn(startEl, range.startContainer, range.startOffset);

  let endChar = charOffsetIn(endEl, range.endContainer, range.endOffset);
  if (endChar <= 0) {
    const all = [...doc.querySelectorAll("[data-eid]")];
    const i = all.indexOf(endEl);
    if (i > 0) {
      endEl = all[i - 1];
      endChar = baseString(endEl).length; // exclusive end = full length
    } else {
      endChar = 1; // degenerate: keep at least one char
    }
  }
  const eid_end = Number(endEl.getAttribute("data-eid"));
  const off_end = Math.max(0, endChar - 1);

  if (!Number.isInteger(eid_start) || !Number.isInteger(eid_end)) return null;
  return { eid_start, off_start, eid_end, off_end };
}

// Reconstruct an annotation's base text from the live DOM, slicing each
export function baseTextOf(doc, ann) {
  if (ann.eid_start == null) return "";
  const all = [...doc.querySelectorAll("[data-eid]")];
  const startEl = doc.querySelector(`[data-eid="${ann.eid_start}"]`);
  const endEl = ann.eid_end != null ? doc.querySelector(`[data-eid="${ann.eid_end}"]`) : startEl;
  if (!startEl) return "";
  const si = all.indexOf(startEl);
  let ei = endEl ? all.indexOf(endEl) : si;
  if (si < 0) return "";
  if (ei < si) ei = si;
  let out = "";
  // [data-eid] elements nest — a heading wrapper holds the same words as the
  let outer = null;
  for (let i = si; i <= ei; i++) {
    const el = all[i];
    if (outer && outer.contains(el)) continue;
    outer = el;
    const text = baseString(el);
    const from = el === startEl ? (ann.off_start ?? 0) : 0;
    // An end offset that lands inside a skipped descendant is measured in that
    const to = el === endEl ? (ann.off_end ?? 0) + 1 : text.length;
    out += text.slice(from, Math.min(to, text.length));
  }
  return out;
}
