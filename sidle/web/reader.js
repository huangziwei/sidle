// reader.js — the built-in reader coordinator. Replaces foliate's view.js with
// a thin layer over the vendored paginator: open a library book (KFX→DOM via
// the `reader_open` Tauri command), paginate it, and paint imported
// annotations as overlay highlights. Exposed as `window.sidleReader` so the
// (classic-script) library.js can drive it across the module boundary.

import "./foliate-kfx/paginator.js"; // defines <foliate-paginator>
import { Overlayer } from "./foliate-kfx/overlayer.js";
import { makeKfxBook } from "./foliate-kfx/kfx-book.js";
import { rangeFor } from "./foliate-kfx/anchor.js";

const $ = (sel) => document.querySelector(sel);
const toast = (msg, isError) => window.showToast?.(msg, isError);

let book = null; // current kfx-book
let paginator = null; // <foliate-paginator>
let annotations = []; // AnnotationDto[] for the open book
let keyHandler = null;

const view = () => $("#reader-view");

// Named Kindle highlight colors → CSS; falls back to a literal color or yellow.
const COLORS = { yellow: "#f4d03f", blue: "#5dade2", pink: "#ec7fa9", orange: "#e59866" };

function paintAnnotations(doc, overlayer) {
  for (const ann of annotations) {
    if (ann.kind === "bookmark") continue; // margin markers handled later
    const range = rangeFor(doc, ann);
    if (!range) continue;
    const color = (ann.color && COLORS[ann.color]) || ann.color || COLORS.yellow;
    overlayer.add(`ann-${ann.id}`, range, Overlayer.highlight, { color });
  }
}

// ---- navigation -----------------------------------------------------------

const forward = () => paginator?.next();
const back = () => paginator?.prev();

function onKey(e) {
  if (e.key === "Escape") {
    close();
    return;
  }
  const rtl = book?.ppd === "rtl"; // vertical-rl / RTL: next page is to the left
  let handled = true;
  switch (e.key) {
    case "ArrowLeft":
      rtl ? forward() : back();
      break;
    case "ArrowRight":
      rtl ? back() : forward();
      break;
    case "ArrowDown":
    case "PageDown":
    case " ":
      forward();
      break;
    case "ArrowUp":
    case "PageUp":
      back();
      break;
    default:
      handled = false;
  }
  if (handled) e.preventDefault();
}

// ---- open / close ---------------------------------------------------------

async function open(bookId) {
  await close(); // tear down any prior session
  let dto, anns;
  try {
    [dto, anns] = await Promise.all([
      window.api.invoke("reader_open", { bookId }),
      window.api.invoke("annotations_for_book", { bookId }),
    ]);
  } catch (err) {
    toast(`Couldn't open reader: ${err}`, true);
    return;
  }
  annotations = anns || [];
  book = makeKfxBook(dto);

  $("#reader-title").textContent = dto.title || "Untitled";
  $("#reader-progress").textContent = "";
  view().hidden = false;
  view().classList.add("open");

  paginator = document.createElement("foliate-paginator");
  paginator.setAttribute("flow", "paginated");
  $("#reader-stage").replaceChildren(paginator);

  paginator.addEventListener("create-overlayer", ({ detail: { doc, attach } }) => {
    const overlayer = new Overlayer();
    attach(overlayer);
    paintAnnotations(doc, overlayer);
  });
  paginator.addEventListener("relocate", ({ detail }) => {
    const pct = Math.round((detail.fraction ?? 0) * 100);
    $("#reader-progress").textContent = `${pct}%`;
  });

  paginator.open(book);
  await paginator.goTo({ index: 0 }); // TODO(T2): restore saved reading position
  paginator.focus?.();
  keyHandler = onKey;
  document.addEventListener("keydown", keyHandler, true);
}

async function close() {
  if (keyHandler) {
    document.removeEventListener("keydown", keyHandler, true);
    keyHandler = null;
  }
  if (paginator) {
    $("#reader-stage")?.replaceChildren();
    paginator = null;
  }
  if (book) {
    book.destroy();
    book = null;
  }
  annotations = [];
  const v = view();
  if (v) {
    v.classList.remove("open");
    v.hidden = true;
  }
}

function wire() {
  $("#reader-close")?.addEventListener("click", () => close());
  $("#reader-prev")?.addEventListener("click", () => back());
  $("#reader-next")?.addEventListener("click", () => forward());
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", wire);
} else {
  wire();
}

window.sidleReader = { open, close };
