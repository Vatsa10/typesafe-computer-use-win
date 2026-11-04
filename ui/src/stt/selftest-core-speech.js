// node src/stt/selftest-core-speech.js: the core-recorded engine and the router, with a fake core
// and a fake browser bridge. No microphone, no network, no Electron.

const { createCoreSpeech, createSpeechRouter } = require("./core-speech");
const { createConversation } = require("../assistant/conversation");

let failed = 0;
const check = (name, ok, extra = "") => {
  if (!ok) failed += 1;
  console.log(`[core-speech-selftest] ${ok ? "PASS" : "FAIL"} ${name}${extra ? ` (${extra})` : ""}`);
};

function fakeCore({ engine = "deepgram", failStart = false } = {}) {
  const calls = [];
  const state = { engine, open: false };
  const call = async (method, params) => {
    calls.push({ method, params });
    if (method === "state") return { ok: true, result: { stt: state.engine } };
    if (method === "listen_start") {
      if (failStart) return { ok: false, error: "deepgram unreachable" };
      if (state.open) return { ok: true, result: { listening: false } };
      state.open = true;
      return { ok: true, result: { listening: true } };
    }
    if (method === "listen_stop") {
      state.open = false;
      return { ok: true, result: { heard: "open calculator" } };
    }
    return { ok: true, result: {} };
  };
  return { call, calls, state };
}

function fakeChrome() {
  const c = { starts: [], sink: null, stops: 0, cancels: 0 };
  return Object.assign(c, {
    start: async (o) => (c.starts.push(o), true),
    stop: async () => (c.stops++, "browser words"),
    cancel: () => c.cancels++,
    setSink: (s) => (c.sink = s),
    setLang() {},
    available: () => true,
    shutdown() {},
    lastError: () => "",
    listening: () => false,
    browser: () => "chrome.exe",
    pid: () => 1,
  });
}

async function run() {
  // transcripts reach the sink like the browser bridge's
  {
    const core = fakeCore();
    const interim = [];
    const segments = [];
    const bar = [];
    const cs = createCoreSpeech({ call: core.call, onInterim: (t) => bar.push(t) });
    cs.setSink({ interim: (t) => interim.push(t), segment: (t) => segments.push(t) });
    await cs.start({ conv: true, purpose: "talk" });
    cs.onTranscript({ text: "open the", final: false, purpose: "talk" });
    cs.onTranscript({ text: "open the calculator", final: true, purpose: "talk" });
    check("conversation interim reaches the sink", interim.join("|") === "open the");
    check("a final is a segment in a conversation", segments.join("|") === "open the calculator");
    check("start passes conv and purpose to the core", JSON.stringify(core.calls[0].params) === JSON.stringify({ conv: true, purpose: "talk" }));
    check("stop returns what the core heard", (await cs.stop()) === "open calculator");
  }
  // one-shot: a final is just the latest words (the bar), never a segment
  {
    const core = fakeCore();
    const bar = [];
    const cs = createCoreSpeech({ call: core.call, onInterim: (t) => bar.push(t) });
    await cs.start({ purpose: "dictate" });
    cs.onTranscript({ text: "hello", final: false });
    cs.onTranscript({ text: "hello there.", final: true });
    check("one-shot words go to the bar", bar.join("|") === "hello|hello there.");
    cs.onTranscript({ text: "", final: false });
    check("empty transcripts are ignored", bar.length === 2);
    cs.cancel();
    cs.onTranscript({ text: "late", final: false });
    check("nothing after a cancel", bar.length === 2);
  }
  // a session left open by an earlier user is ended and replaced
  {
    const core = fakeCore();
    core.state.open = true;
    const cs = createCoreSpeech({ call: core.call });
    await cs.start({});
    check("an open session is replaced", core.calls.map((c) => c.method).join(",") === "listen_start,listen_stop,listen_start");
  }
  // routing
  {
    const core = fakeCore();
    const chrome = fakeChrome();
    const cs = createCoreSpeech({ call: core.call });
    const router = createSpeechRouter({ chrome, core: cs, engineName: async () => core.state.engine });
    await router.start({ conv: true });
    check("deepgram engine uses the core", router.activeName() === "deepgram" && chrome.starts.length === 0);
    // a dropped socket moves the open conversation to the browser, once
    core.state.engine = "chrome";
    await router.onFallback();
    check("fallback restarts the conversation on chrome", router.activeName() === "chrome" && chrome.starts.length === 1 && chrome.starts[0].conv === true);
    await router.onFallback();
    check("a second fallback event does nothing", chrome.starts.length === 1);
    check("stop now asks the browser", (await router.stop()) === "browser words");
  }
  {
    const core = fakeCore({ failStart: true });
    const chrome = fakeChrome();
    const router = createSpeechRouter({ chrome, core: createCoreSpeech({ call: core.call }), engineName: async () => "deepgram" });
    await router.start({ conv: false });
    check("a start the core refuses continues on chrome", router.activeName() === "chrome" && chrome.starts.length === 1);
  }
  {
    const core = fakeCore({ engine: "chrome" });
    const chrome = fakeChrome();
    const router = createSpeechRouter({ chrome, core: createCoreSpeech({ call: core.call }), engineName: async () => "chrome" });
    await router.start({});
    check("the chrome engine never touches the core", core.calls.length === 0);
  }
  // the conversation follows the audio device's own edges
  {
    let t = 1000;
    const conv = createConversation({ call: async () => ({ ok: true, result: {} }), speech: { setSink() {}, async start() {}, cancel() {} }, now: () => t, schedule: () => 1, unschedule() {} });
    conv.onCoreEvent({ event: "speech", action: "start", text: "I opened the calculator for you" });
    conv.onCoreEvent({ event: "speech", action: "start", real: true, engine: "murf" });
    t += 3000;
    conv.onCoreEvent({ event: "speech", action: "end", real: true, engine: "murf", cut: false });
    await conv.begin();
    const sent = [];
    const c2 = createConversation({ call: async (m, p) => (sent.push(m), { ok: true, result: { interrupt: false } }), speech: { setSink() {}, async start() {}, cancel() {} }, now: () => t, schedule: () => 1, unschedule() {} });
    c2.onCoreEvent({ event: "speech", action: "start", text: "I opened the calculator for you" });
    c2.onCoreEvent({ event: "speech", action: "start", real: true });
    t += 1000;
    c2.onCoreEvent({ event: "speech", action: "end", real: true });
    t += 1000; // 1 s after the true end: inside the echo window
    await c2.onSegment("opened the calculator for you");
    check("its own words 1 s after the real end are dropped as echo", !sent.includes("chat"));
    t += 2000; // 3 s after: outside it
    await c2.onSegment("open notepad please");
    check("a new request after the window is sent", sent.includes("chat"));
  }
  console.log(failed ? `[core-speech-selftest] ${failed} FAILED` : "[core-speech-selftest] all passed");
  process.exit(failed ? 1 : 0);
}

run();
