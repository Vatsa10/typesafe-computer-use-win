// The only bridge between the page and the shell. Context isolation is on, so the page gets these
// functions and nothing else — no require, no node, no filesystem.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("core", {
  call: (method, params) => ipcRenderer.invoke("call", method, params),
  on: (handler) => ipcRenderer.on("core", (_event, message) => handler(message)),
});

// The command bar's own hooks. Whether it should start listening the moment it opens is decided by
// the shell and handed over in the page URL, because a sandboxed page cannot read the environment.
const listenOnOpen = new URLSearchParams(location.search).get("listen") === "1";
contextBridge.exposeInMainWorld("bar", {
  done: (text, spoken, purpose) => ipcRenderer.invoke("bar-done", text, spoken, purpose),
  listen: (action) => ipcRenderer.invoke("bar-listen", action),
  onInterim: (handler) => ipcRenderer.on("bar-interim", (_event, text) => handler(text)),
  reportFocus: (report) => ipcRenderer.send("bar-focus-report", report),
  listenOnOpen,
});

// The presence pill. It reports what the user does (hover, drag, typing, buttons); the shell owns
// every decision and sends back one snapshot of everything the pill shows.
contextBridge.exposeInMainWorld("pill", {
  onState: (handler) => ipcRenderer.on("pill-state", (_event, snapshot) => handler(snapshot)),
  ready: () => ipcRenderer.send("pill-ready"),
  hit: (over) => ipcRenderer.send("pill-hit", Boolean(over)),
  drag: (phase, sx, sy) => ipcRenderer.send("pill-drag", { phase, sx, sy }),
  focus: () => ipcRenderer.send("pill-focus"),
  blur: () => ipcRenderer.send("pill-blur"),
  send: (text) => ipcRenderer.send("pill-send", String(text || "")),
  mic: () => ipcRenderer.send("pill-mic"),
  stop: () => ipcRenderer.send("pill-stop"),
  taskStop: (id) => ipcRenderer.send("pill-task-stop", String(id)),
  confirm: (id, allow) => ipcRenderer.send("pill-confirm", String(id), allow === true),
  sendIgnored: (id) => ipcRenderer.send("pill-send-ignored", Number(id)),
});

// Shell-local preferences (wake phrase, camera) for the Settings view.
contextBridge.exposeInMainWorld("assistant", {
  getPrefs: () => ipcRenderer.invoke("assistant-prefs-get"),
  setPrefs: (patch) => ipcRenderer.invoke("assistant-prefs-set", patch),
  onWake: (handler) => ipcRenderer.on("assistant-wake", (_event, state) => handler(state)),
});

// The hidden microphone and camera pages talk back on these channels and no others.
const HIDDEN_CHANNELS = new Set(["wake-audio", "wake-status", "camera-result"]);
contextBridge.exposeInMainWorld("hidden", {
  send: (channel, payload) => {
    if (HIDDEN_CHANNELS.has(channel)) ipcRenderer.send(channel, payload);
  },
});
