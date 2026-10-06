// The Electron shell. It owns windows and nothing else: every decision, every capture and every
// hotkey belongs to the Rust core, which runs as a child process and talks the line protocol in
// src/protocol.md over stdin and stdout.
//
// Why a child process rather than one binary: a global hotkey on Windows is delivered only to the
// thread that registered it, and only while that thread pumps messages. Electron's main thread is
// already running its own loop. Separate processes mean neither has to give up its loop — and a
// crash in the UI leaves a run that is already driving the machine able to finish or be aborted.

const { app, BrowserWindow, ipcMain, screen } = require("electron");
const { spawn } = require("node:child_process");
const path = require("node:path");
const readline = require("node:readline");

const DEV_CORE = path.join(__dirname, "..", "..", "rust", "target", "debug", "winclicker-core.exe");
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

/* ------------------------------------------------------------------ the core */

function corePath() {
  return app.isPackaged ? PACKED_CORE : DEV_CORE;
}

function startCore() {
  core = spawn(corePath(), ["--ipc"], { stdio: ["pipe", "pipe", "pipe"], windowsHide: true });

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

function onEvent(message) {
  if (message.event === "hotkey" && message.name === "bar") return openBar();
  if (message.event === "highlight") return draw(message.marks || [], message.seconds || 5);
  if (message.event === "state" && message.running && panelHidesWhileActing) hidePanelForRun();
  if (message.event === "state" && !message.running) showPanelAfterRun();
  toPanel(message);
}

function toPanel(message) {
  if (panel && !panel.isDestroyed()) panel.webContents.send("core", message);
}

ipcMain.handle("call", (_event, method, params) => {
  if (method === "set_mode" && params) panelHidesWhileActing = params.hide !== false;
  return call(method, params);
});

/* --------------------------------------------------------------- the panel */

let panelHidesWhileActing = true;
let panelHiddenForRun = false;

function createPanel() {
  panel = new BrowserWindow({
    width: 1120,
    height: 780,
    minWidth: 900,
    minHeight: 600,
    backgroundColor: "#0b1120",
    title: "winclicker",
    show: false,
    webPreferences: { preload: path.join(__dirname, "preload.js"), contextIsolation: true },
  });
  panel.removeMenu();
  panel.loadFile(path.join(__dirname, "index.html"));
  panel.once("ready-to-show", () => panel.show());
  panel.on("closed", () => app.quit());
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
    bar.focus();
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
  bar.loadFile(path.join(__dirname, "bar.html"), { query: { listen: process.env.CLICKER_BAR_LISTEN === "0" ? "0" : "1" } });
  bar.once("ready-to-show", () => {
    bar.show();
    bar.focus();
  });
  bar.on("blur", () => {
    // Clicking away is a cancel. A bar left floating over the work is a bar in the way.
    if (bar && !bar.isDestroyed()) bar.close();
  });
}

ipcMain.handle("bar-done", async (_event, text, spoken) => {
  if (bar && !bar.isDestroyed()) bar.close();
  if (!text) return { ok: true };
  // A typed line is a goal. A spoken one is classified first: "stop" arrives as text too.
  return spoken ? call("say", { text }) : call("start", { goal: text, act: null });
});

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
  const right = Math.max(...marks.map((m) => m.x + m.w)) + 28;
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

/* ---------------------------------------------------------------- lifecycle */

app.whenReady().then(() => {
  startCore();
  createPanel();
});

app.on("window-all-closed", () => {
  // The core drives the machine; it must not outlive the window that can stop it.
  if (core && !core.killed) core.kill();
  app.quit();
});
