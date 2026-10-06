// The History view: past runs on the left with outcome badges, and on the right what the chosen
// run did: its meta, a strip of its steps, and the screen each step saw.
import { call } from "../lib/core.js";
import { h, emptyState } from "../lib/dom.js";
import { icon } from "../lib/icons.js";

function badge(run) {
  if (run.goal_achieved === true) return h("span", { class: "badge good", html: icon("checkCircle") + "<span>Achieved</span>" });
  if (run.goal_achieved === false) return h("span", { class: "badge bad", html: icon("xCircle") + "<span>Not achieved</span>" });
  return h("span", { class: "badge", html: icon("info") }, h("span", { text: run.outcome || "unknown" }));
}

// Run folders are named by timestamp; show them the way a person reads a date when we can.
function when(name) {
  const m = /^(\d{4})(\d{2})(\d{2})[-_T]?(\d{2})(\d{2})(\d{2})?/.exec(name || "");
  if (!m) return name;
  const date = new Date(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +(m[6] || 0));
  if (Number.isNaN(date.getTime())) return name;
  return date.toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

export function createHistoryView() {
  const list = h("ul", { class: "run-list", role: "listbox", "aria-label": "Runs" });
  const listEmpty = emptyState(icon("inbox"), "No runs yet", "Finished runs land here with every step they took.");
  const refresh = h("button", { class: "btn", type: "button", html: icon("refresh") + "<span>Refresh</span>" });

  const detail = h("div", { class: "detail" });
  const element = h(
    "section",
    { class: "view", id: "view-history", "aria-labelledby": "history-title" },
    h("div", { class: "view-head" }, h("h1", { id: "history-title", text: "History" }), refresh),
    h("div", { class: "split" }, h("div", { class: "card list-card" }, list, listEmpty), detail),
  );

  let loadToken = 0;

  function showPlaceholder() {
    detail.replaceChildren(h("div", { class: "card shot-wrap" }, emptyState(icon("history"), "Pick a run", "Choose a run on the left to see its goal, outcome and the screen at every step.")));
  }

  async function load() {
    refresh.disabled = true;
    const runs = (await call("runs")) || [];
    refresh.disabled = false;
    list.replaceChildren();
    listEmpty.hidden = runs.length > 0;
    showPlaceholder();
    for (const run of runs) {
      const button = h(
        "button",
        { type: "button", role: "option", "aria-selected": "false" },
        h("span", { class: "goal", text: run.goal || "(no goal recorded)" }),
        h("span", { class: "sub" }, badge(run), h("span", { text: when(run.name) })),
      );
      button.addEventListener("click", () => {
        for (const other of list.querySelectorAll("button")) other.setAttribute("aria-selected", String(other === button));
        select(run);
      });
      list.append(h("li", {}, button));
    }
  }

  async function select(run) {
    const token = ++loadToken;
    const seconds = run.seconds != null ? `${Number(run.seconds).toFixed(1)} s` : null;
    const facts = [run.outcome, seconds, `${run.steps_taken ?? 0} steps`, run.acted ? "acted" : "dry run"].filter(Boolean).join("  ·  ");

    const meta = h(
      "dl",
      { class: "card meta" },
      h("dt", { text: "Goal" }),
      h("dd", { text: run.goal || "(none recorded)" }),
      h("dt", { text: "Outcome" }),
      h("dd", {}, badge(run), h("span", { class: "dim", text: facts })),
      h("dt", { text: "Answer" }),
      h("dd", { text: run.answer || "(none)" }),
      h("dt", { text: "Folder" }),
      h("dd", { class: "dim", text: run.name }),
    );
    const strip = h("div", { class: "strip", role: "toolbar", "aria-label": "Steps" });
    const viewer = h("div", { class: "card shot-wrap", "aria-live": "polite" });
    detail.replaceChildren(meta, strip, viewer);

    const steps = (await call("steps", { name: run.name })) || [];
    if (token !== loadToken) return;
    if (!steps.length) {
      viewer.replaceChildren(emptyState(icon("imageOff"), "No steps recorded", "This run ended before it took a step."));
      return;
    }
    viewer.replaceChildren(emptyState(icon("image"), "Pick a step", "See the screen as that step saw it."));

    for (const step of steps) {
      const label = "Step " + String(step.number).padStart(2, "0");
      const button = h("button", {
        type: "button",
        "aria-pressed": "false",
        class: step.has_shot ? "" : "no-shot",
        title: step.has_shot ? label : `${label} (no capture)`,
        text: label,
      });
      button.addEventListener("click", async () => {
        for (const other of strip.children) other.setAttribute("aria-pressed", String(other === button));
        if (!step.has_shot) {
          viewer.replaceChildren(emptyState(icon("imageOff"), "No capture", "That step wrote no screenshot."));
          return;
        }
        const shot = await call("shot", { name: run.name, number: step.number });
        if (token !== loadToken) return;
        if (!shot) {
          viewer.replaceChildren(emptyState(icon("alert"), "Couldn't load the capture", "The core did not return it. Try again."));
          return;
        }
        if (!shot.data_url) {
          viewer.replaceChildren(emptyState(icon("imageOff"), "No capture", "That step wrote no screenshot."));
          return;
        }
        const image = h("img", { alt: `Screen at ${label.toLowerCase()} of "${run.goal || run.name}"` });
        image.addEventListener("error", () => viewer.replaceChildren(emptyState(icon("alert"), "Couldn't show the capture", "The image is damaged.")));
        image.src = shot.data_url;
        viewer.replaceChildren(image);
      });
      strip.append(button);
    }
    // Arrow keys walk the strip.
    strip.addEventListener("keydown", (event) => {
      if (event.key !== "ArrowRight" && event.key !== "ArrowLeft") return;
      const buttons = [...strip.children];
      const at = buttons.indexOf(document.activeElement);
      if (at < 0) return;
      const next = buttons[Math.min(buttons.length - 1, Math.max(0, at + (event.key === "ArrowRight" ? 1 : -1)))];
      next.focus();
      next.click();
    });
  }

  refresh.addEventListener("click", load);
  showPlaceholder();

  return { element, onShow: load };
}
