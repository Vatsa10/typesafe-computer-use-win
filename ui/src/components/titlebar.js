// The custom title region. The window is frameless at the top (titleBarStyle: hidden) and the
// native caption buttons are drawn by Windows on the right through titleBarOverlay; this strip is
// the draggable part with the Pointer mark and name.
import { h } from "../lib/dom.js";
import { logo } from "../lib/icons.js";

export function mountTitlebar(root) {
  const bar = h(
    "header",
    { class: "titlebar" },
    h("span", { html: logo("logo"), style: "display:flex" }),
    h("span", { class: "name", text: "Pointer" }),
    h("span", { class: "tag", text: "Computer use for Windows" }),
  );
  root.append(bar);
  return bar;
}
