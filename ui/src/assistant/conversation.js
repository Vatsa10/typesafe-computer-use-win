// The voice conversation: what the shell does with speech after the first request, so talking to
// Pointer feels like talking and not like pressing a button each time.
//
//   - After a voice request the mic stays open. Ten seconds after the reply has finished it rests.
//   - While Pointer is busy, the user's speech is not blindly a new request. A live fragment of two
//     words or more stops Pointer's voice (`interrupt`). The finished sentence goes to `gate`, which
//     decides whether it should cancel the work (`chat`) or be shown dimmed as "Ignored" (click to send).
//   - Pointer's own voice, heard back through the mic, is recognised by comparing it with what Pointer
//     said (the `speech` events) for as long as it speaks and 1.5 s after. See voice-rules.js. The core
//     also reports when the audio really started and ended (`speech` with `real: true`); the echo
//     window follows those edges, so it is exact rather than estimated from the text.
//   - A continuation said within 2.5 s of a request, before any reply has started, is merged into it.
//
// Everything outside is injected (the core call, the speech bridge, the clock), so a self-test can
// drive it with fake events and a fake clock. Transcripts are never logged.

const rules = require("./voice-rules");

const REST_MS = 10000;
const ECHO_MS = 1500;
const MERGE_MS = 2500;
const MAX_ITEMS = 40;

function createConversation({ call, speech, notify = () => {}, log = () => {}, now = () => Date.now(), schedule = setTimeout, unschedule = clearTimeout }) {
  let listening = false;
  let starting = null;
  let coreState = "idle";
  let detail = "";
  const spoken = { text: "", active: false, endedAt: 0, seq: 0 };
  let interruptedSeq = -1;
  let lastChat = null; // { text, at, source, replyStarted, itemId }
  let restTimer = null;
  const items = []; // { id, role: "you" | "pointer" | "ignored" | "note", text }
  let nextItem = 1;

  function addItem(role, text) {
    const item = { id: nextItem++, role, text };
    items.push(item);
    if (items.length > MAX_ITEMS) items.shift();
    notify();
    return item;
  }

  const speechRecent = () => spoken.active || (spoken.endedAt > 0 && now() - spoken.endedAt <= ECHO_MS);
  const speakingNow = () => spoken.active || coreState === "speaking";

  function cancelRest() {
    if (restTimer) unschedule(restTimer);
    restTimer = null;
  }

  // Rest only when nothing is happening: not thinking, not working, not speaking.
  function idleCheck() {
    cancelRest();
    if (!listening) return;
    if (coreState === "idle" && !spoken.active) restTimer = schedule(() => end("rest"), REST_MS);
  }

  async function begin() {
    if (listening) {
      idleCheck();
      return { ok: true };
    }
    if (starting) return starting;
    starting = (async () => {
      try {
        speech.setSink({ interim: onInterim, segment: onSegment });
        await speech.start({ conv: true });
        listening = true;
        idleCheck();
        notify();
        return { ok: true };
      } catch (e) {
        speech.setSink(null);
        log(`conversation: could not listen (${e.message})`);
        notify();
        return { ok: false, error: e.message };
      } finally {
        starting = null;
      }
    })();
    return starting;
  }

  function end(reason) {
    cancelRest();
    if (!listening) return;
    listening = false;
    speech.setSink(null);
    speech.cancel();
    log(`conversation: resting (${reason})`);
    notify();
  }

  async function send(text, source, { merge = true } = {}) {
    let out = text;
    const t = now();
    const mergeable = merge && source === "voice" && lastChat && lastChat.source === "voice" && !lastChat.replyStarted && t - lastChat.at < MERGE_MS;
    let itemId;
    if (mergeable) {
      out = `${lastChat.text} ${text}`;
      itemId = lastChat.itemId;
      const item = items.find((i) => i.id === itemId);
      if (item) item.text = out;
      notify();
    } else {
      itemId = addItem("you", text).id;
    }
    lastChat = { text: out, at: t, source, replyStarted: false, itemId };
    const reply = await call("chat", { text: out, source });
    if (!reply.ok) addItem("note", `Could not reach the assistant: ${reply.error}`);
    return reply;
  }

  async function onSegment(raw) {
    cancelRest();
    const text = rules.clean(raw);
    if (!text) return idleCheck();
    if (speechRecent() && rules.isEcho(text, spoken.text)) {
      log("conversation: dropped a line that was Pointer's own voice");
      return idleCheck();
    }
    const speaking = speakingNow();
    if (speaking && rules.isBackchannel(text)) return idleCheck();

    const t = now();
    if (lastChat && lastChat.source === "voice" && !lastChat.replyStarted && t - lastChat.at < MERGE_MS) {
      await send(text, "voice");
      return;
    }
    const busy = speaking || coreState === "thinking" || coreState === "working";
    if (!busy) {
      await send(text, "voice");
      return;
    }
    const verdict = await call("gate", { heard: text, speaking });
    if (verdict.ok && verdict.result && verdict.result.interrupt) {
      await send(text, "voice");
      return;
    }
    if (!verdict.ok && !speaking) {
      // No judge available: when Pointer is only thinking or working, a spoken line is a request.
      await send(text, "voice");
      return;
    }
    addItem("ignored", text);
    idleCheck();
  }

  function onInterim(raw) {
    cancelRest();
    if (!speakingNow()) return;
    const text = rules.clean(raw);
    if (rules.countWords(text) < 2) return;
    if (speechRecent() && rules.isEcho(text, spoken.text)) return;
    if (rules.isBackchannel(text)) return;
    if (interruptedSeq === spoken.seq) return;
    interruptedSeq = spoken.seq;
    call("interrupt");
  }

  function onCoreEvent(m) {
    if (m.event === "assistant") {
      coreState = String(m.state || "idle");
      detail = m.detail ? String(m.detail) : "";
      if (coreState === "speaking" && lastChat) lastChat.replyStarted = true;
      idleCheck();
      notify();
    } else if (m.event === "reply") {
      if (m.text) addItem("pointer", String(m.text));
      if (lastChat) lastChat.replyStarted = true;
    } else if (m.event === "speech" && m.real && m.ack) {
      // An acknowledgement clip ("hmm", "okay"): the lowest-priority sound. It joins the echo
      // history so the clip heard back through the mic is not taken for a request, but it is not a
      // reply: it does not mark the request as answered and does not count as Pointer speaking.
      if (m.action === "start") {
        spoken.text = `${spoken.text} ${String(m.text || "")}`.trim();
        spoken.endedAt = now();
      } else if (m.action === "end") {
        spoken.endedAt = now();
      }
    } else if (m.event === "speech" && m.real) {
      // The audio device's own word on when sound began and ended (no text, no new line). It only
      // moves the echo window to the true edges; the line itself was announced by the plain
      // `speech` events that bracket the voice call.
      if (m.action === "start") {
        spoken.active = true;
        spoken.endedAt = 0;
        cancelRest();
      } else if (m.action === "end") {
        spoken.active = false;
        spoken.endedAt = now();
        idleCheck();
      }
      notify();
    } else if (m.event === "speech") {
      if (m.action === "start") {
        spoken.text = String(m.text || "");
        spoken.active = true;
        spoken.endedAt = 0;
        spoken.seq += 1;
        if (lastChat) lastChat.replyStarted = true;
        cancelRest();
      } else if (m.action === "end") {
        if (m.text) spoken.text = String(m.text);
        spoken.active = false;
        spoken.endedAt = now();
        idleCheck();
      }
      notify();
    }
  }

  async function sendItem(id) {
    const item = items.find((i) => i.id === id);
    if (!item || item.role !== "ignored") return { ok: false, error: "nothing to send" };
    item.role = "you";
    notify();
    lastChat = null;
    return send(item.text, "voice", { merge: false });
  }

  return {
    begin,
    end,
    send,
    sendItem,
    onSegment,
    onInterim,
    onCoreEvent,
    note: (text) => addItem("note", String(text)),
    seedForSelfTest: (role, text) => addItem(role, text),
    isListening: () => listening,
    snapshot: () => ({ listening, coreState, detail, items: items.slice(-10) }),
    _lastChat: () => lastChat,
  };
}

module.exports = { createConversation, REST_MS, ECHO_MS, MERGE_MS };
