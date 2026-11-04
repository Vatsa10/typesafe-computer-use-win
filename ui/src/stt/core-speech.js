// The core-recorded speech engine (CLICKER_STT=deepgram, the default when DEEPGRAM_API_KEY is set).
//
// The core records the microphone and streams it to Deepgram; the API key never leaves the core.
// This module is the shell's half: it asks the core to start and stop (`listen_start` /
// `listen_stop`) and turns the core's `transcript { text, final, purpose }` events into the same
// calls the Chrome bridge makes on its sink, so the conversation, the bar, the echo filter, the
// barge-in and the rest behaviour work unchanged on either engine.
//
// The router at the bottom picks the engine at each start and moves a live conversation over to
// the Chrome bridge if the core reports that Deepgram dropped (`stt_fallback`). Transcripts are
// never logged here.

function createCoreSpeech({ call, onInterim = () => {} }) {
  let sink = null; // { interim, segment }, set while a conversation owns the microphone
  let listening = false;
  let conv = false;
  let lastError = "";

  async function start({ conv: wantConv = false, purpose = "talk" } = {}) {
    lastError = "";
    let reply = await call("listen_start", { conv: wantConv, purpose });
    if (reply.ok && reply.result && reply.result.listening === false) {
      // A session from an earlier user of the microphone is still open: end it, then start ours.
      await call("listen_stop");
      reply = await call("listen_start", { conv: wantConv, purpose });
    }
    if (!reply.ok) {
      lastError = String(reply.error || "could not start");
      throw new Error(lastError);
    }
    listening = true;
    conv = wantConv;
    return true;
  }

  /** Stop and return everything the core heard in this session ("" for silence). */
  async function stop() {
    if (!listening) return "";
    listening = false;
    const reply = await call("listen_stop");
    if (!reply.ok) {
      lastError = String(reply.error || "");
      return "";
    }
    return String((reply.result && reply.result.heard) || "").trim();
  }

  /** Drop the session: the answer is ignored. */
  function cancel() {
    if (!listening) return;
    listening = false;
    call("listen_stop");
  }

  /** A `transcript` event from the core. */
  function onTranscript(message) {
    if (!listening || typeof message.text !== "string" || !message.text) return;
    if (message.final && conv && sink && sink.segment) return sink.segment(message.text);
    // Live words: a finished sentence on a one-shot session is still the latest words.
    if (sink && sink.interim) sink.interim(message.text);
    else onInterim(message.text);
  }

  return {
    start,
    stop,
    cancel,
    onTranscript,
    setSink: (next) => (sink = next),
    setLang: () => {},
    available: () => true,
    shutdown: cancel,
    lastError: () => lastError,
    isListening: () => listening,
    wasConv: () => conv,
    markEnded: () => (listening = false), // the core ended the session by itself (a dropped socket)
  };
}

/** Both engines behind the one interface the conversation and the bar already use. */
function createSpeechRouter({ chrome, core, engineName, log = () => {} }) {
  let active = chrome;
  let sink = null;
  let lastStart = { conv: false, purpose: "talk" };

  async function start(opts = {}) {
    lastStart = { conv: Boolean(opts.conv), purpose: opts.purpose || "talk" };
    const name = await engineName();
    if (name === "deepgram") {
      try {
        active = core;
        core.setSink(sink);
        return await core.start(lastStart);
      } catch (e) {
        // The core has already said why; this start continues on the browser recognizer.
        log(`speech: deepgram did not start (${e.message}); using the browser recognizer`);
      }
    }
    active = chrome;
    chrome.setSink(sink);
    return chrome.start({ conv: lastStart.conv });
  }

  /** The core says Deepgram dropped. A session that was open carries on in the browser. */
  async function onFallback() {
    if (active !== core || !core.isListening()) return;
    const wasOpen = lastStart;
    core.markEnded();
    active = chrome;
    chrome.setSink(sink);
    try {
      await chrome.start({ conv: wasOpen.conv });
    } catch (e) {
      log(`speech: the browser recognizer could not take over (${e.message})`);
    }
  }

  return {
    start,
    onFallback,
    stop: () => active.stop(),
    cancel: () => active.cancel(),
    setSink: (next) => {
      sink = next;
      active.setSink(next);
    },
    setLang: (lang) => chrome.setLang(lang),
    available: () => chrome.available() || core.available(),
    shutdown: () => {
      core.shutdown();
      chrome.shutdown();
    },
    lastError: () => active.lastError(),
    listening: () => (active === core ? core.isListening() : chrome.listening()),
    browser: () => chrome.browser(),
    pid: () => chrome.pid(),
    activeName: () => (active === core ? "deepgram" : "chrome"),
  };
}

module.exports = { createCoreSpeech, createSpeechRouter };
