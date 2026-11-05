// node src/stt/selftest-aec.js: the shell half of the echo-cancelling window, with a fake Electron
// and a fake core. No window, no microphone, no sound.

const { createAec } = require("./aec");
const fs = require("node:fs");
const path = require("node:path");

let failed = 0;
const check = (name, ok, extra = "") => {
  if (!ok) failed += 1;
  console.log(`[aec-selftest] ${ok ? "PASS" : "FAIL"} ${name}${extra ? ` (${extra})` : ""}`);
};

function fakeElectron() {
  const made = [];
  const handlers = new Map();
  class BrowserWindow {
    constructor(opts) {
      this.opts = opts;
      this.sent = [];
      this.destroyed = false;
      this.listeners = {};
      this.webContents = { send: (ch, m) => this.sent.push([ch, m]), on: () => {} };
      made.push(this);
    }
    on(name, fn) {
      this.listeners[name] = fn;
    }
    isDestroyed() {
      return this.destroyed;
    }
    destroy() {
      this.destroyed = true;
    }
    loadFile(file) {
      this.file = file;
      return Promise.resolve();
    }
  }
  const part = { setPermissionRequestHandler: (fn) => (part.request = fn), setPermissionCheckHandler: (fn) => (part.check = fn) };
  return {
    made,
    handlers,
    part,
    electron: {
      BrowserWindow,
      ipcMain: { on: (ch, fn) => handlers.set(ch, fn), removeListener: (ch) => handlers.delete(ch) },
      session: { fromPartition: () => part },
    },
  };
}

function run() {
  const calls = [];
  const fake = fakeElectron();
  const aec = createAec({ call: async (m, p) => calls.push({ m, p }), electron: fake.electron });
  aec.start();
  const win = fake.made[0];
  check("one window, hidden", fake.made.length === 1 && win.opts.show === false);
  check("it loads the audio page, not a remote one", path.basename(win.file) === "aec-window.html" && fs.existsSync(win.file));
  check("the page cannot reach node", win.opts.webPreferences.nodeIntegration === false && win.opts.webPreferences.contextIsolation === true);
  check("it is not throttled while hidden", win.opts.webPreferences.backgroundThrottling === false);
  check("only the microphone is granted", fake.part.check(null, "media") === true && fake.part.check(null, "geolocation") === false);

  const send = fake.handlers.get("aec:send");
  const from = { sender: win.webContents };
  send(from, { method: "aec_ready", params: { ready: true } });
  send(from, { method: "audio_in", params: { pcm_b64: "AAA=", seq: 0, t: 1 } });
  send(from, { method: "tts_state", params: { id: 1, state: "started" } });
  check("the page's three messages reach the core", calls.map((c) => c.m).join() === "aec_ready,audio_in,tts_state");
  check("ready follows the page", aec.ready() === true);

  const before = calls.length;
  send(from, { method: "stop_all", params: {} });
  send(from, { method: "audio_in", params: null });
  send({ sender: {} }, { method: "audio_in", params: { pcm_b64: "AAA=" } });
  check("anything else, or from another page, is refused", calls.length === before);

  check("core audio events go to the page", aec.handleEvent({ event: "tts_pcm", id: 1, pcm_b64: "AAA=", last: false }) === true);
  aec.handleEvent({ event: "aec_capture", on: true });
  aec.handleEvent({ event: "tts_stop", id: 1 });
  check("they arrive with their name and fields", win.sent.length === 3 && win.sent[0][1].name === "tts_pcm" && win.sent[0][1].id === 1 && win.sent[2][1].name === "tts_stop");
  check("other events are not ours", aec.handleEvent({ event: "line", text: "x" }) === false && win.sent.length === 3);

  win.listeners.closed();
  const last = calls[calls.length - 1];
  check("a closed window tells the core to take the microphone back", last.m === "aec_ready" && last.p.ready === false && aec.ready() === false);

  const fake2 = fakeElectron();
  const calls2 = [];
  const quiet = createAec({ call: async (m, p) => calls2.push({ m, p }), electron: fake2.electron });
  quiet.start();
  quiet.shutdown();
  check("shutdown is quiet", calls2.length === 0 && fake2.handlers.size === 0 && fake2.made[0].destroyed);

  // The page itself: the pieces the shell relies on are there.
  const page = fs.readFileSync(path.join(__dirname, "aec-window.html"), "utf8");
  check("the page asks for echo cancellation, noise suppression and gain control", /echoCancellation: true/.test(page) && /noiseSuppression: true/.test(page) && /autoGainControl: true/.test(page));
  check("it plays through an audio element", /<audio id="out"/.test(page) && /srcObject = dest\.stream/.test(page));
  check("mic frames are 30 ms at 16 kHz", /FRAME = 480/.test(page) && /sampleRate: 16000/.test(page));

  console.log(failed ? `[aec-selftest] ${failed} FAILED` : "[aec-selftest] all passed");
  process.exit(failed ? 1 : 0);
}

run();
