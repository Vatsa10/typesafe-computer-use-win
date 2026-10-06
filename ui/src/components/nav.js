// The left NavigationView rail. One item is current at a time; the pill on its left edge is the
// active indicator. Views register an `onShow` to refresh when they come into view.
import { h } from "../lib/dom.js";
import { icon } from "../lib/icons.js";

const ITEMS = [
  { id: "run", label: "Run", icon: "play" },
  { id: "history", label: "History", icon: "history" },
  { id: "settings", label: "Settings", icon: "settings" },
];

export function mountNav(root, views) {
  const hotkeys = h("dl", { class: "hotkey-list" });
  const nav = h("nav", { class: "nav", "aria-label": "Sections" });
  const buttons = new Map();

  for (const item of ITEMS) {
    const button = h(
      "button",
      { class: "nav-item", type: "button", onclick: () => show(item.id) },
      h("span", { html: icon(item.icon, "lg"), style: "display:flex" }),
      h("span", { text: item.label }),
    );
    buttons.set(item.id, button);
    nav.append(button);
  }
  nav.append(h("div", { class: "nav-spacer" }), h("div", { class: "nav-foot" }, h("strong", { text: "Hotkeys" }), hotkeys));
  root.append(nav);

  // Arrow keys move along the rail, as in a Windows NavigationView.
  nav.addEventListener("keydown", (event) => {
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    const list = [...buttons.values()];
    const at = list.indexOf(document.activeElement);
    if (at < 0) return;
    event.preventDefault();
    list[(at + (event.key === "ArrowDown" ? 1 : list.length - 1)) % list.length].focus();
  });

  function show(id) {
    for (const [key, button] of buttons) {
      if (key === id) button.setAttribute("aria-current", "page");
      else button.removeAttribute("aria-current");
    }
    for (const [key, view] of Object.entries(views)) {
      view.element.classList.toggle("is-on", key === id);
      if (key === id && view.onShow) view.onShow();
    }
  }

  return {
    show,
    // The core sends "binding name binding name ..."; show it as a two-column list.
    setHotkeys(text) {
      if (text === hotkeys.dataset.text) return;
      hotkeys.dataset.text = text;
      const words = String(text || "").trim().split(/\s+/).filter(Boolean);
      const rows = [];
      for (let i = 0; i + 1 < words.length; i += 2) rows.push(h("dt", { text: words[i + 1] }), h("dd", {}, h("kbd", { text: words[i] })));
      if (!rows.length && words.length) rows.push(h("dd", { text: words.join(" ") }));
      hotkeys.replaceChildren(...rows);
    },
  };
}
