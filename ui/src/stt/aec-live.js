// Development measurement, not shipped behaviour. Runs the real core and (in window mode) the real
// hidden audio window together and times barge-in:
//
//   electron src/stt/aec-live.js window|core
//
// env: POINTER_CORE (pointer-core.exe), POINTER_HOME (a folder with the .env holding the keys),
//      POINTER_BARGE_CLIP + POINTER_VOICE_TOOL (voice_tool.exe) to play one clip aloud (window mode).
// Pointer is asked a long question and speaks; its own voice heard back counts as a self-trigger
// (words the microphone picked up while only Pointer was sounding). Then the clip is played over it,
// and the shell's steps are timed: words heard, `interrupt`, the window told to stop, sound ended.
// `core` mode records and plays in the core (no window) and only times first audio. Words are
// counted, never printed.

process.on("uncaughtException", (e) => { console.log(`[aec-live] error: ${e.stack}`); process.exit(1); });
const { app } = require("electron");
const { spawn } = require("node:child_process");
const readline = require("node:readline");
const { createAec } = require("./aec");

const mode = ["core", "tone"].includes(process.argv.slice(-1)[0]) ? process.argv.slice(-1)[0] : "window";
const log = (t) => console.log(`[aec-live] ${t}`);
const t0 = Date.now();
const rel = () => Date.now() - t0;
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

let core;
let nextId = 1;
const pending = new Map();
const seen = []; // { at, event }
let aec = null;
let speaking = false;
let clipAt = 0;
const times = {};
let selfTriggers = 0;
let maxWords = 0;
let clipWords = 0;
let firstPcmAt = 0;
let stats = null;

function call(method, params) {
  return new Promise((resolve) => {
    const id = nextId++;
    pending.set(id, resolve);
    core.stdin.write(JSON.stringify({ id, method, params: params || {} }) + "\n");
    setTimeout(() => pending.delete(id) && resolve({ ok: false, error: "timeout" }), 60000);
  });
}
// audio frames are fire and forget: do not wait for their replies
let captureOnAt = 0, firstFrameAt = 0;
function callQuiet(method, params) {
  if (method !== "audio_in") log(`window -> core ${method} ${JSON.stringify(params)} +${rel()}`);
  if (method === "audio_in" && captureOnAt && !firstFrameAt) { firstFrameAt = Date.now(); log(`first mic frame ${firstFrameAt - captureOnAt} ms after aec_capture on`); }
  const id = nextId++;
  core.stdin.write(JSON.stringify({ id, method, params: params || {} }) + "\n");
  return Promise.resolve({ ok: true });
}

function onEvent(m) {
  if (process.env.AEC_TRACE && !["tts_pcm", "transcript"].includes(m.event)) log(`ev +${rel()} ${m.event} ${m.action || m.state || ""} ${m.on === undefined ? "" : "on=" + m.on} ${m.real ? "real" : ""} ${m.reason || ""}`);
  if (m.event === "aec_capture" && m.on) { captureOnAt = Date.now(); firstFrameAt = 0; }
  if (m.event === "tts_pcm" && !firstPcmAt) { firstPcmAt = Date.now(); log(`first tts_pcm id=${m.id} rate=${m.rate} +${rel()}`); }
  if (m.event === "tts_stop") times.ttsStopEvent = Date.now();
  if (aec && aec.handleEvent(m)) return;
  if (m.event === "stt_stats") stats = m;
  if (m.event === "line") log(`core line: ${m.text}`);
  if (m.event === "speech" && m.real && !m.ack) {
    if (m.action === "start") {
      speaking = true;
      times.start = Date.now();
      log(`speech really started +${rel()} ms, first_audio_ms=${m.first_audio_ms}${firstPcmAt ? `, first frame to window ${times.start - firstPcmAt} ms before it` : ""}`);
    } else {
      speaking = false;
      times.end = Date.now();
      log(`speech really ended +${rel()} ms cut=${m.cut}`);
    }
  }
  if (m.event === "transcript") {
    const words = String(m.text || "").split(/\s+/).filter(Boolean).length;
    maxWords = Math.max(maxWords, words);
    if (speaking && !clipAt && words >= 2) selfTriggers += 1;
    if (clipAt && !times.heard && words >= 2) {
      times.heard = Date.now();
      clipWords = words;
      shellReacts();
    }
  }
}

// What the shell's conversation controller does with live words while Pointer speaks.
async function shellReacts() {
  const g = await call("gate", { heard: "x x", speaking: true });
  times.interruptAt = Date.now();
  log(`gate -> ${JSON.stringify(g.result)}; calling interrupt`);
  await call("interrupt");
  times.interruptDone = Date.now();
}

// `tone`: no core and nothing audible. The window plays a tone at about -70 dB (inaudible on any
// speakers) and its own reports are timed: first frame sent to "started", and "tts_stop" to "ended".
async function tone() {
  const reports = [];
  const mine = (m, p) => { reports.push({ at: Date.now(), m, p }); return Promise.resolve({ ok: true }); };
  aec = createAec({ call: mine, log });
  aec.start();
  for (let i = 0; i < 100 && !aec.ready(); i++) await wait(100);
  log(`window ready=${aec.ready()} at +${rel()} ms`);
  const { encode } = { encode: (a) => Buffer.from(Int16Array.from(a).buffer).toString("base64") };
  const frame = (n, rate) => Array.from({ length: Math.round(rate * 0.025) }, (_, i) => Math.round(8 * Math.sin((2 * Math.PI * 440 * (n * 600 + i)) / rate)));
  const send = (id, n, last) => aec.handleEvent({ event: "tts_pcm", id, rate: 24000, seq: n, pcm_b64: encode(last ? [] : frame(n, 24000)), last, prebuffer_ms: 80 });
  for (const [id, frames, stopAfter] of [[1, 40, 0], [2, 120, 500]]) {
    const first = Date.now();
    let fourth = 0;
    for (let n = 0; n < frames; n++) { send(id, n, false); if (n === 3) fourth = Date.now(); await wait(8); } // faster than real time, as synthesis is
    if (!stopAfter) send(id, frames, true);
    if (stopAfter) {
      await wait(Math.max(0, stopAfter - (Date.now() - first)));
      const cutAt = Date.now();
      aec.handleEvent({ event: "tts_stop", id });
      for (let i = 0; i < 50 && !reports.some((r) => r.m === "tts_state" && r.p.id === id && r.p.state === "ended"); i++) await wait(5);
      const e = reports.find((r) => r.m === "tts_state" && r.p.id === id && r.p.state === "ended");
      log(`tone ${id}: tts_stop -> ended ${e ? `${e.at - cutAt} ms (cut=${e.p.cut})` : "never"}`);
    } else {
      for (let i = 0; i < 100 && !reports.some((r) => r.m === "tts_state" && r.p.id === id && r.p.state === "ended"); i++) await wait(50);
      const st = reports.find((r) => r.m === "tts_state" && r.p.id === id && r.p.state === "started");
      const en = reports.find((r) => r.m === "tts_state" && r.p.id === id && r.p.state === "ended");
      log(`tone ${id}: prebuffer filled (4th frame) -> started ${st ? st.at - fourth : "never"} ms, ended ${en ? en.at - first : "never"} ms (audio is 1000 ms)`);
    }
  }
  aec.shutdown();
  setTimeout(() => app.exit(0), 200);
}

async function main() {
  if (mode === "tone") return tone();
  const env = { ...process.env, CLICKER_NO_HOTKEYS: "1", CLICKER_STT: "deepgram" };
  delete env.ELECTRON_RUN_AS_NODE;
  core = spawn(process.env.POINTER_CORE, ["--ipc"], { env, stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
  readline.createInterface({ input: core.stdout }).on("line", (line) => {
    let m;
    try { m = JSON.parse(line); } catch { return; }
    if (m.id !== undefined) { const r = pending.get(m.id); pending.delete(m.id); if (r) r(m); return; }
    onEvent(m);
  });
  readline.createInterface({ input: core.stderr }).on("line", (l) => process.env.AEC_TRACE && log(`core stderr: ${l.slice(0, 200)}`));
  await wait(1500);
  if (mode === "window") {
    aec = createAec({ call: callQuiet, log });
    aec.start();
    for (let i = 0; i < 100 && !aec.ready(); i++) await wait(100);
    log(`window ready=${aec.ready()} at +${rel()} ms`);
    if (!aec.ready()) return finish(1);
    await wait(200);
    log(`core state aec=${(await call("state")).result.aec}`);
  }
  await call("set_mode", { act: false, voice: true, hide: false });
  const opened = Date.now();
  const r = await call("listen_start", { conv: true, purpose: "talk" });
  log(`listen_start ok=${r.ok} in ${Date.now() - opened} ms ${r.error || ""}`);
  await wait(1500);
  const LONG = "The sky looks blue because sunlight is made of many colours, and the air scatters the short blue wavelengths far more than the long red ones. That scattered blue light reaches your eyes from every direction, so the whole sky glows. At sunrise and sunset the light travels through much more air, so most of the blue is scattered away before it gets to you. What is left is the orange and red you see near the horizon. The same effect is why distant mountains look hazy and blue. It is also why the sky on a clear day is a deeper blue straight overhead than it is near the horizon. Violet is scattered even more than blue, but the sun gives off less of it and our eyes are less sensitive to it.";
  const chat = await call("speak", { text: LONG });
  log(`speak -> ${JSON.stringify(chat)}`);
  for (let i = 0; i < 300 && !times.start; i++) await wait(100);
  if (!times.start) { log("Pointer never spoke"); return finish(1); }
  if (mode === "core") {
    await wait(1500);
    await call("stop_all");
    await wait(800);
    return finish(0);
  }
  await wait(6000); // Pointer speaks alone: anything the mic hears now is its own voice
  log(`self-triggers while only Pointer spoke: ${selfTriggers} (max words in one transcript ${maxWords}), speaking=${speaking}`);
  if (!speaking) log("Pointer had already finished; the barge-in below is not valid");
  clipAt = Date.now();
  log(`playing the clip aloud +${rel()} ms`);
  spawn(process.env.POINTER_VOICE_TOOL, ["play", process.env.POINTER_BARGE_CLIP], { stdio: "ignore" });
  for (let i = 0; i < 100 && !times.end; i++) await wait(100);
  const d = (t) => (t ? `${t - clipAt} ms` : "never");
  log(`RESULT clip->words(${clipWords}) ${d(times.heard)}, ->interrupt called ${d(times.interruptAt)}, ->window told to stop ${d(times.ttsStopEvent)}, ->sound ended ${d(times.end)}`);
  if (times.interruptAt && times.end) log(`interrupt call -> sound ended ${times.end - times.interruptAt} ms; interrupt call -> tts_stop event ${times.ttsStopEvent - times.interruptAt} ms`);
  await wait(3000); // let the clip finish so nothing it says becomes a request
  finish(times.end ? 0 : 1);
}

async function finish(code) {
  const stopped = await call("listen_stop");
  if (stats) log(`stt_stats source=${stats.source} mic_lag_ms=${stats.mic_lag_ms} max=${stats.mic_lag_max_ms} peak_rms=${Math.round(stats.peak_rms)} seconds=${stats.seconds}`);
  await call("stop_all");
  if (aec) aec.shutdown();
  core.kill();
  setTimeout(() => app.exit(code), 300);
}

app.whenReady().then(main);
