// The Settings view. Rows come from the core's `settings` call and are grouped here by key. A
// secret's value never reaches this page (the core sends it empty and says only whether one is
// set), and nothing here ever logs or echoes what is typed into a secret field.
import { call } from "../lib/core.js";
import { h, emptyState } from "../lib/dom.js";
import { icon } from "../lib/icons.js";
import { toast } from "./toast.js";
import { createHotkeysSection } from "./hotkeys.js";

const GROUPS = [
  { id: "keys", title: "API keys", match: (row) => row.secret },
  { id: "hotkeys", title: "Hotkeys", match: (row) => row.key.startsWith("CLICKER_HOTKEY_") },
  { id: "voice", title: "Voice", match: (row) => /SPEAK|VOICE|BAR_SILENCE|TRANSCRIBE/.test(row.key) },
  { id: "browser", title: "Browser and sites", match: (row) => /BROWSER|EMAIL|CATALOG|OCR/.test(row.key) },
  { id: "models", title: "Models", match: (row) => /MODEL|PROVIDER/.test(row.key) },
  { id: "other", title: "Other", match: () => true },
];

// The assistant layer (docs/superpowers/specs/2026-11-04-assistant-layer-design.md). These rows are
// shown as one section of their own, whether or not this core lists them yet: a row the core does not
// know is only sent on Save if the user changed it.
const ASSIST = [
  { key: "POINTER_MODE", label: "Mode" },
  { key: "POINTER_LLM", label: "Assistant model" },
  { key: "POINTER_AGENT_MODEL", label: "Agent model", fallback: "gpt-6-luna or claude-haiku-5-5", help: "Overrides the default model of the chosen provider." },
  { key: "POINTER_AGENT_BASE_URL", label: "Agent base URL", fallback: "https://api.openai.com/v1", help: "OpenAI provider only: any OpenAI-compatible server, such as Groq, OpenRouter or one on this machine." },
  { key: "POINTER_AGENT_API_KEY", label: "Agent API key", secret: true, help: "Falls back to OPENAI_API_KEY. With no key at all, Pointer stays in Quick mode." },
];
const ASSIST_KEYS = new Set(ASSIST.map((a) => a.key));
const MODE_COST = {
  assistant: "Assistant: a parent agent talks with you and runs tasks, so each turn costs model tokens.",
  quick: "Quick: Pointer routes with TypeSafe only, about $0.0002 a step.",
};
// Prices are per million tokens. Luna's come from third-party sources, so they are "about".
const LLM_CHOICES = [
  { value: "openai", text: "OpenAI GPT-6 Luna", key: "OPENAI_API_KEY", keyName: "OpenAI API key", price: "about $0.10 in and $0.50 out per 1M tokens" },
  { value: "claude", text: "Claude Haiku 5.5", key: "ANTHROPIC_API_KEY", keyName: "Anthropic API key", price: "$0.10 in and $0.50 out per 1M tokens" },
];
const WAKE_HELP =
  "Say Hey Pointer to start talking. While this is on, Pointer keeps your microphone open all the time so it can hear the phrase. Audio is analysed on this computer and is never sent anywhere or saved. Turning it on downloads a 17 MB model once.";

const GROUP_NOTES = {
  keys: "Stored in .env. Leave a key empty to keep the one already saved.",
  hotkeys: "Hotkey changes take effect after Pointer restarts.",
};

// "My answers": what only you can say on a form (salary, visa, gender and the like). Kept in
// my-answers.json on this computer; Pointer uses them to fill forms without asking, and never
// guesses them from a resume. An empty field is left empty on the form. Saved on its own button.
function createAnswersSection() {
  const fields = new Map();
  const status = h("span", { class: "helper", "aria-live": "polite" });
  const saveBtn = h("button", { class: "btn", type: "button", text: "Save my answers" });
  const card = h("div", { class: "card" });
  const element = h(
    "fieldset",
    { class: "group", id: "my-answers" },
    h("legend", { text: "My answers" }),
    h("p", { class: "helper group-note", text: "Used to fill forms for you without asking. Stored only on this computer. Leave one empty and Pointer leaves it blank on a form, or asks once if the form requires it." }),
    card,
    h("div", { class: "settings-foot" }, saveBtn, status),
  );

  function paint(list) {
    fields.clear();
    card.replaceChildren();
    for (const f of list) {
      const id = "answer-" + f.key;
      const input = h("input", { class: "field", id, type: "text", autocomplete: "off", spellcheck: "false" });
      input.value = f.value || "";
      fields.set(f.key, input);
      card.append(h("div", { class: "setting" }, h("label", { for: id, text: f.label }), input));
    }
  }

  async function load() {
    const result = await call("get_answers");
    if (result && Array.isArray(result.fields)) paint(result.fields);
  }

  saveBtn.addEventListener("click", async () => {
    const values = {};
    for (const [key, input] of fields) values[key] = input.value.trim();
    saveBtn.disabled = true;
    const result = await call("set_answers", { values });
    saveBtn.disabled = false;
    if (result && Array.isArray(result.fields)) {
      paint(result.fields);
      status.textContent = "Saved.";
      toast({ kind: "success", title: "My answers saved", body: "Pointer will use them on forms." });
    }
  });

  return { element, load };
}

export function createSettingsView() {
  const answers = createAnswersSection();
  const scroll = h("div", { class: "settings-scroll" });
  const save = h("button", { class: "btn accent", type: "button", html: icon("save") + "<span>Save</span>" });
  const revert = h("button", { class: "btn", type: "button", text: "Discard changes" });
  const status = h("span", { class: "helper", "aria-live": "polite" });
  let rows = [];
  let inputs = new Map();
  let prefs = { wake: false, camera: false, wakeState: { state: "off", detail: "" } };
  let wakeHelp = null;
  let wakeBox = null;
  // With a core that answers `hotkeys`, the recorder replaces the free-text CLICKER_HOTKEY_ rows.
  const hotkeys = createHotkeysSection();
  let recorder = false;

  const element = h(
    "section",
    { class: "view", id: "view-settings", "aria-labelledby": "settings-title" },
    h("div", { class: "view-head" }, h("h1", { id: "settings-title", text: "Settings" })),
    scroll,
    h("div", { class: "settings-foot" }, save, revert, status),
  );

  function dirty() {
    for (const row of rows) {
      const value = inputs.get(row.key).value.trim();
      if (row.secret ? value !== "" : value !== (row.value || "")) return true;
    }
    return false;
  }

  /* ---- the assistant section */
  function modeNow(row) {
    const chosen = (row.value || "").toLowerCase();
    if (chosen === "assistant" || chosen === "quick") return chosen;
    const agentKey = rows.some((r) => ["POINTER_AGENT_API_KEY", "OPENAI_API_KEY"].includes(r.key) && /^set/i.test(r.fallback || ""));
    return agentKey ? "assistant" : "quick";
  }

  function llmNow(row) {
    const chosen = (row.value || "").toLowerCase();
    if (chosen === "openai" || chosen === "claude") return chosen;
    const isSet = (key) => rows.some((r) => r.key === key && /^set/i.test(r.fallback || ""));
    if (isSet("OPENAI_API_KEY") || isSet("POINTER_AGENT_API_KEY")) return "openai";
    return isSet("ANTHROPIC_API_KEY") ? "claude" : "openai";
  }

  function modeRow(row) {
    return choiceRow(row, "mode", "Mode", [
      { value: "assistant", text: "Assistant" },
      { value: "quick", text: "Quick" },
    ], modeNow(row), (v) => MODE_COST[v]);
  }

  function llmRow(row) {
    const keySet = (key) => rows.some((r) => r.key === key && /^set/i.test(r.fallback || ""));
    return choiceRow(row, "llm", "Assistant model", LLM_CHOICES, llmNow(row), (v) => {
      const c = LLM_CHOICES.find((x) => x.value === v);
      const state = keySet(c.key) || (v === "openai" && keySet("POINTER_AGENT_API_KEY")) ? "key is set" : "no key yet: add the " + c.keyName + " above";
      return c.text + ": " + c.price + ". The " + c.keyName + " " + (state === "key is set" ? "is set." : "is missing; " + state + ".") + " Typing, answers and teaching use the same provider. Applies without a restart.";
    });
  }

  function choiceRow(row, id, label, options, initial, describe) {
    const field = { value: row.value || "" }; // what Save will send; blank leaves the core's default alone
    inputs.set(row.key, field);
    const note = h("span", { class: "helper", id: `set-${id}-help` });
    const buttons = options.map((o) => h("button", { type: "button", role: "radio", "data-value": o.value, text: o.text }));
    const group = h("div", { class: "segmented", role: "radiogroup", "aria-label": label, "aria-describedby": `set-${id}-help` }, ...buttons);
    let current = initial;
    function paint() {
      buttons.forEach((b, i) => {
        const on = options[i].value === current;
        b.setAttribute("aria-checked", String(on));
        b.tabIndex = on ? 0 : -1;
      });
      note.textContent = describe(current);
    }
    function choose(next) {
      current = next;
      field.value = next;
      paint();
      syncDirty();
    }
    buttons.forEach((b, i) => b.addEventListener("click", () => choose(options[i].value)));
    group.addEventListener("keydown", (event) => {
      if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(event.key)) return;
      event.preventDefault();
      const at = options.findIndex((o) => o.value === current);
      const step = event.key === "ArrowLeft" || event.key === "ArrowUp" ? -1 : 1;
      const next = (at + step + options.length) % options.length;
      choose(options[next].value);
      buttons[next].focus();
    });
    paint();
    return h("div", { class: "setting" }, h("label", { id: `set-${id}-label`, text: row.label }), note, group);
  }

  function toggleRow(title, help, checked, onChange, id) {
    const box = h("input", { type: "checkbox", id, "aria-describedby": id + "-help" });
    box.checked = checked;
    box.addEventListener("change", () => onChange(box.checked));
    const helper = h("span", { class: "helper", id: id + "-help", text: help });
    return { box, helper, element: h("div", { class: "setting" }, h("label", { for: id, text: title }), helper, h("label", { class: "toggle", for: id }, box)) };
  }

  function paintWake() {
    if (!wakeHelp || !wakeBox) return;
    const st = prefs.wakeState || { state: "off" };
    const extra =
      {
        downloading: " Downloading the on-device model: " + (st.detail || ""),
        starting: " Starting the microphone.",
        on: " On: listening for the phrase.",
        error: " Could not turn on: " + (st.detail || "unknown error"),
      }[st.state] || "";
    wakeHelp.textContent = WAKE_HELP + extra;
    wakeBox.checked = st.state === "error" ? false : prefs.wake;
  }

  function assistantGroup(list) {
    const wake = toggleRow("Wake phrase: Hey Pointer", WAKE_HELP, prefs.wake, async (on) => {
      prefs = (await window.assistant.setPrefs({ wake: on })) || prefs;
      paintWake();
    }, "set-wake");
    wakeHelp = wake.helper;
    wakeBox = wake.box;
    const camera = toggleRow("Camera", "Pointer can look through your camera only when you ask it to. Frames stay in memory for that one question and are never saved. A badge shows while it is open.", prefs.camera, async (on) => {
      prefs = (await window.assistant.setPrefs({ camera: on })) || prefs;
    }, "set-camera");
    paintWake();
    return h(
      "fieldset",
      { class: "group", id: "group-assistant" },
      h("legend", { text: "Assistant" }),
      h("div", { class: "card" }, list.map((row) => (row.key === "POINTER_MODE" ? modeRow(row) : row.key === "POINTER_LLM" ? llmRow(row) : settingRow(row))), wake.element, camera.element),
    );
  }

  function syncDirty() {
    const changed = dirty();
    save.disabled = !changed;
    revert.disabled = !changed;
    status.textContent = changed ? "Unsaved changes" : "";
  }

  function settingRow(row) {
    const id = "set-" + row.key;
    const helpId = id + "-help";
    const input = h("input", {
      class: "field",
      id,
      type: row.secret ? "password" : "text",
      autocomplete: "off",
      spellcheck: "false",
      "aria-describedby": helpId,
    });
    let helper;
    if (row.secret) {
      const isSet = /^set/i.test(row.fallback || "");
      input.placeholder = isSet ? "Set (hidden). Type to replace" : "Not set";
      helper = h(
        "span",
        { class: "helper", id: helpId },
        h("span", { class: "secret-state" + (isSet ? " set" : ""), html: icon(isSet ? "lock" : "key") + `<span>${isSet ? "Set (hidden)" : "Not set"}</span>` }),
        (ASSIST.find((a) => a.key === row.key) || {}).help ? " · " + ASSIST.find((a) => a.key === row.key).help : null,
      );
    } else {
      input.value = row.value || "";
      input.placeholder = row.fallback || "";
      const hint = (ASSIST.find((a) => a.key === row.key) || {}).help;
      helper = h("span", { class: "helper", id: helpId, text: hint ? `${hint} Default: ${row.fallback}` : row.fallback ? `Default: ${row.fallback}` : row.key });
    }
    input.addEventListener("input", syncDirty);
    inputs.set(row.key, input);
    return h("div", { class: "setting" }, h("label", { for: id, text: row.label }), helper, input);
  }

  function render() {
    inputs = new Map();
    scroll.replaceChildren();
    if (!rows.length) {
      scroll.append(emptyState(icon("sliders"), "Settings are unavailable", "The core did not send its settings. Is it running?"));
      scroll.append(assistantGroup([])); // the wake phrase and camera belong to this app, not the core
      lockFoot();
      return;
    }
    const taken = new Set(ASSIST_KEYS);
    scroll.append(assistantGroup(rows.filter((row) => ASSIST_KEYS.has(row.key))));
    for (const group of GROUPS) {
      if (group.id === "hotkeys" && recorder) {
        scroll.append(hotkeys.element);
        continue;
      }
      const members = rows.filter((row) => !taken.has(row.key) && group.match(row));
      if (!members.length) continue;
      for (const row of members) taken.add(row.key);
      const fieldset = h(
        "fieldset",
        { class: "group" },
        h("legend", { text: group.title }),
        GROUP_NOTES[group.id] ? h("p", { class: "helper group-note", text: GROUP_NOTES[group.id] }) : null,
        h("div", { class: "card" }, members.map(settingRow)),
      );
      scroll.append(fieldset);
    }
    scroll.append(answers.element);
    syncDirty();
  }

  function lockFoot() {
    save.disabled = true;
    revert.disabled = true;
    status.textContent = "";
  }

  async function load() {
    const [all, hasRecorder] = await Promise.all([call("settings"), hotkeys.load()]);
    recorder = hasRecorder;
    // The recorder owns the hotkeys; Save never writes them as text alongside it.
    rows = (all || []).filter((row) => !(recorder && row.key.startsWith("CLICKER_HOTKEY_")));
    if (rows.length) {
      // The assistant rows always exist here, in this order, even before the core lists them.
      const known = new Map(rows.map((row) => [row.key, row]));
      const assistRows = ASSIST.map((a) => {
        const row = known.get(a.key) || { key: a.key, value: "", fallback: a.fallback || (a.secret ? "Not set" : ""), secret: Boolean(a.secret), unlisted: true };
        return { ...row, label: a.label };
      });
      rows = [...assistRows, ...rows.filter((row) => !ASSIST_KEYS.has(row.key))];
    }
    try {
      prefs = (await window.assistant.getPrefs()) || prefs;
    } catch {
      /* no shell bridge: the toggles show their defaults */
    }
    await answers.load();
    render();
  }

  save.addEventListener("click", async () => {
    const values = {};
    for (const row of rows) {
      const value = inputs.get(row.key).value.trim();
      // A row this core does not list is sent only when the user changed it; a secret only when typed.
      if (row.unlisted && (row.secret ? value === "" : value === (row.value || ""))) continue;
      values[row.key] = value;
    }
    save.disabled = true;
    const result = await call("save_settings", { values });
    if (!result) {
      syncDirty();
      return;
    }
    // The reply names the keys, never their values; neither do we.
    const hotkeysChanged = (result.keys || []).some((key) => key.startsWith("CLICKER_HOTKEY_"));
    toast({
      kind: "success",
      title: "Settings saved",
      body: hotkeysChanged ? "Restart Pointer for the new hotkeys to take effect." : "Changes are live.",
    });
    await load();
  });

  revert.addEventListener("click", render);
  if (window.assistant) {
    window.assistant.onWake((state) => {
      prefs.wakeState = state;
      paintWake();
    });
  }

  return { element, onShow: load };
}
