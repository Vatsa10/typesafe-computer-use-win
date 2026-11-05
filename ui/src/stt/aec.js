// The echo-cancelling audio window, shell side.
//
// A hidden BrowserWindow (never shown) owns the microphone and Pointer's playback, so Chromium's
// own echo canceller has the playback as its reference. This module creates it and relays between
// the page and the core: the page's `audio_in`, `tts_state` and `aec_ready` go to the core as
// requests; the core's `aec_capture`, `tts_pcm` and `tts_stop` events go to the page. Audio never
// touches a log. When the window cannot be made, the core records and plays itself.
//
//   const aec = createAec({ call, log });  // call(method, params) -> the core's reply
//   aec.start();                            // after the app is ready
//   in onEvent(message): if (aec.handleEvent(message)) return;
//   aec.announce();                         // when the core (re)starts
//   aec.shutdown();

const path = require("node:path");

const PAGE = path.join(__dirname, "aec-window.html");
const PRELOAD = path.join(__dirname, "aec-preload.js");
const TO_CORE = new Set(["audio_in", "tts_state", "aec_ready"]);
const TO_PAGE = new Set(["aec_capture", "tts_pcm", "tts_stop"]);

function createAec({ call, log = () => {}, electron = require("electron") } = {}) {
  const { BrowserWindow, ipcMain, session } = electron;
  let win = null;
  let ready = false;
  let quitting = false;

  function tell(method, params) {
    // Fire and forget: the reply to a frame carries nothing.
    Promise.resolve(call(method, params)).catch(() => {});
  }

  function setReady(next, reason) {
    ready = next;
    tell("aec_ready", reason ? { ready: next, reason } : { ready: next });
  }

  function onSend(event, message) {
    if (!win || win.isDestroyed() || event.sender !== win.webContents) return;
    const { method, params } = message || {};
    if (!TO_CORE.has(method) || typeof params !== "object" || params === null) return;
    if (method === "aec_ready") ready = Boolean(params.ready);
    tell(method, params);
  }

  function start() {
    if (win) return;
    const part = session.fromPartition("pointer-aec");
    // Only the microphone, and only for our own page.
    part.setPermissionRequestHandler((_wc, permission, cb) => cb(permission === "media"));
    part.setPermissionCheckHandler((_wc, permission) => permission === "media");
    ipcMain.on("aec:send", onSend);
    win = new BrowserWindow({
      show: false,
      width: 200,
      height: 100,
      skipTaskbar: true,
      webPreferences: {
        preload: PRELOAD,
        session: part,
        contextIsolation: true,
        nodeIntegration: false,
        sandbox: false,
        backgroundThrottling: false, // a hidden page must keep its audio and timers live
        autoplayPolicy: "no-user-gesture-required",
      },
    });
    const gone = (why) => {
      if (quitting) return;
      if (ready) log(`audio: the echo-cancelling window ${why}`);
      setReady(false);
    };
    win.on("closed", () => {
      win = null;
      gone("closed");
    });
    win.webContents.on("console-message", (_e, level, text) => level >= 2 && log(`audio window: ${text}`));
    win.webContents.on("render-process-gone", (_e, d) => gone(`stopped (${d.reason})`));
    win.webContents.on("did-fail-load", (_e, _c, text) => gone(`did not load (${text})`));
    win.loadFile(PAGE).catch((e) => gone(`did not load (${e.message})`));
  }

  /** A core event for the page. True when it was one of ours. */
  function handleEvent(message) {
    if (!message || !TO_PAGE.has(message.event)) return false;
    if (win && !win.isDestroyed()) {
      const { event: name, ...rest } = message;
      win.webContents.send("aec:event", { name, ...rest });
    }
    return true;
  }

  /** The core (re)started and knows nothing: say whether the window is up. */
  function announce() {
    if (ready) tell("aec_ready", { ready: true });
  }

  function shutdown() {
    quitting = true;
    ipcMain.removeListener("aec:send", onSend);
    if (win && !win.isDestroyed()) win.destroy();
    win = null;
    ready = false;
  }

  return { start, handleEvent, announce, shutdown, ready: () => ready, window: () => win };
}

module.exports = { createAec };
