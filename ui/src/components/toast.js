// Transient notices, bottom right. The container is an aria-live region, so a screen reader hears
// a save or a failure without the user hunting for it.
import { h } from "../lib/dom.js";
import { icon } from "../lib/icons.js";

const ICONS = { success: "checkCircle", error: "xCircle", info: "info", warn: "alert" };
let region = null;

export function mountToasts(root) {
  region = h("div", { class: "toasts", role: "status", "aria-live": "polite", "aria-atomic": "false" });
  root.append(region);
}

export function toast({ kind = "info", title, body = "", ms = 4500 }) {
  if (!region) return;
  // The same failure twice in a row is one notice, not a stack of them.
  const last = region.lastElementChild;
  if (last && last.dataset.key === kind + title + body) {
    clearTimeout(Number(last.dataset.timer));
    last.dataset.timer = String(setTimeout(() => dismiss(last), ms));
    return;
  }
  const node = h(
    "div",
    { class: `toast ${kind}`, role: kind === "error" ? "alert" : null },
    h("span", { html: icon(ICONS[kind] || "info") }),
    h("div", {}, h("div", { class: "title", text: title }), body ? h("div", { class: "body", text: body }) : null),
  );
  node.dataset.key = kind + title + body;
  node.dataset.timer = String(setTimeout(() => dismiss(node), ms));
  region.append(node);
  while (region.childElementCount > 3) region.firstElementChild.remove();
}

function dismiss(node) {
  node.classList.add("out");
  setTimeout(() => node.remove(), 260);
}
