// The Electron shell. It owns windows and nothing else: every decision, every capture and every
// hotkey belongs to the Rust core, which runs as a child process and talks the line protocol in
// src/protocol.md over stdin and stdout.
//
// Why a child process rather than one binary: a global hotkey on Windows is delivered only to the
// thread that registered it, and only while that thread pumps messages. Electron's main thread is
// already running its own loop. Separate processes mean neither has to give up its loop — and a
// crash in the UI leaves a run that is already driving the machine able to finish or be aborted.

const { app, BrowserWindow, ipcMain, nativeTheme, screen, systemPreferences } = require("electron");
const { spawn } = require("node:child_process");
const os = require("node:os");
const path = require("node:path");
const readline = require("node:readline");
const { createBridge } = require("./stt/bridge");

const DEV_CORE = path.join(__dirname, "..", "..", "rust", "target", "debug", "pointer-core.exe");
const PACKED_CORE = path.join(process.resourcesPath || "", "core.exe");
const BAR = { width: 680, height: 96 };
const MARGIN = 24;

let core = null;
let panel = null;
let bar = null;
let overlay = null;
let overlayTimer = null;
let nextId = 1;
const pending = new Map();
let voiceOn = true; // mirrors the panel's voice toggle (set_mode), which the core checks too

/* ------------------------------------------------------------------ the core */

function corePath() {
  return app.isPackaged ? PACKED_CORE : DEV_CORE;
}

function startCore() {
  // In a checkout the core runs from the repo root, where .env and runs/ already are; installed, it
  // keeps them under %LOCALAPPDATA%\pointer on its own.
  const cwd = app.isPackaged ? undefined : path.join(__dirname, "..", "..");
  // The core leaves this process's windows out of what it reads: the panel is never the app to work in.
  const env = { ...process.env, POINTER_UI_PID: String(process.pid) };
  core = spawn(corePath(), ["--ipc"], { cwd, env, stdio: ["pipe", "pipe", "pipe"], windowsHide: true });

  readline.createInterface({ input: core.stdout }).on("line", (line) => {
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      return toPanel({ event: "line", text: line }); // unstructured output still belongs in the feed
    }
    if (message.id !== undefined) {
      const waiting = pending.get(message.id);
      pending.delete(message.id);
      if (waiting) waiting(message);
      return;
    }
    onEvent(message);
  });

  // The core's stderr is for us, not the user: a panic belongs in the feed, not swallowed.
  readline.createInterface({ input: core.stderr }).on("line", (text) => toPanel({ event: "line", text }));
  core.on("exit", (code) => toPanel({ event: "line", text: `the core exited (${code})` }));
}

function call(method, params) {
  return new Promise((resolve) => {
    if (!core || core.killed) return resolve({ ok: false, error: "the core is not running" });
    const id = nextId++;
    pending.set(id, resolve);
    core.stdin.write(JSON.stringify({ id, method, params: params || {} }) + "\n");
    setTimeout(() => {
      if (pending.delete(id)) resolve({ ok: false, error: "the core did not answer" });
    }, 120000); // a run step or a transcription can take a while; the core is never silent forever
  });
}

/* ------------------------------------------------------- the free speech engine */

// CLICKER_STT=chrome (the default): the core cannot record on this engine, so the shell does,
// through Google's recognizer in a dedicated, hidden Chrome or Edge window (src/stt/bridge.js).
// Started on first use, kept alive, killed on quit. Transcripts are never logged here.
const speech = createBridge({
  log: (text) => toPanel({ event: "line", text }),
  // Live words while someone speaks to the command bar; they go to the bar only, never to a log.
  onInterim: (text) => {
    if (bar && !bar.isDestroyed()) bar.webContents.send("bar-interim", text);
  },
});

async function sttEngine() {
  const reply = await call("state");
  if (reply && reply.ok && reply.result.stt_lang) speech.setLang(reply.result.stt_lang);
  return reply && reply.ok ? reply.result.stt : "";
}

async function speechStart() {
  if (!voiceOn) return { ok: false, error: "voice is off" };
  try {
    return { ok: true, result: { listening: await speech.start() } };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

async function speechStop() {
  const t0 = Date.now();
  const heard = await speech.stop();
  if (SELFTEST) console.log(`[stt-selftest] stop -> heard=${JSON.stringify(heard)} error=${speech.lastError() || "none"} in ${Date.now() - t0}ms`);
  return { ok: true, result: { heard } };
}

// Push-to-talk on the chrome engine: the core brackets the held key with start and stop, and what
// was heard goes back as `heard`, which routes exactly like `say`.
async function onSttEvent(message) {
  if (message.action === "start" && recordingHotkeys) return; // Settings is capturing a combo
  if (message.action === "start") {
    const started = await speechStart();
    if (!started.ok) toPanel({ event: "line", text: `voice: ${started.error}` });
  } else if (message.action === "stop") {
    const { result } = await speechStop();
    // The purpose rides through untouched: dictation is typed into the focused field by the core and
    // never routed. Dictated words may be private, so only talk is echoed into the feed.
    const purpose = message.purpose === "dictate" ? "dictate" : "talk";
    if (!result.heard) return toPanel({ event: "line", text: "heard nothing" });
    if (purpose === "talk") toPanel({ event: "line", text: `heard: ${result.heard}` });
    const routed = await call("heard", { text: result.heard, purpose });
    if (!routed.ok) toPanel({ event: "line", text: `voice: ${routed.error}` });
  }
}

// While Settings records a new combo, pressing an existing one must not open the bar or the panel.
let recordingHotkeys = false;

function onEvent(message) {
  if (message.event === "stt") return onSttEvent(message);
  if (message.event === "hotkey" && recordingHotkeys && (message.name === "bar" || message.name === "goal")) return;
  if (message.event === "hotkey" && message.name === "bar") return openBar();
  if (message.event === "hotkey" && message.name === "goal") showPanel();
  if (message.event === "point") return point(message);
  if (message.event === "highlight") return draw(message.marks || [], message.seconds || 5);
  if (message.event === "state" && message.running && panelHidesWhileActing) hidePanelForRun();
  if (message.event === "state" && !message.running) showPanelAfterRun();
  toPanel(message);
}

function toPanel(message) {
  if (panel && !panel.isDestroyed()) panel.webContents.send("core", message);
}

ipcMain.handle("call", async (_event, method, params) => {
  if (method === "ui_recording_hotkeys") {
    recordingHotkeys = Boolean(params && params.on);
    return { ok: true, result: { recording: recordingHotkeys } };
  }
  if (method === "set_mode" && params) {
    panelHidesWhileActing = params.hide !== false;
    if (typeof params.voice === "boolean") voiceOn = params.voice;
  }
  // On the chrome engine the panel's and the bar's mic are answered here, from the bridge.
  if ((method === "listen_start" || method === "listen_stop") && (await sttEngine()) === "chrome") {
    return method === "listen_start" ? speechStart() : speechStop();
  }
  const reply = await call(method, params);
  // Self-test only: a core built before `hotkeys` existed still lets the recorder be looked at.
  if (process.env.POINTER_BAR_SELFTEST === "1" && !reply.ok && (method === "hotkeys" || method === "set_hotkeys")) {
    const current = { bar: "rightalt", talk: "ctrl+alt+space", dictate: "ctrl+alt+d", goal: "ctrl+alt+g", pause: "ctrl+alt+p", abort: "ctrl+alt+x", quit: "ctrl+alt+q" };
    if (method === "hotkeys") return { ok: true, result: { current, defaults: current } };
    return { ok: true, result: { applied: params.values, refused: [] } };
  }
  return reply;
});

/* --------------------------------------------------------------- the panel */

let panelHidesWhileActing = true;
let panelHiddenForRun = false;

// Mica is a Windows 11 material (build 22000 and later). It is opt-in (POINTER_MICA=1): on Electron
// 33 with a hidden title bar, a Mica window showed only the material and never presented the page.
// Without it the page paints its own solid surface; the panel learns which through its URL.
const MICA = process.env.POINTER_MICA === "1" && process.platform === "win32" && Number(os.release().split(".")[2] || 0) >= 22000;
// The custom title bar (titleBarStyle: hidden + titleBarOverlay) is opt-in too
// (POINTER_CUSTOM_TITLEBAR=1, implied by POINTER_MICA=1): on Electron 33 and this Windows build such
// a window was never presented on screen until something forced a repaint. The native frame
// follows nativeTheme by itself.
const CUSTOM_TITLEBAR = MICA || process.env.POINTER_CUSTOM_TITLEBAR === "1";
const TITLEBAR_HEIGHT = 40; // matches --titlebar-height in styles/tokens.css

// Solid fallbacks, and the caption-button colours, for each theme. These mirror the surface and
// text tokens in styles/tokens.css; the shell cannot read CSS, so they are repeated here once.
const THEME = {
  light: { surface: "#f3f3f3", symbol: "#1a1a1a" },
  dark: { surface: "#202020", symbol: "#ffffff" },
};

function theme() {
  return nativeTheme.shouldUseDarkColors ? THEME.dark : THEME.light;
}

function titleBarOverlay() {
  // Over Mica the caption buttons sit on the material itself; without it they match the surface.
  return { color: MICA ? "#00000000" : theme().surface, symbolColor: theme().symbol, height: TITLEBAR_HEIGHT };
}

function createPanel() {
  panel = new BrowserWindow({
    icon: path.join(__dirname, "icon.ico"),
    width: 1120,
    height: 780,
    minWidth: 900,
    minHeight: 600,
    backgroundColor: MICA ? "#00000000" : theme().surface,
    backgroundMaterial: MICA ? "mica" : "none",
    ...(CUSTOM_TITLEBAR ? { titleBarStyle: "hidden", titleBarOverlay: titleBarOverlay() } : {}),
    title: "Pointer",
    show: false,
    webPreferences: { preload: path.join(__dirname, "preload.js"), contextIsolation: true },
  });
  panel.removeMenu();
  panel.loadFile(path.join(__dirname, "index.html"), { query: { mica: MICA ? "1" : "0", titlebar: CUSTOM_TITLEBAR ? "custom" : "native" } });
  panel.once("ready-to-show", () => panel.show());
  panel.on("closed", () => app.quit());
}

nativeTheme.on("updated", () => {
  if (!panel || panel.isDestroyed()) return;
  if (CUSTOM_TITLEBAR) panel.setTitleBarOverlay(titleBarOverlay());
  if (!MICA) panel.setBackgroundColor(theme().surface);
});

function showPanel() {
  if (!panel || panel.isDestroyed()) return;
  if (panel.isMinimized()) panel.restore();
  panel.show();
  panel.focus();
}

// The panel is on screen, so a run would read its buttons as things to click. The Python version
// learned this from a run that offered "Dry run (decide and report, touch nothing)" as a target.
function hidePanelForRun() {
  if (panel && !panel.isDestroyed() && panel.isVisible()) {
    panelHiddenForRun = true;
    panel.minimize();
  }
}

function showPanelAfterRun() {
  if (panelHiddenForRun && panel && !panel.isDestroyed()) {
    panelHiddenForRun = false;
    panel.restore();
  }
}

/* ------------------------------------------------------------ the command bar */

function atCursor() {
  // Electron speaks DIPs, consistently, so cursor, display and window bounds all agree here. The
  // physical-pixel world of the core never enters this function.
  const point = screen.getCursorScreenPoint();
  const area = screen.getDisplayNearestPoint(point).workArea;
  const x = Math.min(Math.max(point.x - BAR.width / 2, area.x + MARGIN), area.x + area.width - BAR.width - MARGIN);
  const y = Math.min(Math.max(point.y + 28, area.y + MARGIN), area.y + area.height - BAR.height - MARGIN);
  return { x: Math.round(x), y: Math.round(y) };
}

function openBar() {
  if (bar && !bar.isDestroyed()) {
    focusBar();
    return;
  }
  const { x, y } = atCursor();
  bar = new BrowserWindow({
    width: BAR.width,
    height: BAR.height,
    x,
    y,
    frame: false,
    transparent: true,
    resizable: false,
    alwaysOnTop: true,
    skipTaskbar: true,
    show: false,
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
    },
  });
  bar.setAlwaysOnTop(true, "screen-saver");
  const listen = process.env.CLICKER_BAR_LISTEN === "0" || !voiceOn ? "0" : "1";
  bar.loadFile(path.join(__dirname, "bar.html"), { query: { listen } });
  bar.once("ready-to-show", focusBar);
  bar.on("blur", () => {
    if (!bar || bar.isDestroyed()) return;
    // The first listen launches the speech browser, whose window can take the foreground for a
    // moment. That is not the user clicking away: take the focus back instead of closing.
    if (Date.now() < barHoldOpenUntil) {
      barLog("blurred while the speech browser started; taking focus back");
      setTimeout(focusBar, 100);
      return;
    }
    // Clicking away is a cancel. A bar left floating over the work is a bar in the way.
    bar.close();
  });
  bar.on("closed", () => {
    // Whatever was being heard for the bar is dropped with it.
    if (barListening) cancelBarSpeech();
    bar = null;
  });
}

const BAR_DEBUG = process.env.POINTER_BAR_DEBUG === "1" || process.env.POINTER_BAR_SELFTEST === "1";

function barLog(text) {
  if (BAR_DEBUG) console.log(`[bar] ${text}`);
}

// Windows refuses SetForegroundWindow to a process that is not already in front, so a window
// opened from a global hotkey can appear without the keyboard. The core calls
// AllowSetForegroundWindow before it emits the hotkey; here we ask for focus, and if Windows still
// said no, ask once more, harder, 50 ms later.
function focusBar() {
  if (!bar || bar.isDestroyed()) return;
  bar.show();
  bar.focus();
  bar.webContents.focus();
  barLog(`shown, isFocused=${bar.isFocused()}`);
  if (!bar.isFocused()) {
    setTimeout(() => {
      if (!bar || bar.isDestroyed()) return;
      app.focus({ steal: true });
      bar.focus();
      bar.webContents.focus();
      barLog(`retried with app.focus({steal:true}), isFocused=${bar.isFocused()}`);
    }, 50);
  }
}

let barListening = false;
let barHoldOpenUntil = 0; // until then a blur is the speech browser starting, not a click away
let barEngineLive = false; // the bar's speech is on the chrome bridge, so it can be cancelled

function cancelBarSpeech() {
  barListening = false;
  if (barEngineLive) speech.cancel();
  else call("listen_stop"); // the other engines have no cancel; the answer is ignored
}

function closeBar() {
  if (bar && !bar.isDestroyed()) bar.close();
}

ipcMain.handle("bar-listen", async (_event, action) => {
  if (action === "start") {
    if (!voiceOn) return { ok: false, error: "voice is off" };
    barEngineLive = (await sttEngine()) === "chrome";
    barHoldOpenUntil = Date.now() + 30000;
    const started = barEngineLive ? await speechStart() : await call("listen_start");
    barHoldOpenUntil = Date.now() + 1000;
    barListening = Boolean(started.ok);
    return { ...started, live: barEngineLive }; // live: interim words stream, so silence can submit
  }
  if (!barListening) return { ok: true, result: { heard: "" } };
  if (action === "stop") {
    barListening = false;
    return barEngineLive ? speechStop() : call("listen_stop");
  }
  // cancel: the user started typing. Drop the utterance; nothing is transcribed or routed.
  cancelBarSpeech();
  return { ok: true };
});

ipcMain.on("bar-focus-report", (_event, report) => barLog(`renderer reports ${JSON.stringify(report)}`));

ipcMain.handle("bar-done", async (_event, text, spoken, purpose) => {
  if (barListening) cancelBarSpeech();
  closeBar();
  if (!text) return { ok: true };
  if (purpose === "dictate") {
    // Dictation types into the focused field, and the bar had the focus: it is closed now, so give
    // Windows a moment to hand focus back to the window underneath, then let the core type.
    await new Promise((resolve) => setTimeout(resolve, 150));
    return call("heard", { text, purpose: "dictate" });
  }
  // Typed or spoken, the bar is classified: a goal runs, a question is taught, "stop" stops. Typed
  // text was not misheard, so it goes in as `heard` (fully heard); speech goes through `say`.
  return spoken ? call("say", { text }) : call("heard", { text, purpose: "talk" });
});

// A development check (POINTER_BAR_SELFTEST=1): open the bar directly, report whether it has the
// keyboard, and only if it is the focused window type into it (sendInputEvent goes to the bar's own
// page, never to another app). Esc closes it without submitting, so nothing runs. Then open Settings
// and start the hotkey recorder. Window captures go to POINTER_BAR_SELFTEST_DIR when it is set.
function barSelfTest() {
  const fs = require("node:fs");
  const dir = process.env.POINTER_BAR_SELFTEST_DIR;
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const shot = async (win, name) => {
    if (!dir || !win || win.isDestroyed()) return;
    fs.mkdirSync(dir, { recursive: true });
    const image = await win.webContents.capturePage();
    fs.writeFileSync(path.join(dir, name), image.toPNG());
    console.log(`[bar-selftest] saved ${name}`);
  };
  const barState = () =>
    bar.webContents.executeJavaScript(
      `({ hasFocus: document.hasFocus(), active: document.activeElement && document.activeElement.id, mode: document.getElementById("mode").textContent, value: document.getElementById("barGoal").value })`,
    );
  setTimeout(async () => {
    openBar();
    await wait(Number(process.env.POINTER_BAR_SELFTEST_MS || 6000)); // the first listen launches the speech browser
    if (!bar || bar.isDestroyed()) return console.log("[bar-selftest] the bar closed before the check");
    const state = await barState();
    console.log(`[bar-selftest] after open: isFocused=${bar.isFocused()} listening=${barListening} ${JSON.stringify(state)}`);
    await shot(bar, "bar-open.png");
    if (!bar || bar.isDestroyed()) return console.log("[bar-selftest] the bar closed during the capture");
    // The guard: type only when the bar is the focused window and its page has the focus.
    if (bar.isFocused() && state.hasFocus) {
      for (const ch of "what is on my screen") {
        const keyCode = ch === " " ? "Space" : ch;
        bar.webContents.sendInputEvent({ type: "keyDown", keyCode });
        bar.webContents.sendInputEvent({ type: "char", keyCode: ch });
        bar.webContents.sendInputEvent({ type: "keyUp", keyCode });
        await wait(25);
      }
      await wait(300);
      console.log(`[bar-selftest] after typing: listening=${barListening} ${JSON.stringify(await barState())}`);
      await shot(bar, "bar-typing.png");
      bar.webContents.sendInputEvent({ type: "keyDown", keyCode: "Escape" });
      await wait(400);
      console.log(`[bar-selftest] after Esc: bar open=${Boolean(bar && !bar.isDestroyed())}`);
    } else {
      console.log("[bar-selftest] the bar is not the focused window; not typing");
      closeBar();
    }
    showPanel();
    await panel.webContents.executeJavaScript(`document.querySelectorAll(".nav-item")[2].click()`);
    await wait(1500);
    const clicked = await panel.webContents.executeJavaScript(`(() => { const b = document.querySelector(".hk-change"); if (b) b.click(); return Boolean(b); })()`);
    await wait(300);
    console.log(`[bar-selftest] recorder started=${clicked} recordingHotkeys=${recordingHotkeys}`);
    await shot(panel, "settings-recording.png");
    // A combo into the recorder (the panel's own page only), to show a captured, unapplied value.
    if (panel.isFocused()) {
      panel.webContents.sendInputEvent({ type: "keyDown", keyCode: "K", modifiers: ["control", "alt"] });
      await wait(400);
    }
    await shot(panel, "settings-hotkeys.png");
    console.log("[bar-selftest] done");
  }, 2500);
}

/* ------------------------------------------------------------- the overlay */

// Marks arrive from the core in PHYSICAL pixels on the virtual desktop, because that is what the
// core captures and clicks in. This desk runs three monitors at three different scale factors —
// 150%, 125% and 100% — so physical and DIP coordinates differ per monitor, and the monitor to the
// right sits at x=2561 in one and x=2049 in the other. Every mark goes through screenToDipRect, which
// knows which monitor a rectangle is on; scaling by one global factor would draw on the wrong spot.
function toDip(mark) {
  const rect = screen.screenToDipRect(null, {
    x: Math.round(mark.x),
    y: Math.round(mark.y),
    width: Math.max(1, Math.round(mark.w)),
    height: Math.max(1, Math.round(mark.h)),
  });
  return { ...mark, x: rect.x, y: rect.y, w: rect.width, h: rect.height };
}

function draw(physicalMarks, seconds) {
  if (!physicalMarks.length) return;
  const marks = physicalMarks.slice(0, 6).map(toDip); // more than a handful stops being a hint
  const left = Math.min(...marks.map((m) => m.x)) - 28;
  const top = Math.min(...marks.map((m) => m.y)) - 56; // labels sit above their box
  // A label is often wider than its box ("would press: Following" over a short tab), so the window
  // must reach past whichever is wider. ponytail: ~8 DIP per character estimate, measure if it clips.
  const labelEnd = (m) => m.x + (m.label ? m.label.length * 8 + 32 : 0);
  const right = Math.max(...marks.map((m) => Math.max(m.x + m.w, labelEnd(m)))) + 28;
  const bottom = Math.max(...marks.map((m) => m.y + m.h)) + 28;
  const local = marks.map((m) => ({ ...m, x: m.x - left, y: m.y - top }));

  clearOverlay();
  overlay = new BrowserWindow({
    x: Math.round(left),
    y: Math.round(top),
    width: Math.round(right - left),
    height: Math.round(bottom - top),
    frame: false,
    transparent: true,
    focusable: false, // never steals focus from the app being explained
    skipTaskbar: true,
    resizable: false,
    hasShadow: false,
    alwaysOnTop: true,
    show: false,
    webPreferences: { preload: path.join(__dirname, "preload.js"), contextIsolation: true },
  });
  overlay.setAlwaysOnTop(true, "screen-saver");
  // Clicks go straight through to whatever is underneath: this window points, it never intercepts.
  overlay.setIgnoreMouseEvents(true);
  overlay.loadFile(path.join(__dirname, "overlay.html"));
  overlay.webContents.once("did-finish-load", () => {
    overlay.webContents.send("core", { event: "draw", marks: local });
    overlay.showInactive();
  });
  overlayTimer = setTimeout(clearOverlay, seconds * 1000);
}

function clearOverlay() {
  if (overlayTimer) clearTimeout(overlayTimer);
  overlayTimer = null;
  if (overlay && !overlay.isDestroyed()) overlay.destroy();
  overlay = null;
}

ipcMain.handle("draw", (_event, marks, seconds) => draw(marks || [], seconds || 5));

/* --------------------------------------------------------------- the buddy */

// The cursor buddy (ADR-1 in docs/superpowers/specs/2026-10-07-buddy-teach-dictate-design.md): one
// small click-through window that glides to whatever the core points at, while draw() boxes the
// target. It is created by this process, so POINTER_UI_PID already keeps the core from reading it;
// the title "Pointer buddy" is a second guard the core matches on.
const BUDDY = { width: 300, height: 130, tipX: 8, tipY: 6, glideMs: 350 };
let buddy = null;
let buddyReady = null;
let buddyVisible = false;
let buddyTimer = null;
let buddyGlide = null;

function ensureBuddy() {
  if (buddy && !buddy.isDestroyed()) return buddyReady;
  buddy = new BrowserWindow({
    width: BUDDY.width,
    height: BUDDY.height,
    title: "Pointer buddy",
    frame: false,
    transparent: true,
    focusable: false, // never steals focus from the app being explained
    skipTaskbar: true,
    resizable: false,
    hasShadow: false,
    alwaysOnTop: true,
    show: false,
    webPreferences: { preload: path.join(__dirname, "preload.js"), contextIsolation: true },
  });
  buddy.setAlwaysOnTop(true, "screen-saver");
  buddy.setIgnoreMouseEvents(true);
  buddy.on("page-title-updated", (event) => event.preventDefault()); // the title is the core's guard
  buddy.on("closed", () => (buddy = null));
  buddyReady = new Promise((resolve) => buddy.webContents.once("did-finish-load", resolve));
  buddy.loadFile(path.join(__dirname, "buddy.html"));
  return buddyReady;
}

function prefersReducedMotion() {
  try {
    return systemPreferences.getAnimationSettings().prefersReducedMotion;
  } catch {
    return false;
  }
}

// Where the window goes so the arrow's tip rests just inside the target's left edge, at its middle;
// the bubble then hangs down-right of the tip. Kept inside the work area of the target's monitor.
function parkFor(target) {
  const tip = { x: target.x + Math.min(target.w * 0.25, 18), y: target.y + target.h / 2 };
  const area = screen.getDisplayNearestPoint({ x: Math.round(tip.x), y: Math.round(tip.y) }).workArea;
  const x = Math.min(Math.max(tip.x - BUDDY.tipX, area.x), area.x + area.width - BUDDY.width);
  const y = Math.min(Math.max(tip.y - BUDDY.tipY, area.y), area.y + area.height - BUDDY.height);
  return { x: Math.round(x), y: Math.round(y) };
}

function glideTo(to) {
  if (buddyGlide) clearInterval(buddyGlide);
  buddyGlide = null;
  const size = { width: BUDDY.width, height: BUDDY.height };
  let from;
  if (buddyVisible) {
    const b = buddy.getBounds();
    from = { x: b.x, y: b.y };
  } else {
    const c = screen.getCursorScreenPoint(); // already DIP
    from = { x: c.x - BUDDY.tipX, y: c.y - BUDDY.tipY };
  }
  if (prefersReducedMotion()) return buddy.setBounds({ ...to, ...size });
  buddy.setBounds({ ...from, ...size });
  const start = Date.now();
  buddyGlide = setInterval(() => {
    if (!buddy || buddy.isDestroyed()) return clearInterval(buddyGlide);
    const t = Math.min(1, (Date.now() - start) / BUDDY.glideMs);
    const e = 1 - Math.pow(1 - t, 3); // ease-out cubic
    buddy.setBounds({ x: Math.round(from.x + (to.x - from.x) * e), y: Math.round(from.y + (to.y - from.y) * e), ...size });
    if (t >= 1) {
      clearInterval(buddyGlide);
      buddyGlide = null;
      buddy.setBounds({ ...to, ...size }); // a mixed-DPI crossing can rescale mid-glide; land exactly
    }
  }, 16);
}

async function point(message) {
  if (message.clear) return hideBuddy();
  const target = toDip(message);
  const hold = Number(message.hold) > 0 ? Number(message.hold) : 5;
  draw([{ x: message.x, y: message.y, w: message.w, h: message.h, tone: message.tone }], hold); // the box; the bubble carries the label
  await ensureBuddy();
  if (!buddy || buddy.isDestroyed()) return;
  buddy.webContents.send("core", { event: "buddy", label: message.label, tone: message.tone, step: message.step, of: message.of });
  glideTo(parkFor(target));
  if (!buddyVisible) buddy.showInactive();
  buddyVisible = true;
  buddy.moveTop(); // above the box overlay that draw() just made
  if (buddyTimer) clearTimeout(buddyTimer);
  buddyTimer = setTimeout(hideBuddy, hold * 1000);
}

function hideBuddy() {
  if (buddyTimer) clearTimeout(buddyTimer);
  if (buddyGlide) clearInterval(buddyGlide);
  buddyTimer = buddyGlide = null;
  clearOverlay();
  if (!buddy || buddy.isDestroyed() || !buddyVisible) return;
  buddyVisible = false;
  buddy.webContents.send("core", { event: "buddy-hide" });
  buddy.hide();
}

// A development check (POINTER_BUDDY_SELFTEST=1): point at a spot on every monitor in turn, in
// physical pixels, through the same handler the core's events take.
function buddySelfTest() {
  const displays = screen.getAllDisplays().sort((a, b) => a.bounds.x - b.bounds.x);
  displays.forEach((d, i) => {
    setTimeout(() => {
      const dip = { x: d.workArea.x + Math.round(d.workArea.width * 0.4), y: d.workArea.y + Math.round(d.workArea.height * 0.35), width: 160, height: 40 };
      const r = screen.dipToScreenRect(null, dip);
      console.log(`[buddy-selftest] display ${i + 1} scale=${d.scaleFactor} dip=${JSON.stringify(dip)} physical=${JSON.stringify(r)}`);
      onEvent({ event: "point", x: r.x, y: r.y, w: r.width, h: r.height, label: `Step on monitor ${i + 1}: this is the button to press`, tone: i === 1 ? "note" : "point", step: i + 1, of: displays.length, hold: 6 });
    }, 1500 + i * 4000);
  });
}

/* ---------------------------------------------------------------- lifecycle */

app.whenReady().then(() => {
  startCore();
  createPanel();
  if (process.env.POINTER_STT_SELFTEST === "1") panel.webContents.once("did-finish-load", sttSelfTest);
  if (process.env.POINTER_BAR_SELFTEST === "1") panel.webContents.once("did-finish-load", barSelfTest);
  if (process.env.POINTER_BUDDY_SELFTEST === "1") panel.webContents.once("did-finish-load", buddySelfTest);
});

// A development check (POINTER_STT_SELFTEST=1): click the panel's mic, wait, click it again, and
// print to stdout what the bridge answered — the whole path a user's click takes.
const SELFTEST = process.env.POINTER_STT_SELFTEST === "1";

function sttSelfTest() {
  const click = () => panel.webContents.executeJavaScript(`document.querySelector("button.mic").click()`);
  setTimeout(async () => {
    console.log(`[stt-selftest] engine=${await sttEngine()} browser=${speech.browser()}`);
    await click();
    setTimeout(async () => {
      console.log(`[stt-selftest] speech browser pid=${speech.pid()} recognizer listening=${speech.listening()}; clicking stop`);
      await click();
    }, Number(process.env.POINTER_STT_SELFTEST_MS || 6000));
  }, 2000);
}
