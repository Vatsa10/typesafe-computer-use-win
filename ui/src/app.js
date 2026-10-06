// The panel's entry point. It builds the shell, routes the core's events to the views that care,
// and turns every failed call into a toast (and a feed line) instead of silence.
import { call, on, onError, describeError } from "./lib/core.js";
import { mountTitlebar } from "./components/titlebar.js";
import { mountNav } from "./components/nav.js";
import { mountToasts, toast } from "./components/toast.js";
import { createRunView } from "./components/run.js";
import { createHistoryView } from "./components/history.js";
import { createSettingsView } from "./components/settings.js";

const query = new URLSearchParams(location.search);
if (query.get("mica") === "1") document.documentElement.classList.add("mica");
if (query.get("titlebar") === "custom") document.documentElement.classList.add("custom-titlebar");

const app = document.getElementById("app");
const run = createRunView();
const history = createHistoryView();
const settings = createSettingsView();

mountTitlebar(app);
const nav = mountNav(app, { run, history, settings });
const content = document.createElement("main");
content.className = "content";
content.append(run.element, history.element, settings.element);
app.append(content);
mountToasts(document.body);

onError((method, message) => {
  const { title, body } = describeError(method, message);
  toast({ kind: "error", title, body });
  run.feed.line(`error: ${method}: ${message}`);
});

let offline = false;

function paintState(state) {
  run.paintState(state);
  if (state.hotkeys) nav.setHotkeys(state.hotkeys);
}

on((message) => {
  if (message.event === "line") {
    run.feed.line(message.text);
    if (/^the core exited/.test(message.text)) {
      offline = true;
      paintState({ offline: true });
    }
  }
  if (message.event === "state") paintState(message);
  if (message.event === "answer") run.feed.answer(message.text, message.spoken);
  if (message.event === "hotkey" && message.name === "goal") {
    nav.show("run");
    run.focusGoal();
  }
});

async function refreshState() {
  if (offline) return;
  const state = await call("state", {}, { quiet: true });
  paintState(state || { offline: true });
}

nav.show("run");
refreshState();
setInterval(refreshState, 1000); // events carry the news; this only catches a missed one
