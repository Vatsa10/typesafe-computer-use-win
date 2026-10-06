// The Settings view. Rows come from the core's `settings` call and are grouped here by key. A
// secret's value never reaches this page (the core sends it empty and says only whether one is
// set), and nothing here ever logs or echoes what is typed into a secret field.
import { call } from "../lib/core.js";
import { h, emptyState } from "../lib/dom.js";
import { icon } from "../lib/icons.js";
import { toast } from "./toast.js";

const GROUPS = [
  { id: "keys", title: "API keys", match: (row) => row.secret },
  { id: "hotkeys", title: "Hotkeys", match: (row) => row.key.startsWith("CLICKER_HOTKEY_") },
  { id: "voice", title: "Voice", match: (row) => /SPEAK|VOICE|BAR_SILENCE|TRANSCRIBE/.test(row.key) },
  { id: "browser", title: "Browser and sites", match: (row) => /BROWSER|EMAIL|CATALOG|OCR/.test(row.key) },
  { id: "models", title: "Models", match: (row) => /MODEL|PROVIDER/.test(row.key) },
  { id: "other", title: "Other", match: () => true },
];

const GROUP_NOTES = {
  keys: "Stored in .env. Leave a key empty to keep the one already saved.",
  hotkeys: "Hotkey changes take effect after Pointer restarts.",
};

export function createSettingsView() {
  const scroll = h("div", { class: "settings-scroll" });
  const save = h("button", { class: "btn accent", type: "button", html: icon("save") + "<span>Save</span>" });
  const revert = h("button", { class: "btn", type: "button", text: "Discard changes" });
  const status = h("span", { class: "helper", "aria-live": "polite" });
  let rows = [];
  let inputs = new Map();

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
      );
    } else {
      input.value = row.value || "";
      input.placeholder = row.fallback || "";
      helper = h("span", { class: "helper", id: helpId, text: row.fallback ? `Default: ${row.fallback}` : row.key });
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
      lockFoot();
      return;
    }
    const taken = new Set();
    for (const group of GROUPS) {
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
    syncDirty();
  }

  function lockFoot() {
    save.disabled = true;
    revert.disabled = true;
    status.textContent = "";
  }

  async function load() {
    rows = (await call("settings")) || [];
    render();
  }

  save.addEventListener("click", async () => {
    const values = {};
    for (const row of rows) values[row.key] = inputs.get(row.key).value.trim();
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

  return { element, onShow: load };
}
