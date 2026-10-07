// The free speech engine (CLICKER_STT=chrome, the default): Google's recognizer, reached through
// webkitSpeechRecognition in the user's own Chrome (or Edge, which ships with Windows).
//
// Why a separate browser: inside Electron webkitSpeechRecognition fails with "network" — Google's
// speech service refuses embedded Chromium — while real Chrome and Edge use it free and keyless.
// So this module, in Electron's main process, serves a tiny page (stt.html) on 127.0.0.1 and opens
// it in a dedicated, off-screen app window of that browser. Commands go to the page over
// server-sent events; transcripts come back as POSTs.
//
// Safety: the browser runs on its own profile (%LOCALAPPDATA%\pointer\speech-profile), never the
// user's, so it is a separate browser process that shares no windows, tabs or sign-ins with theirs.
// Only the process tree this module spawned is ever killed. Every URL carries a random per-session
// token and anything without it is refused. Transcripts are never logged here.

const http = require("node:http");
const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");
const { spawn, execFile } = require("node:child_process");

const PAGE = path.join(__dirname, "stt.html");
const STOP_TIMEOUT_MS = 8000;
const READY_TIMEOUT_MS = 20000;
const MAX_BODY = 64 * 1024;

/** The first Chrome, else Edge, that exists on this machine; null when neither does. */
function findBrowser(env = process.env, exists = fs.existsSync) {
  const pf = env.ProgramFiles || "C:\\Program Files";
  const pf86 = env["ProgramFiles(x86)"] || "C:\\Program Files (x86)";
  const local = env.LOCALAPPDATA || "";
  const candidates = [
    path.join(pf, "Google", "Chrome", "Application", "chrome.exe"),
    path.join(pf86, "Google", "Chrome", "Application", "chrome.exe"),
    local && path.join(local, "Google", "Chrome", "Application", "chrome.exe"),
    path.join(pf86, "Microsoft", "Edge", "Application", "msedge.exe"),
    path.join(pf, "Microsoft", "Edge", "Application", "msedge.exe"),
  ].filter(Boolean);
  return candidates.find((p) => exists(p)) || null;
}

function profileDir(env = process.env) {
  const local = env.LOCALAPPDATA || path.join(require("node:os").homedir(), "AppData", "Local");
  return path.join(local, "pointer", "speech-profile");
}

/** The command line for the dedicated speech window. Never the user's own profile. */
function browserArgs(url, profile) {
  return [
    `--user-data-dir=${profile}`,
    `--app=${url}`,
    "--use-fake-ui-for-media-stream", // grants the microphone without a prompt, on this profile only
    "--window-position=-32000,-32000",
    "--window-size=200,100",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-features=Translate",
    // Keep the page live while hidden: an off-screen, occluded window is otherwise throttled.
    "--disable-background-timer-throttling",
    "--disable-renderer-backgrounding",
    "--disable-backgrounding-occluded-windows",
  ];
}

function createBridge({ lang: initialLang = process.env.CLICKER_STT_LANG || "en-US", log = () => {}, onInterim = () => {} } = {}) {
  let lang = initialLang;
  let sweptOrphans = false;
  const token = crypto.randomBytes(24).toString("hex");
  let server = null;
  let port = 0;
  let child = null;
  let starting = null; // the promise of a server + browser + connected page
  const clients = new Set(); // open SSE responses
  let waitingForPage = []; // resolvers for "a page connected"
  let waitingForFinal = null; // { resolve, timer } while a stop is in flight
  let lastError = "";
  let listening = false; // the page confirmed its recognizer started, since the last start()
  let quitting = false;

  const browser = findBrowser();

  function available() {
    return Boolean(browser);
  }

  function send(cmd) {
    const line = `data: ${JSON.stringify({ cmd })}\n\n`;
    for (const res of clients) res.write(line);
    return clients.size > 0;
  }

  function settleFinal(text) {
    if (!waitingForFinal) return;
    const { resolve, timer } = waitingForFinal;
    waitingForFinal = null;
    clearTimeout(timer);
    resolve(text);
  }

  function onResult(body) {
    let msg;
    try {
      msg = JSON.parse(body);
    } catch {
      return;
    }
    if (msg.state === "listening") listening = true;
    if (msg.error) {
      lastError = String(msg.error);
      log(`speech: ${lastError}`); // an error code such as no-speech, never a transcript
    }
    // Live text goes to whoever asked (the command bar), never to a log.
    if (!msg.final && typeof msg.interim === "string" && listening) onInterim(msg.interim);
    if (msg.final) settleFinal(typeof msg.text === "string" ? msg.text.trim() : "");
  }

  function handle(req, res) {
    // Only this session's token, only on our own host name (no DNS-rebinding through a page).
    const host = req.headers.host || "";
    const url = new URL(req.url, `http://127.0.0.1:${port}`);
    const parts = url.pathname.split("/").filter(Boolean);
    if (host !== `127.0.0.1:${port}` || parts.length !== 2 || parts[0] !== token) {
      res.writeHead(404).end();
      return;
    }
    const route = `${req.method} ${parts[1]}`;
    if (route === "GET stt.html") {
      res.writeHead(200, { "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-store" });
      res.end(fs.readFileSync(PAGE));
    } else if (route === "GET events") {
      res.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-store", Connection: "keep-alive" });
      res.write(": connected\n\n");
      clients.add(res);
      req.on("close", () => clients.delete(res));
      const waiting = waitingForPage;
      waitingForPage = [];
      waiting.forEach((resolve) => resolve(true));
    } else if (route === "POST result") {
      let body = "";
      req.setEncoding("utf8");
      req.on("data", (chunk) => {
        body += chunk;
        if (body.length > MAX_BODY) req.destroy();
      });
      req.on("end", () => {
        res.writeHead(204).end();
        onResult(body);
      });
    } else {
      res.writeHead(404).end();
    }
  }

  function listen() {
    if (server) return Promise.resolve();
    return new Promise((resolve, reject) => {
      server = http.createServer(handle);
      server.on("error", reject);
      server.listen(0, "127.0.0.1", () => {
        port = server.address().port;
        resolve();
      });
    });
  }

  function pageUrl() {
    return `http://127.0.0.1:${port}/${token}/stt.html?lang=${encodeURIComponent(lang)}`;
  }

  // A speech browser left over from a force-killed Pointer would swallow the new launch (Chrome hands
  // a second launch on the same profile to the running one). Only processes on OUR dedicated profile
  // are matched, so the user's own browser is never touched. Once per session.
  function sweepOrphans() {
    if (sweptOrphans) return;
    sweptOrphans = true;
    const profile = profileDir().replace(/'/g, "''");
    const script =
      "Get-CimInstance Win32_Process -Filter \"Name='chrome.exe' or Name='msedge.exe'\" | " +
      `Where-Object { $_.CommandLine -and $_.CommandLine.Contains('${profile}') } | ` +
      "ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }";
    try {
      require("node:child_process").execFileSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], {
        stdio: "ignore",
        windowsHide: true,
        timeout: 8000,
      });
    } catch {
      // Nothing to sweep, or PowerShell unavailable: launching still works, just possibly into the old one.
    }
  }

  function launch() {
    if (child && child.exitCode === null) return;
    sweepOrphans();
    const proc = spawn(browser, browserArgs(pageUrl(), profileDir()), { stdio: "ignore", windowsHide: false });
    child = proc;
    proc.on("error", (e) => {
      log(`speech: could not start the browser (${e.message})`);
      if (child === proc) child = null;
    });
    proc.on("exit", () => {
      if (child === proc) child = null;
      // A dead browser is relaunched lazily: the next start() brings it back.
      if (!quitting) log("speech: the speech browser closed; it restarts on next use");
    });
  }

  function waitForPage(ms) {
    if (clients.size > 0) return Promise.resolve(true);
    return new Promise((resolve) => {
      const done = (ok) => {
        clearTimeout(timer);
        resolve(ok);
      };
      const timer = setTimeout(() => {
        waitingForPage = waitingForPage.filter((r) => r !== done);
        resolve(false);
      }, ms);
      waitingForPage.push(done);
    });
  }

  async function ensure() {
    if (!browser) throw new Error("free voice needs Google Chrome or Microsoft Edge installed");
    await listen();
    if (!child || child.exitCode !== null) {
      // The page that belonged to a dead browser is gone; forget its streams.
      for (const res of clients) res.end();
      clients.clear();
      launch();
    }
    if (!(await waitForPage(READY_TIMEOUT_MS))) throw new Error("the speech browser did not load its page");
  }

  /** Begin listening. Resolves true once the page has the command. */
  async function start() {
    if (!starting) starting = ensure().finally(() => (starting = null));
    await starting;
    lastError = "";
    listening = false;
    settleFinal(""); // a stop still in flight from an earlier utterance is over
    if (!send("start")) throw new Error("the speech page is not connected");
    return true;
  }

  /** Stop and return the final transcript ("" for silence, an error, or no answer in time). */
  function stop() {
    if (!send("stop")) return Promise.resolve("");
    settleFinal("");
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        waitingForFinal = null;
        resolve("");
      }, STOP_TIMEOUT_MS);
      waitingForFinal = { resolve, timer };
    });
  }

  /** Drop the current utterance: the page posts no transcript and any pending stop resolves "". */
  function cancel() {
    listening = false;
    send("cancel");
    settleFinal("");
  }

  /** Kill the browser this bridge spawned (its process tree only) and close the server. */
  function shutdown() {
    quitting = true;
    for (const res of clients) res.end();
    clients.clear();
    if (child && child.exitCode === null && child.pid) {
      try {
        execFile("taskkill", ["/PID", String(child.pid), "/T", "/F"], { windowsHide: true }, () => {});
      } catch {
        child.kill();
      }
    }
    child = null;
    if (server) server.close();
    server = null;
  }

  // The language comes from the core's settings once it is up; it applies to the next launch.
  function setLang(next) {
    if (next) lang = next;
  }

  return {
    setLang,
    start,
    stop,
    cancel,
    available,
    shutdown,
    lastError: () => lastError,
    listening: () => listening,
    browser: () => browser,
    pid: () => (child ? child.pid : null),
  };
}

module.exports = { createBridge, findBrowser, browserArgs, profileDir };
