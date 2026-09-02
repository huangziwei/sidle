export const INHERITED = new Set([
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

export function matchedRules(el, ancestor) {
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

export function inspect(el) {
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
