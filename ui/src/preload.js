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
