// Shared by the specs: a server on a random port, scoring with run.mjs's own evaluate(), and the
// task list. Nothing here opens a visible browser.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "../serve.mjs";
import { evaluate } from "../run.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
export const TASKS = JSON.parse(fs.readFileSync(path.join(here, "..", "tasks.json"), "utf8")).tasks;
export const task = (id) => TASKS.find((t) => t.id === id);

export async function startSite() {
  const srv = createServer();
  const port = await srv.listen(0);
  const base = `http://127.0.0.1:${port}`;
  return {
    base,
    close: () => srv.close(),
    reset: () => fetch(`${base}/reset`, { method: "POST" }),
    results: async () => (await (await fetch(`${base}/results`)).json()).results,
    // Same function the live runner uses on GET /results.
    score: async (id) => evaluate(task(id).check, (await (await fetch(`${base}/results`)).json()).results),
  };
}

// Wait until the server has seen a result with this page/status.
export async function seen(site, page, status, ms = 5000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if ((await site.results()).some((r) => r.page === page && r.status === status)) return true;
    await new Promise((r) => setTimeout(r, 50));
  }
  return false;
}
