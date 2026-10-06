// The Run view: the goal composer, the mode switch, the run controls and the activity feed.
// Start is the one primary action here; Abort is the one destructive one and sits apart from it.
import { call } from "../lib/core.js";
import { h } from "../lib/dom.js";
import { icon } from "../lib/icons.js";
import { createFeed } from "./feed.js";
import { toast } from "./toast.js";

const MODE_HELP = {
  dry: { text: "Dry run decides every step and reports it, but never touches the mouse or keyboard.", tone: "" },
  act: { text: "Act really clicks and types. Hands off the mouse while it runs; Abort stops it at once.", tone: "warn" },
};

const STATES = {
  idle: { label: "Idle", icon: "circle" },
  running: { label: "Running", icon: "dot" },
  paused: { label: "Paused", icon: "pause" },
  offline: { label: "Core offline", icon: "plug" },
};

export function createRunView() {
  const feed = createFeed();
  let act = false;
  let listening = false;

  /* ---------------------------------------------------------- composer */
  const goal = h("input", {
    class: "field",
    id: "goal",
    type: "text",
    autocomplete: "off",
    spellcheck: "false",
    placeholder: "What should Pointer do?  e.g. open YouTube and search for lo-fi",
    "aria-label": "Goal",
  });
  const mic = h("button", { class: "btn icon-only mic", type: "button", "aria-label": "Speak the goal", title: "Speak the goal", html: icon("mic") });
  const start = h("button", { class: "btn accent", type: "submit", html: icon("play") + "<span>Start</span>" });
  const form = h("form", { class: "composer-row", "aria-label": "New run" }, goal, mic, start);

  const dry = h("button", { type: "button", role: "radio", "data-mode": "dry", html: icon("eye") + "<span>Dry run</span>" });
  const actButton = h("button", { type: "button", role: "radio", "data-mode": "act", html: icon("hand") + "<span>Act</span>" });
  const segmented = h("div", { class: "segmented", role: "radiogroup", "aria-label": "Mode" }, dry, actButton);
  const help = h("span", { class: "helper", id: "mode-help" });
  segmented.setAttribute("aria-describedby", "mode-help");

  const voice = h("input", { type: "checkbox", id: "voiceOn", checked: true });
  const hide = h("input", { type: "checkbox", id: "hideOnAct", checked: true });

  const composer = h(
    "section",
    { class: "card composer", "aria-label": "Goal" },
    form,
    h("div", { class: "composer-meta" }, segmented, help),
    h(
      "div",
      { class: "options" },
      h("label", { class: "toggle" }, voice, h("span", { text: "Voice" })),
      h("label", { class: "toggle" }, hide, h("span", { text: "Hide this window while acting" })),
    ),
  );

  /* ---------------------------------------------------------- run bar */
  const chip = h("span", { class: "chip", role: "status", "aria-live": "polite" });
  const pause = h("button", { class: "btn", type: "button", disabled: true });
  const abort = h("button", { class: "btn danger", type: "button", disabled: true, html: icon("stop") + "<span>Abort</span>" });
  const runBar = h("div", { class: "run-bar" }, chip, h("span", { class: "grow" }), pause, h("span", { class: "sep", "aria-hidden": "true" }), abort);

  const element = h(
    "section",
    { class: "view", id: "view-run", "aria-labelledby": "run-title" },
    h("div", { class: "view-head" }, h("h1", { id: "run-title", text: "Run" })),
    composer,
    runBar,
    feed.element,
  );

  /* ---------------------------------------------------------- behaviour */
  function paintMode() {
    dry.setAttribute("aria-checked", String(!act));
    actButton.setAttribute("aria-checked", String(act));
    dry.tabIndex = act ? -1 : 0;
    actButton.tabIndex = act ? 0 : -1;
    help.textContent = MODE_HELP[act ? "act" : "dry"].text;
    help.className = "helper " + MODE_HELP[act ? "act" : "dry"].tone;
  }

  function sendMode() {
    call("set_mode", { act, voice: voice.checked, hide: hide.checked });
  }

  function setMode(next) {
    act = next;
    paintMode();
    sendMode();
  }

  dry.addEventListener("click", () => setMode(false));
  actButton.addEventListener("click", () => setMode(true));
  segmented.addEventListener("keydown", (event) => {
    if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(event.key)) return;
    event.preventDefault();
    setMode(!act);
    (act ? actButton : dry).focus();
  });
  voice.addEventListener("change", sendMode);
  hide.addEventListener("change", sendMode);

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const text = goal.value.trim();
    if (!text) {
      goal.focus();
      return;
    }
    start.disabled = true;
    const result = await call("start", { goal: text, act });
    start.disabled = false;
    if (result) {
      goal.value = "";
      if (result.queued) toast({ kind: "info", title: "Queued", body: "It starts when the current run ends." });
    }
  });

  mic.addEventListener("click", async () => {
    if (listening) {
      listening = false;
      mic.classList.remove("live");
      mic.classList.add("busy");
      mic.innerHTML = icon("loader");
      mic.setAttribute("aria-label", "Transcribing");
      const heard = await call("listen_stop");
      mic.classList.remove("busy");
      mic.innerHTML = icon("mic");
      mic.setAttribute("aria-label", "Speak the goal");
      if (heard && heard.heard) {
        goal.value = heard.heard;
        feed.line(`heard: ${heard.heard}`);
        goal.focus();
      } else if (heard) {
        toast({ kind: "warn", title: "Nothing heard", body: "Click the mic, speak, then click it again." });
      }
      return;
    }
    const started = await call("listen_start");
    if (!started || !started.listening) return;
    listening = true;
    mic.classList.add("live");
    mic.setAttribute("aria-label", "Stop listening");
  });

  pause.addEventListener("click", () => call("pause"));
  abort.addEventListener("click", () => call("abort"));

  function paintState(state) {
    const key = state.offline ? "offline" : state.running ? (state.paused ? "paused" : "running") : "idle";
    chip.dataset.state = key;
    chip.innerHTML = icon(STATES[key].icon) + `<span>${STATES[key].label}</span>`;
    pause.disabled = !state.running;
    abort.disabled = !state.running;
    pause.innerHTML = state.paused ? icon("play") + "<span>Resume</span>" : icon("pause") + "<span>Pause</span>";
  }

  paintMode();
  paintState({ running: false, paused: false });

  return {
    element,
    feed,
    paintState,
    focusGoal: () => goal.focus(),
    onShow: () => goal.focus(),
  };
}
