// The wake phrase, "Hey Pointer", spotted entirely on this device.
//
// How it works: a hidden window opens the microphone and hands 16 kHz audio to this process over IPC
// (wake/wake.html). Here, sherpa-onnx's streaming keyword spotter (a 3.3 MB zipformer model, Apache
// 2.0) listens for the one phrase. Nothing leaves the machine: no audio is written to disk, no audio
// goes to any service, and only the word "wake" crosses into the rest of the shell. The model is
// downloaded once, on enable, to %LOCALAPPDATA%\pointer\wake-model and checked against a pinned hash.
//
// While this is on, the microphone is open all the time. The Settings toggle says so.

const crypto = require("node:crypto");
const fs = require("node:fs");
const https = require("node:https");
const path = require("node:path");
const { execFile } = require("node:child_process");
const { BrowserWindow, ipcMain } = require("electron");
const { homeDir } = require("./prefs");

const MODEL_NAME = "sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01";
const MODEL_URL = `https://github.com/k2-fsa/sherpa-onnx/releases/download/kws-models/${MODEL_NAME}.tar.bz2`;
const MODEL_SHA256 = "f170013b4716e41b62b9bfd809687c207cef798ef9bc6534d524e17af9b6561a";
const SUFFIX = "epoch-12-avg-2-chunk-16-left-64.int8.onnx";
// The phrase, as the model's BPE tokens spell it: HEY POINTER.
const KEYWORD_LINE = "▁HE Y ▁POINT ER :2.0 #0.25 @HEY_POINTER\n";
const COOLDOWN_MS = 3000;

const modelRoot = () => path.join(homeDir(), "wake-model");
const modelDir = () => path.join(modelRoot(), MODEL_NAME);
const needed = (dir) => [`encoder-${SUFFIX}`, `decoder-${SUFFIX}`, `joiner-${SUFFIX}`, "tokens.txt"].map((f) => path.join(dir, f));
const hasModel = () => needed(modelDir()).every((f) => fs.existsSync(f));

function download(url, dest, onProgress, hops = 0) {
  return new Promise((resolve, reject) => {
    if (hops > 5) return reject(new Error("too many redirects"));
    https
      .get(url, { headers: { "User-Agent": "pointer" } }, (res) => {
        if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location) {
          res.resume();
          return resolve(download(new URL(res.headers.location, url).toString(), dest, onProgress, hops + 1));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`download failed (HTTP ${res.statusCode})`));
        }
        const total = Number(res.headers["content-length"]) || 0;
        const hash = crypto.createHash("sha256");
        const out = fs.createWriteStream(dest);
        let got = 0;
        res.on("data", (chunk) => {
          hash.update(chunk);
          got += chunk.length;
          if (total) onProgress(Math.round((got / total) * 100));
        });
        res.pipe(out);
        out.on("finish", () => out.close(() => resolve(hash.digest("hex"))));
        out.on("error", reject);
        res.on("error", reject);
      })
      .on("error", reject);
  });
}

function extract(archive, into) {
  const tar = path.join(process.env.SystemRoot || "C:\\Windows", "System32", "tar.exe"); // bsdtar ships with Windows 10 and 11
  return new Promise((resolve, reject) => {
    execFile(tar, ["-xjf", archive, "-C", into], { windowsHide: true }, (error) => (error ? reject(new Error(`could not unpack the model (${error.message})`)) : resolve()));
  });
}

async function ensureModel(onProgress) {
  if (hasModel()) return;
  fs.mkdirSync(modelRoot(), { recursive: true });
  const archive = path.join(modelRoot(), "download.tar.bz2");
  const staging = path.join(modelRoot(), "staging");
  fs.rmSync(staging, { recursive: true, force: true });
  fs.mkdirSync(staging, { recursive: true });
  try {
    const digest = await download(MODEL_URL, archive, onProgress);
    if (digest !== MODEL_SHA256) throw new Error("the downloaded model did not match its checksum; it was discarded");
    await extract(archive, staging);
    fs.rmSync(modelDir(), { recursive: true, force: true });
    fs.renameSync(path.join(staging, MODEL_NAME), modelDir());
  } finally {
    fs.rmSync(archive, { force: true });
    fs.rmSync(staging, { recursive: true, force: true });
  }
}

function makeSpotter(dir) {
  let sherpa;
  try {
    sherpa = require("sherpa-onnx-node");
  } catch (e) {
    throw new Error(`the keyword engine did not load (${e.message.split("\n")[0]})`);
  }
  const keywords = path.join(dir, "hey-pointer.txt");
  fs.writeFileSync(keywords, KEYWORD_LINE);
  const [encoder, decoder, joiner, tokens] = needed(dir);
  return new sherpa.KeywordSpotter({
    featConfig: { sampleRate: 16000, featureDim: 80 },
    modelConfig: { transducer: { encoder, decoder, joiner }, tokens, numThreads: 1, provider: "cpu", debug: 0 },
    keywordsFile: keywords,
  });
}

function createWake({ onWake, onState = () => {}, log = () => {} }) {
  let state = { state: "off", detail: "" };
  let spotter = null;
  let stream = null;
  let micWin = null;
  let busy = null;
  let lastHit = 0;
  let fed = 0;

  function set(next, detail = "") {
    state = { state: next, detail };
    onState(state);
  }

  function feed(samples) {
    if (!spotter || !stream) return;
    fed += samples.length;
    stream.acceptWaveform({ sampleRate: 16000, samples });
    while (spotter.isReady(stream)) {
      spotter.decode(stream);
      const result = spotter.getResult(stream);
      if (result.keyword) {
        spotter.reset(stream);
        const now = Date.now();
        if (now - lastHit >= COOLDOWN_MS) {
          lastHit = now;
          onWake();
        }
      }
    }
  }

  ipcMain.on("wake-audio", (event, samples) => {
    if (micWin && !micWin.isDestroyed() && event.sender === micWin.webContents && samples instanceof Float32Array) feed(samples);
  });
  ipcMain.on("wake-status", (event, report) => {
    if (!micWin || micWin.isDestroyed() || event.sender !== micWin.webContents) return;
    if (report && report.ok) set("on");
    else {
      set("error", `microphone: ${(report && report.error) || "unavailable"}`);
      closeMic();
    }
  });

  function closeMic() {
    if (micWin && !micWin.isDestroyed()) micWin.destroy();
    micWin = null;
  }

  function openMic() {
    micWin = new BrowserWindow({
      show: false,
      width: 200,
      height: 100,
      title: "Pointer buddy", // the core skips windows with this title; this one never shows anyway
      webPreferences: { preload: path.join(__dirname, "..", "preload.js"), contextIsolation: true, backgroundThrottling: false },
    });
    micWin.on("page-title-updated", (e) => e.preventDefault());
    micWin.loadFile(path.join(__dirname, "wake", "wake.html"));
  }

  async function enable() {
    if (busy) return busy;
    if (state.state === "on" || state.state === "starting") return state;
    busy = (async () => {
      try {
        if (!hasModel()) {
          set("downloading", "0%");
          await ensureModel((pct) => set("downloading", `${pct}%`));
        }
        set("starting");
        spotter = makeSpotter(modelDir());
        stream = spotter.createStream();
        fed = 0;
        openMic(); // status arrives over wake-status; on success the state becomes "on"
      } catch (e) {
        spotter = stream = null;
        closeMic();
        log(`wake phrase: ${e.message}`);
        set("error", e.message);
      } finally {
        busy = null;
      }
      return state;
    })();
    return busy;
  }

  function disable() {
    closeMic();
    spotter = stream = null;
    set("off");
  }

  return { enable, disable, state: () => state, fed: () => fed, hasModel, modelDir };
}

module.exports = { createWake, hasModel, modelDir, MODEL_NAME };
