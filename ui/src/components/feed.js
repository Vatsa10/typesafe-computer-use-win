// The activity feed: every `line` the core prints, styled by what the run log already
// distinguishes, plus answers as their own cards. Capped so a long session never grows the DOM
// without bound, and it only follows the bottom when the user has not scrolled up.
import { h, emptyState } from "../lib/dom.js";
import { icon } from "../lib/icons.js";

const CAP = 1200;

const KINDS = {
  step: "chevron",
  did: "check",
  heard: "ear",
  bad: "xCircle",
  warn: "alert",
};

export function lineKind(text) {
  if (text.startsWith("step ")) return "step";
  if (text.includes("did: ")) return "did";
  if (text.startsWith("heard") || text.startsWith("test:")) return "heard";
  if (/refused|failed|unavailable|aborted|no answer|error:/.test(text)) return "bad";
  if (/below|ignored|stopping|nothing/.test(text)) return "warn";
  return "";
}

export function createFeed() {
  const list = h("ol", { class: "feed", "aria-live": "polite", "aria-label": "Activity" });
  const empty = emptyState(icon("terminal"), "Nothing yet", "Start a goal and each step, action and answer shows up here as it happens.");
  const clear = h("button", { class: "btn subtle icon-only", type: "button", "aria-label": "Clear activity", title: "Clear activity", html: icon("trash") });

  const card = h(
    "section",
    { class: "card feed-card", "aria-label": "Activity" },
    h("div", { class: "feed-head" }, h("h2", { text: "Activity" }), clear),
    list,
    empty,
  );

  function sync() {
    empty.hidden = list.childElementCount > 0;
    clear.disabled = list.childElementCount === 0;
  }

  function append(node) {
    const atBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 40;
    list.append(node);
    while (list.childElementCount > CAP) list.firstElementChild.remove();
    sync();
    if (atBottom || node.classList.contains("answer")) list.scrollTop = list.scrollHeight;
  }

  clear.addEventListener("click", () => {
    list.replaceChildren();
    sync();
  });
  sync();

  return {
    element: card,
    line(text) {
      const kind = lineKind(text);
      append(
        h(
          "li",
          { class: kind },
          kind ? h("span", { html: icon(KINDS[kind]), style: "display:flex" }) : h("span", { class: "gutter" }),
          h("span", { text }),
        ),
      );
    },
    answer(text, spoken) {
      append(
        h(
          "li",
          { class: "answer" },
          h("div", { class: "answer-label", html: icon("message") + (spoken ? "Answer (read aloud)" : "Answer") }),
          h("div", { text }),
        ),
      );
    },
  };
}
