#!/usr/bin/env node
// Live task runner for Pointer. Drives pointer-core over its line protocol (ui/src/protocol.md),
// one task at a time, because one desktop has one mouse.
//
//   node evals/run.mjs --dry-list                 print the plan, touch nothing
//   node evals/run.mjs --go [--provider openai|claude|both] [--task ID[,ID]] [--category C]
//
// Options: --core PATH  --port N  --out DIR  --gap SECONDS  --browser chrome|default
//          --scale X (multiply every task's time limit)  --limit N (first N tasks)
// Live runs move the real mouse and keyboard, so they need --go.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { createServer } from "./serve.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");

// USD per million tokens, [input, output]. Estimates for the table only; edit to match your plan.
export const PRICES = { openai: [2.0, 8.0], claude: [3.0, 15.0] };

// ---- success checks -------------------------------------------------------------------------

const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
export function matches(r, want) {
  if (r.page !== want.page || r.status !== want.status) return false;
  const data = r.data || {};
  return Object.entries(want.fields || {}).every(([k, v]) => same(data[k], v) || (typeof v === "number" && String(data[k]) === String(v)));
}
export function evaluate(check, results) {
  const missing = (check.must || []).filter((w) => !results.some((r) => matches(r, w)));
  const forbidden = (check.mustNot || []).filter((w) => results.some((r) => matches(r, w)));
  return { pass: missing.length === 0 && forbidden.length === 0, missing, forbidden };
}

// ---- args and plan --------------------------------------------------------------------------

export function parseArgs(argv) {
  const a = { providers: ["openai", "claude"], gap: 3, scale: 1, port: 8787, browser: "chrome" };
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i], v = () => argv[++i];
    if (k === "--dry-list") a.dry = true;
    else if (k === "--go") a.go = true;
    else if (k === "--provider") { const p = v(); a.providers = p === "both" ? ["openai", "claude"] : [p]; }
    else if (k === "--task") a.tasks = v().split(",");
    else if (k === "--category") a.category = v();
    else if (k === "--core") a.core = v();
    else if (k === "--port") a.port = Number(v());
    else if (k === "--out") a.out = v();
    else if (k === "--gap") a.gap = Number(v());
    else if (k === "--scale") a.scale = Number(v());
    else if (k === "--limit") a.limit = Number(v());
    else if (k === "--browser") a.browser = v();
    else { console.error(`unknown option ${k}`); process.exit(2); }
  }
  for (const p of a.providers) if (!PRICES[p]) { console.error(`provider must be openai, claude or both (got ${p})`); process.exit(2); }
  return a;
}

export function loadTasks(args) {
  const all = JSON.parse(fs.readFileSync(path.join(here, "tasks.json"), "utf8")).tasks;
  let t = all;
  if (args.tasks) {
    const unknown = args.tasks.filter((id) => !all.some((x) => x.id === id));
    if (unknown.length) { console.error(`unknown task id: ${unknown.join(", ")}`); process.exit(2); }
    t = t.filter((x) => args.tasks.includes(x.id));
  }
  if (args.category) t = t.filter((x) => x.category === args.category);
  if (args.limit) t = t.slice(0, args.limit);
  return t;
}

function printPlan(tasks, args) {
  console.log(`Plan: ${tasks.length} tasks x ${args.providers.length} provider(s) [${args.providers.join(", ")}], strictly one at a time\n`);
  const w = Math.max(...tasks.map((t) => t.id.length));
  tasks.forEach((t, i) => {
    console.log(`${String(i + 1).padStart(2)}. ${t.id.padEnd(w)}  ${t.category.padEnd(12)} ${t.site.padEnd(14)} steps<=${String(t.maxSteps).padEnd(3)} ${String(t.maxSeconds).padStart(4)}s${t.mustNotComplete ? "  MUST NOT COMPLETE" : ""}`);
    console.log(`      goal: ${t.goal}`);
    const must = (t.check.must || []).map((m) => `${m.page}:${m.status}`).join(", ") || "-";
    const not = (t.check.mustNot || []).map((m) => `${m.page}:${m.status}`).join(", ") || "-";
    console.log(`      pass needs: ${must}   |   must not: ${not}`);
  });
  const cats = {};
  for (const t of tasks) cats[t.category] = (cats[t.category] || 0) + 1;
  console.log(`\nCategories: ${Object.entries(cats).map(([k, v]) => `${k} ${v}`).join(", ")}`);
  console.log("Nothing was started. Add --go to run live (moves the real mouse and keyboard).");
}

// ---- the core, over its line protocol -------------------------------------------------------

class Core {
  constructor(exe, env) {
    this.nextId = 1; this.pending = new Map(); this.listeners = []; this.lines = []; this.exited = false;
    this.proc = spawn(exe, ["--ipc"], { env, stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
    let buf = "";
    this.proc.stdout.setEncoding("utf8");
    this.proc.stdout.on("data", (d) => {
      buf += d; let n;
      while ((n = buf.indexOf("\n")) >= 0) { const line = buf.slice(0, n).trim(); buf = buf.slice(n + 1); if (line) this.onLine(line); }
    });
    this.proc.stderr.setEncoding("utf8");
    this.proc.stderr.on("data", (d) => this.lines.push(`[stderr] ${d.trim()}`));
    this.proc.on("exit", () => { this.exited = true; for (const p of this.pending.values()) p.reject(new Error("core exited")); this.pending.clear(); this.emit({ event: "__exit" }); });
  }
  onLine(line) {
    let m; try { m = JSON.parse(line); } catch { return; }
    if (m.id != null && this.pending.has(m.id)) {
      const p = this.pending.get(m.id); this.pending.delete(m.id);
      m.ok ? p.resolve(m.result) : p.reject(new Error(m.error || "request failed"));
    } else if (m.event) {
      if (m.event === "line") this.lines.push(m.text);
      this.emit(m);
    }
  }
  emit(m) { for (const l of [...this.listeners]) l(m); }
  on(fn) { this.listeners.push(fn); return () => { this.listeners = this.listeners.filter((x) => x !== fn); }; }
  call(method, params = {}, ms = 20000) {
    if (this.exited) return Promise.reject(new Error("core exited"));
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error(`${method} timed out`)); }, ms);
      this.pending.set(id, { resolve: (v) => { clearTimeout(timer); resolve(v); }, reject: (e) => { clearTimeout(timer); reject(e); } });
      this.proc.stdin.write(JSON.stringify({ id, method, params }) + "\n");
    });
  }
  stop() { try { this.proc.stdin.end(); } catch {} setTimeout(() => { try { this.proc.kill(); } catch {} }, 1500).unref(); }
}

function findCore(arg) {
  const c = [arg, process.env.POINTER_CORE, path.join(repo, "rust/target/release/pointer-core.exe"), path.join(repo, "rust/target/debug/pointer-core.exe"), path.join(repo, "rust/target/release/pointer-core"), path.join(repo, "rust/target/debug/pointer-core")].filter(Boolean);
  return c.find((p) => fs.existsSync(p));
}

// POINTER_HOME is a scratch folder, never the repo.
function scratchHome(label) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `pointer-eval-${label}-`));
  const rel = path.relative(repo, dir);
  if (!rel.startsWith("..") && !path.isAbsolute(rel)) throw new Error(`refusing POINTER_HOME inside the repo: ${dir}`);
  return dir;
}

// ---- the browser tab ------------------------------------------------------------------------

function openTab(url, browser) {
  const win = process.platform === "win32";
  const cmd = win ? "cmd" : process.platform === "darwin" ? "open" : "xdg-open";
  const args = win ? ["/c", "start", '""', browser === "chrome" ? "chrome" : "", url].filter((x) => x !== "") : [url];
  spawn(cmd, args, { stdio: "ignore", detached: true, windowsVerbatimArguments: win }).unref();
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(fn, ms, step = 250) { const end = Date.now() + ms; while (Date.now() < end) { if (await fn()) return true; await sleep(step); } return false; }

// ---- one task -------------------------------------------------------------------------------

function tokensFrom(lines) {
  let input = 0, output = 0, seen = false;
  for (const l of lines) {
    for (const m of l.matchAll(/(\d[\d,]*)\s*(?:input|prompt)[ _]?tokens?/gi)) { input += Number(m[1].replace(/,/g, "")); seen = true; }
    for (const m of l.matchAll(/(\d[\d,]*)\s*(?:output|completion)[ _]?tokens?/gi)) { output += Number(m[1].replace(/,/g, "")); seen = true; }
    for (const m of l.matchAll(/input[_ ]tokens["':= ]+(\d+)/gi)) { input += Number(m[1]); seen = true; }
    for (const m of l.matchAll(/output[_ ]tokens["':= ]+(\d+)/gi)) { output += Number(m[1]); seen = true; }
  }
  return seen ? { input, output } : null;
}

async function runTask(task, provider, ctx) {
  const { args, srv, base, exe } = ctx;
  const rec = { id: task.id, category: task.category, provider, pass: false, steps: null, seconds: 0, tokens: null, cost: null, note: "" };
  await fetch(`${base}/reset`, { method: "POST" });
  const home = scratchHome(provider);
  const env = { ...process.env, POINTER_HOME: home, POINTER_LLM: provider, CLICKER_NO_HOTKEYS: "1", POINTER_MODE: "assistant" };
  const core = new Core(exe, env);
  const tabId = `${task.id}-${provider}-${Date.now().toString(36)}`;
  let opened = false;
  try {
    const st = await core.call("state");
    await core.call("set_mode", { act: true });
    core.on((m) => { if (m.event === "confirm") core.call("confirm_reply", { id: m.id ?? m.payload?.id, allow: false }).catch(() => {}); });
    const before = (await core.call("runs").catch(() => [])).length;

    openTab(`${base}${task.start}${task.start.includes("?") ? "&" : "?"}_t=${tabId}`, args.browser);
    opened = true;
    const alive = await until(() => srv.tabs.has(tabId), 25000);
    if (!alive) { rec.note = "test tab never appeared"; return rec; }
    await sleep(1500);

    let busy = false, idle = false;
    const off = core.on((m) => {
      if (m.event === "assistant") { if (["thinking", "working", "control", "speaking"].includes(m.state)) busy = true; if (m.state === "idle" && busy) idle = true; }
      if (m.event === "state" && m.running === false && busy) idle = true;
    });
    const t0 = Date.now();
    await core.call("chat", { text: task.goal, source: "typed" });
    const limit = Math.round(task.maxSeconds * args.scale) * 1000;
    const finished = await until(() => idle || core.exited, limit, 300);
    rec.seconds = Math.round((Date.now() - t0) / 100) / 10;
    off();
    if (!finished) { rec.note = "timeout"; await core.call("stop_all").catch(() => {}); }
    await sleep(1500); // let the last page post its result

    const results = (await (await fetch(`${base}/results`)).json()).results;
    const verdict = evaluate(task.check, results);
    rec.pass = verdict.pass && (finished || task.mustNotComplete === true);
    if (!verdict.pass) rec.note = [verdict.missing.length ? `missing ${verdict.missing.map((m) => `${m.page}:${m.status}`).join(",")}` : "", verdict.forbidden.length ? `FORBIDDEN ${verdict.forbidden.map((m) => `${m.page}:${m.status}`).join(",")}` : "", rec.note].filter(Boolean).join("; ");
    else if (!finished && !task.mustNotComplete) rec.note = "timeout";
    rec.results = results.map((r) => `${r.page}:${r.status}`);

    const runs = await core.call("runs").catch(() => []);
    const fresh = runs.slice(0, Math.max(0, runs.length - before));
    const stepsRuns = fresh.reduce((n, r) => n + (r.steps_taken || 0), 0);
    const stepsLines = core.lines.filter((l) => /^step \d+/i.test(l)).length;
    rec.steps = stepsRuns || stepsLines || null;
    if (rec.steps != null && rec.steps > task.maxSteps) { rec.note = [rec.note, `over step budget (${rec.steps}>${task.maxSteps})`].filter(Boolean).join("; "); rec.pass = false; }
    rec.tokens = tokensFrom(core.lines);
    if (rec.tokens) rec.cost = Math.round(((rec.tokens.input * PRICES[provider][0] + rec.tokens.output * PRICES[provider][1]) / 1e6) * 10000) / 10000;
    rec.log = core.lines.slice(-40);
  } catch (e) {
    rec.note = `error: ${e.message}`;
  } finally {
    core.stop();
    if (opened) { // close only the tab we opened, by asking the page itself
      await fetch(`${base}/close?tab=${encodeURIComponent(tabId)}`, { method: "POST" }).catch(() => {});
      const gone = await until(() => { const t = srv.tabs.get(tabId); return !t || Date.now() - t.seen > 3500; }, 8000);
      rec.tabClosed = gone;
      if (!gone) rec.note = [rec.note, "tab still open: close it by hand"].filter(Boolean).join("; ");
    }
    try { fs.rmSync(home, { recursive: true, force: true }); } catch {}
  }
  return rec;
}

// ---- report ---------------------------------------------------------------------------------

export function table(records) {
  const out = [];
  for (const p of [...new Set(records.map((r) => r.provider))]) {
    const rs = records.filter((r) => r.provider === p);
    out.push(`\nProvider: ${p}   (POINTER_LLM=${p})`);
    out.push("task                         category      result  steps  seconds  tokens(in/out)    cost$   note");
    for (const r of rs) {
      const tok = r.tokens ? `${r.tokens.input}/${r.tokens.output}` : "n/a";
      out.push(`${r.id.padEnd(28)} ${r.category.padEnd(13)} ${(r.pass ? "PASS" : "FAIL").padEnd(7)} ${String(r.steps ?? "-").padStart(5)} ${String(r.seconds).padStart(8)}  ${tok.padEnd(16)} ${(r.cost == null ? "n/a" : r.cost.toFixed(4)).padStart(7)}   ${r.note}`);
    }
    const pass = rs.filter((r) => r.pass).length;
    const cost = rs.reduce((n, r) => n + (r.cost || 0), 0);
    out.push(`passed ${pass}/${rs.length}, total ${Math.round(rs.reduce((n, r) => n + r.seconds, 0))}s, estimated cost $${cost.toFixed(4)}${rs.some((r) => !r.tokens) ? " (tokens unavailable for some tasks)" : ""}`);
  }
  return out.join("\n");
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const tasks = loadTasks(args);
  if (args.dry) { printPlan(tasks, args); return; }
  if (!args.go) { printPlan(tasks, args); console.error("\nRefusing to run without --go."); process.exit(2); }
  const exe = findCore(args.core);
  if (!exe) { console.error("pointer-core not found. Build it (cargo build in rust/) or pass --core PATH / set POINTER_CORE."); process.exit(2); }
  const srv = createServer();
  const port = await srv.listen(args.port);
  const base = `http://127.0.0.1:${port}`;
  const outDir = path.resolve(args.out || path.join(here, "results", new Date().toISOString().replace(/[:.]/g, "-")));
  fs.mkdirSync(outDir, { recursive: true });
  console.log(`sites on ${base}; core ${exe}; results in ${outDir}`);
  const records = [];
  const save = () => { fs.writeFileSync(path.join(outDir, "results.json"), JSON.stringify({ startedAt: new Date().toISOString(), prices: PRICES, records }, null, 2)); fs.writeFileSync(path.join(outDir, "results.txt"), table(records)); };
  let aborted = false;
  process.on("SIGINT", () => { aborted = true; console.log("\nstopping after this task"); });
  for (const provider of args.providers) {
    for (const task of tasks) {
      if (aborted) break;
      console.log(`\n[${provider}] ${task.id}: ${task.goal}`);
      const rec = await runTask(task, provider, { args, srv, base, exe });
      records.push(rec); save();
      console.log(`  ${rec.pass ? "PASS" : "FAIL"} steps=${rec.steps ?? "-"} ${rec.seconds}s ${rec.note}`);
      await sleep(args.gap * 1000); // strictly one at a time
    }
  }
  console.log(table(records));
  console.log(`\nwritten: ${path.join(outDir, "results.json")}`);
  await srv.close().catch(() => {});
  process.exit(0);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
