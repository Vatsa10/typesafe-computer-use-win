// Runs the task set against the real assistant in a headless browser: real models, no window.
//   node evals/headless.mjs --bin <headless_task.exe> --home <POINTER_HOME> [--provider openai|claude|both]
//                           [--task a,b] [--category c]
import { spawn } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { evaluate } from "./run.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const args = Object.fromEntries(process.argv.slice(2).join(" ").split("--").filter(Boolean).map((a) => {
  const [k, ...v] = a.trim().split(" ");
  return [k, v.join(" ")];
}));
const base = "http://127.0.0.1:8787";
const providers = !args.provider || args.provider === "both" ? ["openai", "claude"] : [args.provider];
let tasks = JSON.parse(readFileSync(path.join(here, "tasks.json"), "utf8"));
tasks = (Array.isArray(tasks) ? tasks : tasks.tasks).filter((t) => t.category !== "teach");
if (args.task) tasks = tasks.filter((t) => args.task.split(",").includes(t.id));
if (args.category) tasks = tasks.filter((t) => t.category === args.category);

function runTask(task, provider) {
  return new Promise((resolve) => {
    const seconds = Math.min(task.maxSeconds || 120, 150);
    const child = spawn(args.bin, ["--assistant", "--provider", provider, "--url", base + task.start, "--goal", task.goal,
      "--results", `${base}/results`, "--max-seconds", String(seconds)],
      { env: { ...process.env, POINTER_HOME: args.home }, stdio: ["ignore", "pipe", "pipe"] });
    let out = "";
    child.stdout.on("data", (d) => (out += d));
    child.stderr.on("data", (d) => (out += d));
    const timer = setTimeout(() => child.kill(), (seconds + 30) * 1000);
    child.on("close", () => { clearTimeout(timer); resolve(out); });
  });
}

const rows = [];
for (const provider of providers) {
  for (const task of tasks) {
    await fetch(`${base}/reset`, { method: "POST" });
    const started = Date.now();
    const out = await runTask(task, provider);
    const seconds = (Date.now() - started) / 1000;
    const { results } = await (await fetch(`${base}/results`)).json();
    const score = evaluate(task.check, results);
    const ran = /event assistant|reply:/.test(out);
    const pass = score.pass && ran;
    const reply = (out.match(/^reply: (.*)$/gm) || []).pop() || "";
    rows.push({ provider, id: task.id, category: task.category, pass, seconds, missing: score.missing, forbidden: score.forbidden, reply: reply.slice(0, 240) });
    console.log(`${pass ? "PASS" : "FAIL"} ${provider.padEnd(6)} ${task.id.padEnd(28)} ${seconds.toFixed(1)}s`);
  }
}
const dir = path.join(here, "results");
mkdirSync(dir, { recursive: true });
writeFileSync(path.join(dir, "headless-latest.json"), JSON.stringify(rows, null, 1));
for (const provider of providers) {
  const mine = rows.filter((r) => r.provider === provider);
  const passed = mine.filter((r) => r.pass);
  const mean = passed.reduce((a, r) => a + r.seconds, 0) / Math.max(passed.length, 1);
  console.log(`TOTAL ${provider}: ${passed.length}/${mine.length} passed, mean ${mean.toFixed(1)}s when passing`);
}
