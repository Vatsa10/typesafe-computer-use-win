// Preferences that belong to the shell rather than the core: whether the wake phrase and the camera
// are on, and where the presence pill was left. Plain JSON under %LOCALAPPDATA%\pointer. Nothing
// secret ever goes in here (the agent key lives in the core's .env).

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const DEFAULTS = { wake: false, camera: false, pill: null };

function homeDir() {
  const local = process.env.LOCALAPPDATA || path.join(os.homedir(), "AppData", "Local");
  return path.join(local, "pointer");
}

function prefsFile() {
  return process.env.POINTER_UI_PREFS_FILE || path.join(homeDir(), "ui-prefs.json");
}

function load() {
  try {
    const raw = JSON.parse(fs.readFileSync(prefsFile(), "utf8"));
    return {
      wake: raw.wake === true,
      camera: raw.camera === true,
      pill: raw.pill && Number.isFinite(raw.pill.cx) && Number.isFinite(raw.pill.top) ? { cx: raw.pill.cx, top: raw.pill.top } : null,
    };
  } catch {
    return { ...DEFAULTS };
  }
}

function save(next) {
  const file = prefsFile();
  try {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, JSON.stringify({ wake: next.wake === true, camera: next.camera === true, pill: next.pill || null }, null, 2));
    return true;
  } catch {
    return false;
  }
}

module.exports = { load, save, homeDir, prefsFile };
