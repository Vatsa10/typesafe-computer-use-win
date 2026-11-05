// The only door between the hidden audio window and the shell: it can send three kinds of message
// to the core (through aec.js, which checks them) and receive the core's audio events.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("aecBridge", {
  send: (method, params) => ipcRenderer.send("aec:send", { method, params }),
  on: (handler) => ipcRenderer.on("aec:event", (_e, message) => handler(message)),
});
