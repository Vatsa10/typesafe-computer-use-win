// The Electron shell. It owns windows and nothing else: every decision, every capture and every
// hotkey belongs to the Rust core, which runs as a child process and talks the line protocol in
// src/protocol.md over stdin and stdout.
//
// Why a child process rather than one binary: a global hotkey on Windows is delivered only to the
// thread that registered it, and only while that thread pumps messages. Electron's main thread is
// already running its own loop. Separate processes mean neither has to give up its loop — and a
// crash in the UI leaves a run that is already driving the machine able to finish or be aborted.

const { app, BrowserWindow, ipcMain } = require("electron");
const { spawn } = require("node:child_process");
const path = require("node:path");
const readline = require("node:readline");

const DEV_CORE = path.join(__dirname, "..", "..", "rust", "target", "debug", "winclicker-core.exe");
const PACKED_CORE = path.join(process.resourcesPath || "", "core.exe");

let core = null;
let panel = null;
let nextId = 1;
const pending = new Map();

function corePath() {
  return app.isPackaged ? PACKED_CORE : DEV_CORE;
}

function startCore() {
  core = spawn(corePath(), ["--ipc"], { stdio: ["pipe", "pipe", "pipe"] });

  readline.createInterface({ input: core.stdout }).on("line", (line) => {
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      return send("line", { text: line }); // the core printed something unstructured; show it anyway
    }
    if (message.id !== undefined) {
      const waiting = pending.get(message.id);
      pending.delete(message.id);
      if (waiting) waiting(message);
      return;
    }
    send(message.event, message);
  });

  // The core's stderr is for us, not the user: a panic belongs in the feed, not swallowed.
  readline.createInterface({ input: core.stderr }).on("line", (text) => send("line", { text }));

  core.on("exit", (code) => send("line", { text: `the core exited (${code})` }));
}

function send(event, payload) {
  if (panel && !panel.isDestroyed()) panel.webContents.send("core", { event, ...payload });
}

function call(method, params) {
  return new Promise((resolve) => {
    if (!core || core.killed) return resolve({ ok: false, error: "the core is not running" });
    const id = nextId++;
    pending.set(id, resolve);
    core.stdin.write(JSON.stringify({ id, method, params: params || {} }) + "\n");
    setTimeout(() => {
      if (pending.delete(id)) resolve({ ok: false, error: "the core did not answer" });
    }, 30000);
  });
}

ipcMain.handle("call", (_event, method, params) => call(method, params));

function createPanel() {
  panel = new BrowserWindow({
    width: 1120,
    height: 780,
    minWidth: 900,
    minHeight: 600,
    backgroundColor: "#0b1120",
    show: false,
    webPreferences: { preload: path.join(__dirname, "preload.js"), contextIsolation: true },
  });
  panel.removeMenu();
  panel.loadFile(path.join(__dirname, "index.html"));
  panel.once("ready-to-show", () => panel.show());
}

app.whenReady().then(() => {
  startCore();
  createPanel();
});

app.on("window-all-closed", () => {
  // The core drives the machine; it must not outlive the window that can stop it.
  if (core && !core.killed) core.kill();
  app.quit();
});
