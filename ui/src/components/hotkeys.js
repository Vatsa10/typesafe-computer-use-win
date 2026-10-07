// The Hotkeys section of Settings: one recorder per action. "Change" listens for the next combo,
// shows it in the core's spec syntax (lowercase, "ctrl+alt+space"), flags clashes between Pointer's
// own actions, and "Apply" hands the changed ones to the core (`set_hotkeys`), which registers them
// at once and says per action whether Windows gave it the combo or another app already holds it.
import { h } from "../lib/dom.js";

const ACTIONS = [
  { name: "bar", label: "Open the command bar" },
  { name: "talk", label: "Talk", hold: true },
  { name: "dictate", label: "Dictate", hold: true },
  { name: "goal", label: "Open Pointer" },
  { name: "pause", label: "Pause or resume a run" },
  { name: "abort", label: "Stop a run" },
  { name: "quit", label: "Quit Pointer" },
];

const MODIFIER_CODES = new Set(["ControlLeft", "ControlRight", "AltLeft", "AltRight", "ShiftLeft", "ShiftRight", "MetaLeft", "MetaRight", "OSLeft", "OSRight"]);

/** The core's name for a physical key, or "" when the core has no name for it. */
export function keyName(code) {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3).toLowerCase();
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^F([1-9]|1[0-2])$/.test(code)) return code.toLowerCase();
  if (code === "Space") return "space";
  return "";
}

/** A keydown as a spec ("ctrl+alt+k"), or { error } when it cannot be one. */
export function specFromEvent(e) {
  const key = keyName(e.code);
  if (!key) return { error: "That key can't be a hotkey. Use a letter, digit, Space or F1 to F12." };
  const mods = [];
  if (e.ctrlKey) mods.push("ctrl");
  if (e.altKey) mods.push("alt");
  if (e.shiftKey) mods.push("shift");
  if (e.metaKey) mods.push("win");
  if (!mods.length && !/^f\d+$/.test(key)) return { error: "Add Ctrl, Alt, Shift or Win, or use an F key." };
  return { spec: [...mods, key].join("+") };
}

const pretty = (spec) => (spec || "none").split("+").join(" + ");

export function createHotkeysSection() {
  const card = h("div", { class: "card" });
  const apply = h("button", { class: "btn accent", type: "button", text: "Apply" });
  const discard = h("button", { class: "btn", type: "button", text: "Discard" });
  const status = h("span", { class: "helper", role: "status", "aria-live": "polite" });
  const element = h(
    "fieldset",
    { class: "group", id: "hotkeys-group" },
    h("legend", { text: "Hotkeys" }),
    h("p", { class: "helper group-note", text: "Click Change, then press the new combo. Esc cancels, Backspace puts back the default. Applied at once." }),
    card,
    h("div", { class: "hk-actions" }, apply, discard, status),
  );

  let current = {};
  let defaults = {};
  let draft = {};
  let recording = null; // the action name being recorded
  let rightAltDown = false;
  const rows = new Map();

  function setRecordingFlag(on) {
    // Tells the shell to ignore Pointer's own hotkey events while a combo is being pressed here.
    window.core.call("ui_recording_hotkeys", { on }).catch(() => {});
  }

  function conflicts() {
    const byspec = new Map();
    for (const { name } of ACTIONS) {
      const spec = draft[name];
      if (!spec) continue;
      if (!byspec.has(spec)) byspec.set(spec, []);
      byspec.get(spec).push(name);
    }
    const out = {};
    for (const names of byspec.values()) if (names.length > 1) for (const n of names) out[n] = names.filter((m) => m !== n);
    return out;
  }

  function changed() {
    return ACTIONS.filter(({ name }) => draft[name] !== current[name]).map(({ name }) => name);
  }

  function labelOf(name) {
    return ACTIONS.find((a) => a.name === name).label;
  }

  function paint() {
    const clash = conflicts();
    for (const { name } of ACTIONS) {
      const row = rows.get(name);
      const isRec = recording === name;
      row.keys.textContent = isRec ? "Press keys…" : pretty(draft[name]);
      row.keys.classList.toggle("recording", isRec);
      row.keys.classList.toggle("dirty", !isRec && draft[name] !== current[name]);
      row.change.textContent = isRec ? "Cancel" : "Change";
      row.change.setAttribute("aria-pressed", String(isRec));
      if (clash[name]) {
        row.note.className = "helper danger hk-note";
        row.note.textContent = `Also used by ${clash[name].map(labelOf).join(", ")}`;
      } else if (row.result) {
        row.note.className = "helper hk-note " + row.result.kind;
        row.note.textContent = row.result.text;
      } else {
        row.note.className = "helper hk-note";
        row.note.textContent = "";
      }
    }
    const dirty = changed().length > 0;
    const hasClash = Object.keys(clash).length > 0;
    apply.disabled = !dirty || hasClash || recording !== null;
    discard.disabled = !dirty;
    if (hasClash) status.textContent = "Two actions share a combo. Change one to apply.";
    else if (recording) status.textContent = `Recording a combo for ${labelOf(recording)}`;
    else if (dirty) status.textContent = "Unapplied changes";
  }

  function stopRecording() {
    if (!recording) return;
    const name = recording;
    recording = null;
    rightAltDown = false;
    window.removeEventListener("keydown", onKeyDown, true);
    window.removeEventListener("keyup", onKeyUp, true);
    window.removeEventListener("blur", onBlur);
    setRecordingFlag(false);
    paint();
    rows.get(name).change.focus();
  }

  function startRecording(name) {
    if (recording) stopRecording();
    recording = name;
    rows.get(name).result = null;
    status.textContent = "";
    window.addEventListener("keydown", onKeyDown, true);
    window.addEventListener("keyup", onKeyUp, true);
    window.addEventListener("blur", onBlur);
    setRecordingFlag(true);
    paint();
  }

  function take(spec) {
    draft[recording] = spec;
    stopRecording();
  }

  // While recording, every key belongs to the recorder: nothing else on the panel may act on it.
  function onKeyDown(e) {
    e.preventDefault();
    e.stopPropagation();
    e.stopImmediatePropagation();
    if (e.repeat) return;
    if (e.key === "Escape" && !e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey) return stopRecording();
    if (e.key === "Backspace" && !e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey) return take(defaults[recording] || current[recording]);
    if (e.code === "AltRight" && !e.ctrlKey && !e.shiftKey && !e.metaKey) {
      rightAltDown = true; // bare Right Alt, if it is released before any other key
      return;
    }
    rightAltDown = false;
    if (MODIFIER_CODES.has(e.code)) return; // wait for the key that completes the combo
    const result = specFromEvent(e);
    if (result.error) {
      rows.get(recording).result = { kind: "danger", text: result.error };
      paint();
      return;
    }
    rows.get(recording).result = null;
    take(result.spec);
  }

  function onKeyUp(e) {
    e.preventDefault();
    e.stopPropagation();
    e.stopImmediatePropagation();
    if (e.code === "AltRight" && rightAltDown) take("rightalt");
  }

  function onBlur() {
    stopRecording();
  }

  function row({ name, label, hold }) {
    const id = "hk-" + name;
    const keys = h("span", { class: "hk-keys", id: id + "-keys", "aria-live": "polite" });
    const change = h("button", {
      class: "btn hk-change",
      type: "button",
      "aria-describedby": `${id}-label ${id}-keys`,
      onclick: () => (recording === name ? stopRecording() : startRecording(name)),
    });
    const note = h("span", { class: "helper hk-note", id: id + "-note", role: "status", "aria-live": "polite" });
    const entry = { keys, change, note, result: null };
    rows.set(name, entry);
    return h(
      "div",
      { class: "setting hk-row" },
      h("span", { class: "hk-label", id: id + "-label" }, label, hold ? h("span", { class: "hk-hold", text: "hold" }) : null),
      h("span", { class: "helper", text: `Default: ${pretty(defaults[name])}` }),
      h("div", { class: "hk-control" }, keys, change),
      note,
    );
  }

  apply.addEventListener("click", async () => {
    const names = changed();
    if (!names.length) return;
    const values = {};
    for (const n of names) values[n] = draft[n];
    apply.disabled = true;
    status.textContent = "Applying…";
    let reply;
    try {
      reply = await window.core.call("set_hotkeys", { values });
    } catch (error) {
      reply = { ok: false, error: String((error && error.message) || error) };
    }
    if (reply && reply.ok) {
      const applied = (reply.result && reply.result.applied) || {};
      const refused = new Set((reply.result && reply.result.refused) || []);
      for (const n of names) {
        const r = rows.get(n);
        if (refused.has(n)) {
          r.result = { kind: "danger", text: "Taken by another app. Your old combo still works." };
          draft[n] = current[n];
        } else {
          r.result = { kind: "success", text: "Applied" };
          current[n] = applied[n] || draft[n];
          draft[n] = current[n];
        }
      }
      status.textContent = refused.size ? `${names.length - refused.size} applied, ${refused.size} refused` : "Hotkeys applied";
    } else {
      const message = (reply && reply.error) || "the core did not answer";
      // The core packs one reason per action: "hotkeys not saved: goal: <why>; abort: <why>".
      const reasons = new Map();
      for (const part of message.replace(/^hotkeys not saved:\s*/, "").split(/;\s*/)) {
        const m = part.match(/^(\w+):\s*(.+)$/);
        if (m) reasons.set(m[1], m[2]);
      }
      let placed = false;
      for (const n of names) {
        if (reasons.has(n)) {
          rows.get(n).result = { kind: "danger", text: reasons.get(n) };
          placed = true;
        }
      }
      status.textContent = placed ? "Some hotkeys were not applied" : `Not applied: ${message}`;
    }
    paint();
  });

  discard.addEventListener("click", () => {
    stopRecording();
    draft = { ...current };
    for (const r of rows.values()) r.result = null;
    status.textContent = "";
    paint();
  });

  /** Fill from the core's `hotkeys` reply. Returns false when the core has no such method. */
  async function load() {
    stopRecording();
    let reply;
    try {
      reply = await window.core.call("hotkeys", {});
    } catch {
      reply = null;
    }
    if (!(reply && reply.ok && reply.result && reply.result.current)) return false;
    current = { ...reply.result.current };
    defaults = { ...(reply.result.defaults || {}) };
    draft = { ...current };
    rows.clear();
    card.replaceChildren(...ACTIONS.map(row));
    status.textContent = "";
    paint();
    return true;
  }

  return { element, load, stop: stopRecording };
}
