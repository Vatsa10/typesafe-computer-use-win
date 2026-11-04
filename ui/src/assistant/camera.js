// One camera capture at a time, in a hidden window that exists only for the capture. The caller shows
// the indicator while `onOpen` has fired and `onClose` has not. Frames stay in memory.

const path = require("node:path");
const { BrowserWindow, ipcMain } = require("electron");

function createCamera({ onOpen = () => {}, onClose = () => {} } = {}) {
  let win = null;
  let settle = null;

  ipcMain.on("camera-result", (event, result) => {
    if (!win || win.isDestroyed() || event.sender !== win.webContents || !settle) return;
    settle(result && Array.isArray(result.images) && result.images.length ? { images: result.images } : { error: (result && result.error) || "no frames" });
  });

  function capture(seconds) {
    if (win) return Promise.resolve({ error: "the camera is already in use" });
    return new Promise((resolve) => {
      let timer = null;
      settle = (result) => {
        settle = null;
        if (timer) clearTimeout(timer);
        if (win && !win.isDestroyed()) win.destroy();
        win = null;
        onClose();
        resolve(result);
      };
      win = new BrowserWindow({
        show: false,
        width: 320,
        height: 240,
        title: "Pointer buddy",
        webPreferences: { preload: path.join(__dirname, "..", "preload.js"), contextIsolation: true, backgroundThrottling: false },
      });
      win.on("page-title-updated", (e) => e.preventDefault());
      onOpen();
      win.loadFile(path.join(__dirname, "camera", "camera.html"), { query: { seconds: String(seconds) } });
      timer = setTimeout(() => settle && settle({ error: "the camera did not answer" }), (Number(seconds) || 2) * 1000 + 12000);
    });
  }

  return { capture, isOpen: () => Boolean(win) };
}

module.exports = { createCamera };
