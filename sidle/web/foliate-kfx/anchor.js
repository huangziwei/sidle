// anchor.js — resolve a KFX annotation's (eid, char-offset) anchor to a DOM
// Range inside a rendered section. Pure given the section `doc` + annotation,
// so it's unit-testable independent of the reader coordinator.
//
// boko stamps `data-eid="<eid>"` on every addressable element; the char offset
// indexes the element's *base text* (ruby <rt>/<rp> excluded), exactly as the
// KFX content text does — proven char-exact against My Clippings in P0.

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
