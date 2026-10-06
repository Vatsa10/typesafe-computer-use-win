/* The panel's behaviour. Everything it knows comes from pywebview's bridge: `window.pywebview.api`
   is the Python object in panel.py, and every method returns a promise.

   The feed is polled rather than pushed. The run loop prints from a worker thread, and a poll is
   the one shape that cannot race a WebView2 message pump: the panel asks, Python hands over
   whatever has accumulated, and nothing is ever delivered to a page that is not ready for it. */

const POLL_MS = 250;
const $ = (id) => document.getElementById(id);

let activeRun = null;
let activeSteps = [];
let settingsRows = [];

/* ------------------------------------------------------------------ feed */

function lineClass(text) {
  // The feed colours what the run log already distinguishes, so the two read the same way.
  if (text.startsWith("step ")) return "step";
  if (text.includes("did: ")) return "did";
  if (text.startsWith("heard") || text.startsWith("test:")) return "heard";
  if (/refused|failed|unavailable|aborted|no answer/.test(text)) return "bad";
  if (/below|ignored|stopping|nothing/.test(text)) return "warn";
  if (text.startsWith("  ") && text.trim().length > 90) return "answer";
  return "";
}

function pushLines(lines) {
  if (!lines.length) return;
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

/* ----------------------------------------------------------------- state */

function paintState(s) {
  const running = s.running;
  const paused = s.paused;
  $("dot").className = "dot" + (running ? (paused ? " paused" : " running") : "");
  $("stateText").textContent = paused ? "paused" : running ? "running" : "idle";
  $("pause").disabled = !running;
  $("abort").disabled = !running;
  $("pause").textContent = paused ? "Resume" : "Pause";
  $("hotkeys").textContent = s.hotkeys || "";
}

async function poll() {
  try {
    const s = await window.pywebview.api.poll();
    pushLines(s.lines || []);
    paintState(s);
  } catch (e) {
    /* the window is closing, or Python is mid-restart: the next tick will pick it up */
  }
  setTimeout(poll, POLL_MS);
}

/* ------------------------------------------------------------------ tabs */

for (const tab of document.querySelectorAll(".tab")) {
  tab.addEventListener("click", () => {
    for (const t of document.querySelectorAll(".tab")) {
      const on = t === tab;
      t.classList.toggle("is-on", on);
      t.setAttribute("aria-selected", String(on));
    }
    for (const v of document.querySelectorAll(".view")) {
      v.classList.toggle("is-on", v.id === "view-" + tab.dataset.view);
    }
    if (tab.dataset.view === "history") loadRuns();
    if (tab.dataset.view === "settings") loadSettings();
  });
}

/* --------------------------------------------------------------- the ask */

$("ask").addEventListener("submit", async (e) => {
  e.preventDefault();
  const goal = $("goal").value.trim();
  if (!goal) return;
  $("goal").value = "";
  await window.pywebview.api.start(goal);
});

$("pause").addEventListener("click", () => window.pywebview.api.pause());
$("abort").addEventListener("click", () => window.pywebview.api.abort());
$("testVoice").addEventListener("click", () => window.pywebview.api.test_voice());

function setMode(act) {
  $("modeAct").classList.toggle("is-on", act);
  $("modeAct").setAttribute("aria-pressed", String(act));
  $("modeDry").classList.toggle("is-on", !act);
  $("modeDry").setAttribute("aria-pressed", String(!act));
  window.pywebview.api.set_mode(act, $("voiceOn").checked, $("hideOnAct").checked);
}

$("modeAct").addEventListener("click", () => setMode(true));
$("modeDry").addEventListener("click", () => setMode(false));
$("voiceOn").addEventListener("change", () => setMode($("modeAct").classList.contains("is-on")));
$("hideOnAct").addEventListener("change", () => setMode($("modeAct").classList.contains("is-on")));

/* --------------------------------------------------------------- history */

async function loadRuns() {
  const runs = await window.pywebview.api.runs();
  const list = $("runList");
  list.innerHTML = "";
  if (!runs.length) {
    list.innerHTML = '<li class="what">No runs yet.</li>';
    return;
  }
  for (const run of runs) {
    const li = document.createElement("li");
    li.innerHTML = `<div class="when"></div><div class="what"></div>`;
    li.querySelector(".when").textContent = run.name;
    li.querySelector(".what").textContent = run.goal || run.outcome;
    li.addEventListener("click", () => {
      for (const other of list.children) other.classList.remove("is-on");
      li.classList.add("is-on");
      showRun(run);
    });
    list.appendChild(li);
  }
}

function badge(run) {
  if (run.goal_achieved === true) return '<span class="badge good">achieved</span>';
  if (run.goal_achieved === false) return '<span class="badge bad">not achieved</span>';
  return '<span class="badge">' + run.outcome + "</span>";
}

async function showRun(run) {
  activeRun = run;
  const meta = $("runMeta");
  meta.className = "meta";
  meta.innerHTML = `
    <dt>Goal</dt><dd></dd>
    <dt>Outcome</dt><dd>${badge(run)} <span class="default">${run.outcome}${
    run.seconds != null ? " · " + run.seconds + "s" : ""
  } · ${run.steps_taken} steps${run.acted ? "" : " · dry run"}</span></dd>
    <dt>Answer</dt><dd class="answer-text"></dd>`;
  meta.querySelector("dd").textContent = run.goal || "(none recorded)";
  meta.querySelector(".answer-text").textContent = run.answer || "(none)";

  activeSteps = await window.pywebview.api.steps(run.name);
  const strip = $("stepStrip");
  strip.innerHTML = "";
  $("shot").hidden = true;
  $("shotEmpty").hidden = false;
  for (const step of activeSteps) {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = "step " + String(step.number).padStart(2, "0");
    b.addEventListener("click", () => {
      for (const other of strip.children) other.classList.remove("is-on");
      b.classList.add("is-on");
      showShot(run.name, step.number);
    });
    strip.appendChild(b);
  }
  if (!activeSteps.length) $("shotEmpty").textContent = "This run recorded no steps.";
}

async function showShot(name, number) {
  const data = await window.pywebview.api.shot(name, number);
  const img = $("shot");
  if (!data) {
    img.hidden = true;
    $("shotEmpty").hidden = false;
    $("shotEmpty").textContent = "That step wrote no capture.";
    return;
  }
  img.src = data;
  img.hidden = false;
  $("shotEmpty").hidden = true;
}

$("refresh").addEventListener("click", loadRuns);

/* -------------------------------------------------------------- settings */

async function loadSettings() {
  settingsRows = await window.pywebview.api.settings();
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
  await window.pywebview.api.save_settings(values);
  $("saved").textContent = "Saved. Hotkey changes need a restart.";
  setTimeout(() => ($("saved").textContent = ""), 4000);
});

/* ----------------------------------------------------------------- start */

window.addEventListener("pywebviewready", () => {
  poll();
  window.pywebview.api.settings().then(() => {});
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") $("goal").blur();
});
