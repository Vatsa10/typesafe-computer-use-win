// Tiny DOM helpers. Text always goes in through textContent; only our own icon markup is ever
// written as HTML.

export const $ = (selector, root = document) => root.querySelector(selector);

export function h(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value == null || value === false) continue;
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (key === "html") node.innerHTML = value; // trusted markup only (icons)
    else if (key.startsWith("on") && typeof value === "function") node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? "" : value);
  }
  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

export function emptyState(iconMarkup, title, body) {
  return h("div", { class: "empty" }, h("span", { html: iconMarkup }), h("strong", { text: title }), body ? h("span", { text: body }) : null);
}
