// Tiny static server for the eval sites. Built-ins only, 127.0.0.1 only.
//   GET  /<site>/...        static files from evals/sites
//   POST /result            a page reports {page, status, data, title, tab}; kept in memory
//   GET  /results           {results:[...]} everything reported since the last reset
//   POST /reset             clear results (DELETE /results does the same)
//   POST /hb  {tab}         a test tab's heartbeat; the reply {close:bool} tells it to close itself
//   GET  /tabs              {tabs:{id:lastSeenMs}}
//   POST /close?tab=ID      ask that tab to close
//   GET  /health
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(here, "sites");
const TYPES = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".json": "application/json", ".svg": "image/svg+xml", ".txt": "text/plain; charset=utf-8" };

export function createServer() {
  let results = [];
  const tabs = new Map(); // id -> { seen, close }
  const send = (res, code, body, type = "application/json") => {
    res.writeHead(code, { "content-type": type, "cache-control": "no-store" });
    res.end(typeof body === "string" || Buffer.isBuffer(body) ? body : JSON.stringify(body));
  };
  const readBody = (req) => new Promise((ok) => {
    let b = "";
    req.on("data", (c) => { b += c; if (b.length > 1e6) req.destroy(); });
    req.on("end", () => { try { ok(b ? JSON.parse(b) : {}); } catch { ok({}); } });
  });

  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, "http://127.0.0.1");
    const p = decodeURIComponent(url.pathname);
    if (req.method === "POST" && p === "/result") {
      const b = await readBody(req);
      results.push({ page: String(b.page || ""), status: String(b.status || ""), data: b.data ?? {}, title: String(b.title || ""), tab: String(b.tab || ""), t: Date.now() });
      return send(res, 200, { ok: true });
    }
    if (req.method === "GET" && p === "/results") return send(res, 200, { results });
    if ((req.method === "POST" && p === "/reset") || (req.method === "DELETE" && p === "/results")) { results = []; return send(res, 200, { ok: true }); }
    if (req.method === "POST" && p === "/hb") {
      const b = await readBody(req);
      const id = String(b.tab || "");
      if (!id) return send(res, 200, { close: false });
      const t = tabs.get(id) || { seen: 0, close: false };
      t.seen = Date.now();
      tabs.set(id, t);
      return send(res, 200, { close: t.close });
    }
    if (req.method === "GET" && p === "/tabs") return send(res, 200, { tabs: Object.fromEntries([...tabs].map(([k, v]) => [k, { seen: v.seen, close: v.close }])) });
    if (req.method === "POST" && p === "/close") { const t = tabs.get(url.searchParams.get("tab") || ""); if (t) t.close = true; return send(res, 200, { ok: Boolean(t) }); }
    if (p === "/health") return send(res, 200, { ok: true });
    if (req.method !== "GET" && req.method !== "HEAD") return send(res, 405, { error: "method" });

    let file = path.normalize(path.join(ROOT, p));
    if (!file.startsWith(ROOT)) return send(res, 403, "forbidden", "text/plain");
    try {
      if (fs.statSync(file).isDirectory()) {
        if (!p.endsWith("/")) { res.writeHead(302, { location: p + "/" + url.search }); return res.end(); }
        file = path.join(file, "index.html");
      }
      return send(res, 200, fs.readFileSync(file), TYPES[path.extname(file)] || "application/octet-stream");
    } catch {
      return send(res, 404, `<!doctype html><title>404 Not Found</title><h1>404</h1><p>There is nothing at ${p.replace(/</g, "&lt;")}.</p>`, "text/html; charset=utf-8");
    }
  });
  return { server, tabs, listen: (port = 0) => new Promise((ok) => server.listen(port, "127.0.0.1", () => ok(server.address().port))), close: () => new Promise((ok) => server.close(ok)) };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const port = Number(process.env.EVAL_PORT || process.argv[2] || 8787);
  const s = createServer();
  s.listen(port).then((pt) => console.log(`eval sites on http://127.0.0.1:${pt}/`));
}
