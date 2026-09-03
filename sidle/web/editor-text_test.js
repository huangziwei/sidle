import { assert, assertEquals } from "@std/assert";
import {
  elementAddressAt,
  findAll,
  highlight,
  classAtCursor,
  relativePath,
  renderLines,
  langOf,
  lineAt,
  lineStart,
  replaceAll,
  stampLines,
} from "./editor-text.js";

Deno.test("stampLines numbers every start tag and leaves the rest verbatim", () => {
  const src = `<?xml version="1.0"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>a > b</title></head>
<body>
<!-- <p>not a tag</p> -->
<p class="x" title="a>b">text<br/></p>
<![CDATA[ <q> ]]>
</body></html>`;
  const out = stampLines(src);
  assertEquals(out.match(/data-lnum="(\d+)"/g), [
    'data-lnum="3"',
    'data-lnum="4"',
    'data-lnum="4"',
    'data-lnum="5"',
    'data-lnum="7"',
    'data-lnum="7"',
  ]);
  assert(out.includes('<p class="x" title="a>b" data-lnum="7">'));
  assert(out.includes('<br data-lnum="7"/>'));
  assert(out.includes("<!-- <p>not a tag</p> -->"));
  assert(out.includes("<![CDATA[ <q> ]]>"));
  assertEquals(out.replace(/ data-lnum="\d+"/g, ""), src);
});

Deno.test("stampLines survives an unterminated tag", () => {
  assertEquals(stampLines("<p>ok</p>\n<div class=\"x"), "<p data-lnum=\"1\">ok</p>\n<div class=\"x");
});

Deno.test("line arithmetic", () => {
  const t = "ab\ncd\nef";
  assertEquals(lineAt(t, 0), 1);
  assertEquals(lineAt(t, 3), 2);
  assertEquals(lineAt(t, 8), 3);
  assertEquals(lineStart(t, 1), 0);
  assertEquals(lineStart(t, 3), 6);
  assertEquals(lineStart(t, 9), 8);
});

Deno.test("elementAddressAt picks the start tag at or before the cursor on its line", () => {
  const t = "<p>a</p>\n<div><span>x</span> <em>y</em></div>";
  assertEquals(elementAddressAt(t, 2), { line: 1, index: 0, tag: "p" });
  assertEquals(elementAddressAt(t, 9), { line: 2, index: 0, tag: "div" });
  assertEquals(elementAddressAt(t, 17), { line: 2, index: 1, tag: "span" });
  assertEquals(elementAddressAt(t, 35), { line: 2, index: 2, tag: "em" });
});

Deno.test("findAll and replaceAll, plain and regex", () => {
  const t = "紅色的跟睛，跟睛。";
  assertEquals(findAll(t, "跟睛"), [{ start: 3, end: 5 }, { start: 6, end: 8 }]);
  assertEquals(replaceAll(t, "跟睛", "眼睛"), { text: "紅色的眼睛，眼睛。", count: 2 });
  assertEquals(replaceAll("a1b22", "\\d+", "#", { regex: true }), { text: "a#b#", count: 2 });
  assertEquals(replaceAll("AbAB", "ab", "x"), { text: "xx", count: 2 });
  assertEquals(replaceAll("AbAB", "ab", "x", { caseSensitive: true }), { text: "AbAB", count: 0 });
  assertEquals(findAll("x", "("), []);
});

Deno.test("highlight escapes and never loses characters", () => {
  const xml = '<p class="a&b">x < y</p><!-- c -->';
  const h = highlight(xml, "xml");
  assertEquals(h.replace(/<[^>]+>/g, ""), xml.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;"));
  const css = "p, a { color: red; /* c */ } @media x { b { x: 1 } }";
  assertEquals(highlight(css, "css").replace(/<[^>]+>/g, ""), css);
  assertEquals(langOf("OEBPS/a.xhtml"), "xml");
  assertEquals(langOf("a/b.CSS"), "css");
  assertEquals(langOf("x.txt"), "text");
});

Deno.test("renderLines splits tokens at newlines and numbers every line", () => {
  const xml = "<p>a</p>\n<!-- two\nlines -->\nend\n";
  const html = renderLines(xml, "xml");
  const lines = html.match(/<div class="line">/g);
  assertEquals(lines.length, 5);
  assert(html.includes('<span class="ln">1</span>'));
  assert(html.includes('<span class="ln">5</span>'));
  assert(html.includes('<span class="hl-c">&lt;!-- two</span>\n</div>'));
  assert(html.includes('<div class="line"><span class="ln">3</span><span class="hl-c">lines --&gt;</span>\n</div>'));
  const stripped = html.replace(/<span class="ln">\d+<\/span>/g, "").replace(/<[^>]+>/g, "").replace(/\n<\/div>|\n$/g, "");
  assertEquals(stripped.replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&amp;/g, "&").replace(/\n/g, ""), xml.replace(/\n/g, ""));
});

Deno.test("relativePath walks up and down between member directories", () => {
  assertEquals(relativePath("OEBPS/text/a.xhtml", "OEBPS/images/x y.png"), "../images/x%20y.png");
  assertEquals(relativePath("OEBPS/a.xhtml", "OEBPS/b.xhtml"), "b.xhtml");
  assertEquals(relativePath("a.xhtml", "OEBPS/b.xhtml"), "OEBPS/b.xhtml");
  assertEquals(relativePath("OEBPS/text/a.xhtml", "nav.xhtml"), "../../nav.xhtml");
});

Deno.test("classAtCursor reads the first class of the tag around the cursor", () => {
  const text = '<p class="a b">x</p><span>y</span>';
  assertEquals(classAtCursor(text, 5), "a");
  assertEquals(classAtCursor(text, 16), "");
  assertEquals(classAtCursor(text, 22), "");
});
