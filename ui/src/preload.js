// The only bridge between the page and the shell. Context isolation is on, so the page gets these
// functions and nothing else — no require, no node, no filesystem.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("core", {
  call: (method, params) => ipcRenderer.invoke("call", method, params),
  on: (handler) => ipcRenderer.on("core", (_event, message) => handler(message)),
});
