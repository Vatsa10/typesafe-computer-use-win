// POINTER_CONV_SELFTEST=1: drives the conversation controller with fake core events, a fake speech
// bridge and a fake clock. No microphone, no core, no network. Prints PASS or FAIL per rule.

const { createConversation } = require("./conversation");
const rules = require("./voice-rules");

function rig() {
  let t = 100000;
  const timers = [];
  const calls = [];
  const speech = {
    sink: null,
    cancels: 0,
    setSink(s) { this.sink = s; },
    async start() {},
    cancel() { this.cancels += 1; },
  };
  const gate = { interrupt: false, ok: true };
  const call = async (method, params) => {
    calls.push({ method, params });
    if (method === "gate") return gate.ok ? { ok: true, result: { interrupt: gate.interrupt, confidence: 0.9 } } : { ok: false, error: "unknown method" };
    return { ok: true, result: {} };
  };
  const conv = createConversation({
    call, speech,
    now: () => t,
    schedule: (fn, ms) => { const timer = { at: t + ms, fn }; timers.push(timer); return timer; },
    unschedule: (timer) => { const i = timers.indexOf(timer); if (i >= 0) timers.splice(i, 1); },
  });
  const advance = (ms) => {
    t += ms;
    for (const timer of timers.filter((x) => x.at <= t)) { timers.splice(timers.indexOf(timer), 1); timer.fn(); }
  };
  const chats = () => calls.filter((c) => c.method === "chat").map((c) => c.params.text);
  const count = (m) => calls.filter((c) => c.method === m).length;
  return { conv, speech, calls, gate, advance, chats, count };
}

async function run() {
  const results = [];
  const check = (name, ok, extra = "") => {
    results.push(ok);
    console.log(`[conv-selftest] ${ok ? "PASS" : "FAIL"} ${name}${extra ? ` (${extra})` : ""}`);
  };

  // rules
  check("fillers dropped", rules.clean("um open uh the calendar") === "open the calendar");
  check("lone okay is a backchannel", rules.isBackchannel("Okay.") && !rules.isBackchannel("okay open it"));
  check("echo: own words", rules.isEcho("opened the settings page", "I opened the settings page for you"));
  check("echo: no/not/instead protect", !rules.isEcho("no not the settings page", "I opened the settings page for you") && !rules.isEcho("instead open the page", "I opened the settings page"));

  let r = rig();
  await r.conv.begin();
  check("begin opens the speech session", r.speech.sink !== null);

  // idle request, fillers dropped
  await r.conv.onSegment("um open uh the calendar");
  check("filler-stripped request goes to chat as voice", r.chats().join("|") === "open the calendar" && r.calls[0].params.source === "voice");

  // echo of its own voice
  r = rig();
  await r.conv.begin();
  r.conv.onCoreEvent({ event: "speech", action: "start", text: "I opened the settings page for you" });
  r.conv.onCoreEvent({ event: "assistant", state: "speaking" });
  await r.conv.onSegment("opened the settings page");
  check("own voice while speaking is dropped (no chat, no gate)", r.chats().length === 0 && r.count("gate") === 0);
  r.conv.onCoreEvent({ event: "speech", action: "end", text: "I opened the settings page for you" });
  r.conv.onCoreEvent({ event: "assistant", state: "idle" });
  r.advance(1000);
  await r.conv.onSegment("settings page for you");
  check("own voice 1.0 s after it ended is still dropped", r.chats().length === 0);
  r.advance(1000); // 2.0 s after
  await r.conv.onSegment("settings page for you");
  check("the same words 2.0 s after it ended are a request", r.chats().length === 1);

  // lone okay while speaking
  r = rig();
  await r.conv.begin();
  r.conv.onCoreEvent({ event: "speech", action: "start", text: "Here is the plan" });
  r.conv.onCoreEvent({ event: "assistant", state: "speaking" });
  await r.conv.onSegment("okay");
  check("a lone okay while speaking is not a request", r.chats().length === 0 && r.count("gate") === 0);
  r.conv.onInterim("okay okay");
  check("okay okay does not interrupt", r.count("interrupt") === 0);

  // interim interrupt
  r.conv.onInterim("wait");
  check("one-word interim does not interrupt", r.count("interrupt") === 0);
  r.conv.onInterim("no instead");
  r.conv.onInterim("no instead open the mail");
  check("two-word interim interrupts exactly once per speech", r.count("interrupt") === 1);

  // gate decides
  r.gate.interrupt = true;
  await r.conv.onSegment("no, do the other one instead");
  check("busy + gate says interrupt -> chat", r.chats().length === 1 && r.chats()[0].includes("instead"));

  r = rig();
  await r.conv.begin();
  r.conv.onCoreEvent({ event: "speech", action: "start", text: "Here is the plan" });
  r.conv.onCoreEvent({ event: "assistant", state: "speaking" });
  await r.conv.onSegment("what time is it in Tokyo");
  const snap = r.conv.snapshot();
  const ignored = snap.items.find((i) => i.role === "ignored");
  check("gate says no -> shown as Ignored, not sent", Boolean(ignored) && r.chats().length === 0);
  await r.conv.sendItem(ignored.id);
  check("clicking Ignored sends it", r.chats().join("|") === "what time is it in Tokyo");

  // gate unavailable
  r = rig();
  r.gate.ok = false;
  await r.conv.begin();
  r.conv.onCoreEvent({ event: "assistant", state: "working" });
  await r.conv.onSegment("also check the weather");
  check("no gate and only working -> treated as a request", r.chats().length === 1);

  // merge
  r = rig();
  await r.conv.begin();
  await r.conv.onSegment("open the");
  r.advance(1500);
  await r.conv.onSegment("calendar please");
  check("continuation within 2.5 s merges", r.chats().join("|") === "open the|open the calendar please");
  check("merge replaces the conversation line, not adds one", r.conv.snapshot().items.filter((i) => i.role === "you").length === 1);

  r = rig();
  await r.conv.begin();
  await r.conv.onSegment("open the calendar");
  r.conv.onCoreEvent({ event: "reply", text: "Opening it" });
  r.advance(1000);
  await r.conv.onSegment("and the mail");
  check("no merge once a reply has started", r.chats()[1] === "and the mail");

  r = rig();
  await r.conv.begin();
  await r.conv.onSegment("open the");
  r.advance(3000);
  await r.conv.onSegment("calendar");
  check("no merge after 2.5 s", r.chats()[1] === "calendar");

  // rest
  r = rig();
  await r.conv.begin();
  await r.conv.onSegment("hello there");
  r.conv.onCoreEvent({ event: "assistant", state: "speaking" });
  r.conv.onCoreEvent({ event: "reply", text: "Hi" });
  r.advance(20000);
  check("never rests while speaking", r.conv.isListening());
  r.conv.onCoreEvent({ event: "assistant", state: "idle" });
  r.advance(9000);
  check("still listening 9 s after the reply", r.conv.isListening());
  r.advance(1500);
  check("rests 10 s after the reply", !r.conv.isListening() && r.speech.cancels === 1);

  const failed = results.filter((ok) => !ok).length;
  console.log(`[conv-selftest] ${results.length - failed}/${results.length} passed`);
  return failed === 0;
}

module.exports = { run };
