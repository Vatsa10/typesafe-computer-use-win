/* The panel's behaviour. Everything it knows comes from the shell's bridge: `window.core.call`
   sends a request to the Rust core and resolves with its reply, and `window.core.on` receives the
   events the core sends unasked.

   The feed is pushed rather than polled. The Python panel polled because a webview bridge only
   answers questions; a separate process can talk whenever it likes, so a line appears when it is
   printed rather than up to a quarter second later. */

const $ = (id) => document.getElementById(id);

let activeSteps = [];
let settingsRows = [];

async function call(method, params) {
  const reply = await window.core.call(method, params || {});
  if (!reply || !reply.ok) {
    pushLines([`error: ${(reply && reply.error) || "the core did not answer"}`]);
    return null;
  }
  return reply.result;
}

/* ------------------------------------------------------------------ feed */

function lineClass(text) {
  // The feed colours what the run log already distinguishes, so the two read the same way.
  if (text.startsWith("step ")) return "step";
  if (text.includes("did: ")) return "did";
  if (text.startsWith("heard") || text.startsWith("test:")) return "heard";
  if (/refused|failed|unavailable|aborted|no answer|error:/.test(text)) return "bad";
  if (/below|ignored|stopping|nothing/.test(text)) return "warn";
  return "";
}

function pushLines(lines) {
  const feed = $("feed");
  const atBottom = feed.scrollHeight - feed.scrollTop - feed.clientHeight < 40;
  for (const text of lines) {
    const li = document.createElement("li");
    li.className = lineClass(text);
    li.textContent = text;
    feed.appendChild(li);
  }
  while (feed.childElementCount > 1200) feed.removeChild(feed.firstChild);
  if (atBottom) feed.scrollTop = feed.scrollHeight; // only follow if the user had not scrolled up
}

function pushAnswer(text) {
  const li = document.createElement("li");
  li.className = "answer";
  li.textContent = text;
  $("feed").appendChild(li);
  $("feed").scrollTop = $("feed").scrollHeight;
}

/* ----------------------------------------------------------------- state */

function paintState(s) {
  $("dot").className = "dot" + (s.running ? (s.paused ? " paused" : " running") : "");
  $("stateText").textContent = s.paused ? "paused" : s.running ? "running" : "idle";
  $("pause").disabled = !s.running;
  $("abort").disabled = !s.running;
  $("pause").textContent = s.paused ? "Resume" : "Pause";
  if (s.hotkeys) $("hotkeys").textContent = s.hotkeys;
}

window.core.on((message) => {
  if (message.event === "line") pushLines([message.text]);
  if (message.event === "state") paintState(message);
  if (message.event === "answer") pushAnswer(message.text);
  if (message.event === "hotkey" && message.name === "bar") $("goal").focus();
});

async function refreshState() {
  const state = await call("state");
  if (state) paintState(state);
}

/* ------------------------------------------------------------------ tabs */

for (const tab of document.querySelectorAll(".tab")) {
  tab.addEventListener("click", () => {
    for (const other of document.querySelectorAll(".tab")) {
      const on = other === tab;
      other.classList.toggle("is-on", on);
      other.setAttribute("aria-selected", String(on));
    }
    for (const view of document.querySelectorAll(".view")) {
      view.classList.toggle("is-on", view.id === "view-" + tab.dataset.view);
    }
    if (tab.dataset.view === "history") loadRuns();
    if (tab.dataset.view === "settings") loadSettings();
  });
}

/* --------------------------------------------------------------- the ask */

$("ask").addEventListener("submit", async (event) => {
  event.preventDefault();
  const goal = $("goal").value.trim();
  if (!goal) return;
  $("goal").value = "";
  await call("start", { goal, act: $("modeAct").classList.contains("is-on") });
});

$("pause").addEventListener("click", () => call("pause"));
$("abort").addEventListener("click", () => call("abort"));
$("testVoice").addEventListener("click", async () => {
  pushLines(["test: hold the talk hotkey and say something"]);
  const started = await call("listen_start");
  if (!started) return;
  const heard = await call("listen_stop");
  pushLines([heard && heard.heard ? `heard: ${heard.heard}` : "heard nothing"]);
});

function setMode(act) {
  $("modeAct").classList.toggle("is-on", act);
  $("modeAct").setAttribute("aria-pressed", String(act));
  $("modeDry").classList.toggle("is-on", !act);
  $("modeDry").setAttribute("aria-pressed", String(!act));
  call("set_mode", { act, voice: $("voiceOn").checked, hide: $("hideOnAct").checked });
}

$("modeAct").addEventListener("click", () => setMode(true));
$("modeDry").addEventListener("click", () => setMode(false));
$("voiceOn").addEventListener("change", () => setMode($("modeAct").classList.contains("is-on")));
$("hideOnAct").addEventListener("change", () => setMode($("modeAct").classList.contains("is-on")));

/* --------------------------------------------------------------- history */

async function loadRuns() {
  const runs = (await call("runs")) || [];
  const list = $("runList");
  list.innerHTML = "";
  if (!runs.length) {
    list.innerHTML = '<li class="what">No runs yet.</li>';
    return;
  }
  for (const run of runs) {
    const item = document.createElement("li");
    item.innerHTML = '<div class="when"></div><div class="what"></div>';
    item.querySelector(".when").textContent = run.name;
    item.querySelector(".what").textContent = run.goal || run.outcome;
    item.addEventListener("click", () => {
      for (const other of list.children) other.classList.remove("is-on");
      item.classList.add("is-on");
      showRun(run);
    });
    list.appendChild(item);
  }
}

function badge(run) {
  if (run.goal_achieved === true) return '<span class="badge good">achieved</span>';
  if (run.goal_achieved === false) return '<span class="badge bad">not achieved</span>';
  return '<span class="badge">' + run.outcome + "</span>";
}

async function showRun(run) {
  const meta = $("runMeta");
  meta.className = "meta";
  const seconds = run.seconds != null ? ` · ${run.seconds}s` : "";
  meta.innerHTML =
    "<dt>Goal</dt><dd class='goal-text'></dd>" +
    `<dt>Outcome</dt><dd>${badge(run)} <span class="default">${run.outcome}${seconds} · ${run.steps_taken} steps${
      run.acted ? "" : " · dry run"
    }</span></dd>` +
    "<dt>Answer</dt><dd class='answer-text'></dd>";
  meta.querySelector(".goal-text").textContent = run.goal || "(none recorded)";
  meta.querySelector(".answer-text").textContent = run.answer || "(none)";

  activeSteps = (await call("steps", { name: run.name })) || [];
  const strip = $("stepStrip");
  strip.innerHTML = "";
  $("shot").hidden = true;
  $("shotEmpty").hidden = false;
  for (const step of activeSteps) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = "step " + String(step.number).padStart(2, "0");
    button.addEventListener("click", async () => {
      for (const other of strip.children) other.classList.remove("is-on");
      button.classList.add("is-on");
      const shot = await call("shot", { name: run.name, number: step.number });
      const image = $("shot");
      if (!shot || !shot.data_url) {
        image.hidden = true;
        $("shotEmpty").hidden = false;
        $("shotEmpty").textContent = "That step wrote no capture.";
        return;
      }
      image.src = shot.data_url;
      image.hidden = false;
      $("shotEmpty").hidden = true;
    });
    strip.appendChild(button);
  }
  if (!activeSteps.length) $("shotEmpty").textContent = "This run recorded no steps.";
}

$("refresh").addEventListener("click", loadRuns);

/* -------------------------------------------------------------- settings */

async function loadSettings() {
  settingsRows = (await call("settings")) || [];
  const box = $("settings");
  box.innerHTML = "";
  for (const row of settingsRows) {
    const label = document.createElement("label");
    label.textContent = row.label;
    label.htmlFor = "set-" + row.key;
    const input = document.createElement("input");
    input.id = "set-" + row.key;
    input.value = row.value || "";
    input.placeholder = row.fallback;
    input.spellcheck = false;
    const note = document.createElement("span");
    note.className = "default";
    note.textContent = "default: " + row.fallback;
    box.append(label, input, note);
  }
}

$("save").addEventListener("click", async () => {
  const values = {};
  for (const row of settingsRows) values[row.key] = $("set-" + row.key).value.trim();
  await call("save_settings", { values });
  $("saved").textContent = "Saved. Hotkey changes need a restart.";
  setTimeout(() => ($("saved").textContent = ""), 4000);
});

/* ----------------------------------------------------------------- start */

window.addEventListener("DOMContentLoaded", () => {
  refreshState();
  setInterval(refreshState, 1000); // events carry the news; this only catches a missed one
});
