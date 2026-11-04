// The presence pill's window: always on top, top centre of the primary display, never takes the
// focus, click-through everywhere except the pill itself, draggable, and remembered.
//
// The window is larger than the pill so the hover panel has somewhere to open. It ignores the mouse
// (with forwarding, so the page still sees the pointer arrive) and the page asks for clicks back only
// while the pointer is over the pill or its panel. The pill sits at the top of the window when the
// panel opens downward and at the bottom when it opens upward; either way the pill itself stays put.
//
// The only time this window becomes focusable is when the user clicks into its text box, and only
// until they click out.

const path = require("node:path");
const { BrowserWindow, ipcMain, screen } = require("electron");

const W = 440;
const H = 560;
const PILL_H = 44;
const TOP_GAP = 8;

function createPill({ onSend, onMic, onStop, onTaskStop, onConfirm, onSendIgnored, onMoved }) {
  let win = null;
  let ready = false;
  let latest = null;
  let anchor = null; // { cx, top } of the pill in screen DIPs
  let dir = "down";
  let drag = null;

  const fromPill = (event) => win && !win.isDestroyed() && event.sender === win.webContents;
  const workAreas = () => screen.getAllDisplays().map((d) => d.workArea);
  const inside = (a, x, y) => x >= a.x && x <= a.x + a.width && y >= a.y && y <= a.y + a.height;

  function home() {
    const area = screen.getPrimaryDisplay().workArea;
    return { cx: Math.round(area.x + area.width / 2), top: area.y + TOP_GAP };
  }

  // A remembered spot is used only if it is still on a screen; monitors come and go.
  function validAnchor(a) {
    if (!a) return null;
    const area = workAreas().find((w) => inside(w, a.cx, a.top + PILL_H / 2));
    if (!area) return null;
    return { cx: a.cx, top: Math.min(Math.max(a.top, area.y), area.y + area.height - PILL_H) };
  }

  // Open downward unless the panel would run off the bottom of the screen and there is room above.
  function dirFor(a) {
    const area = workAreas().find((w) => inside(w, a.cx, a.top + PILL_H / 2)) || screen.getPrimaryDisplay().workArea;
    const fitsDown = a.top + H <= area.y + area.height;
    const fitsUp = a.top + PILL_H - H >= area.y;
    return !fitsDown && fitsUp ? "up" : "down";
  }

  function boundsFor(a, d) {
    return { x: Math.round(a.cx - W / 2), y: Math.round(d === "down" ? a.top : a.top + PILL_H - H), width: W, height: H };
  }

  function place(a) {
    anchor = a;
    dir = dirFor(a);
    if (win && !win.isDestroyed()) win.setBounds(boundsFor(a, dir));
    sendState();
  }

  function sendState() {
    if (!win || win.isDestroyed() || !ready || !latest) return;
    win.webContents.send("pill-state", { ...latest, dir });
  }

  function show(saved) {
    if (win && !win.isDestroyed()) {
      if (!win.isVisible()) win.showInactive();
      return;
    }
    anchor = validAnchor(saved) || home();
    dir = dirFor(anchor);
    ready = false;
    win = new BrowserWindow({
      ...boundsFor(anchor, dir),
      title: "Pointer buddy", // the core skips windows with exactly this title
      frame: false,
      transparent: true,
      focusable: false, // never steals focus from the app being used
      skipTaskbar: true,
      resizable: false,
      movable: false, // moved by hand below, so the window keeps its size across displays
      hasShadow: false,
      alwaysOnTop: true,
      show: false,
      webPreferences: { preload: path.join(__dirname, "..", "preload.js"), contextIsolation: true, backgroundThrottling: false },
    });
    win.setAlwaysOnTop(true, "screen-saver");
    win.setIgnoreMouseEvents(true, { forward: true });
    win.on("page-title-updated", (event) => event.preventDefault());
    win.on("closed", () => {
      win = null;
      ready = false;
    });
    win.loadFile(path.join(__dirname, "..", "pill.html"));
    win.once("ready-to-show", () => win && !win.isDestroyed() && win.showInactive());
  }

  function hide() {
    if (win && !win.isDestroyed()) win.destroy();
    win = null;
    ready = false;
  }

  function push(snapshot) {
    latest = snapshot;
    sendState();
  }

  ipcMain.on("pill-ready", (event) => {
    if (!fromPill(event)) return;
    ready = true;
    sendState();
  });
  ipcMain.on("pill-hit", (event, over) => {
    if (fromPill(event)) win.setIgnoreMouseEvents(!over, { forward: true });
  });
  ipcMain.on("pill-focus", (event) => {
    if (!fromPill(event)) return;
    win.setFocusable(true);
    win.focus();
  });
  ipcMain.on("pill-blur", (event) => {
    if (fromPill(event)) win.setFocusable(false);
  });
  ipcMain.on("pill-drag", (event, move) => {
    if (!fromPill(event) || !move) return;
    if (move.phase === "start") {
      const b = win.getBounds();
      drag = { x: b.x, y: b.y, sx: move.sx, sy: move.sy };
    } else if (move.phase === "move" && drag) {
      win.setBounds({ x: Math.round(drag.x + move.sx - drag.sx), y: Math.round(drag.y + move.sy - drag.sy), width: W, height: H });
    } else if (move.phase === "end" && drag) {
      drag = null;
      const b = win.getBounds();
      const next = validAnchor({ cx: Math.round(b.x + W / 2), top: Math.round(dir === "down" ? b.y : b.y + H - PILL_H) }) || home();
      place(next);
      if (onMoved) onMoved(next);
    }
  });
  ipcMain.on("pill-send", (event, text) => fromPill(event) && onSend(text));
  ipcMain.on("pill-mic", (event) => fromPill(event) && onMic());
  ipcMain.on("pill-stop", (event) => fromPill(event) && onStop());
  ipcMain.on("pill-task-stop", (event, id) => fromPill(event) && onTaskStop(id));
  ipcMain.on("pill-confirm", (event, id, allow) => fromPill(event) && onConfirm(id, allow));
  ipcMain.on("pill-send-ignored", (event, id) => fromPill(event) && onSendIgnored(id));

  screen.on("display-metrics-changed", () => {
    if (anchor && win && !win.isDestroyed()) place(validAnchor(anchor) || home());
  });

  return {
    show,
    hide,
    push,
    window: () => (win && !win.isDestroyed() ? win : null),
    anchor: () => anchor,
    dir: () => dir,
    place: (a) => place(validAnchor(a) || home()),
    visible: () => Boolean(win && !win.isDestroyed()),
  };
}

module.exports = { createPill, W, H, PILL_H };
