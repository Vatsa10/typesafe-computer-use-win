// Layout and link checks at 1280x800: nothing permanently covers a control, and no link is broken
// except the one intended 404 (the "Our awards" link).
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "@playwright/test";
import { startSite } from "./harness.js";

const sites = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "sites");
const pages = [];
for (const d of fs.readdirSync(sites, { withFileTypes: true }).filter((e) => e.isDirectory()))
  for (const f of fs.readdirSync(path.join(sites, d.name)).filter((x) => x.endsWith(".html"))) pages.push(`/${d.name}/${f}`);
const INTENDED_404 = new Set(["/acme/awards.html"]);

let site;
test.beforeAll(async () => { site = await startSite(); });
test.afterAll(async () => { await site.close(); });

test("found the pages", () => { expect(pages.length).toBeGreaterThanOrEqual(20); });

for (const p of pages) {
  test(`no control is permanently blocked: ${p}`, async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.goto(site.base + p);
    await page.waitForTimeout(2300); // let timed popups appear
    // Temporary overlays must be dismissible with a control that is itself on top.
    for (const id of ["#pop", "#cookie"]) {
      if (await page.locator(id).isVisible()) {
        const btn = page.locator(id === "#pop" ? "#nl-close" : "#cookie-no");
        expect(await topmost(btn), `${id} dismiss button covered`).toBe(true);
        await btn.click();
        await expect(page.locator(id)).toBeHidden();
      }
    }
    const blocked = await page.evaluate(() => {
      const out = [];
      const els = [...document.querySelectorAll("a[href],button,input,select,textarea,summary")];
      for (const el of els) {
        const cs = getComputedStyle(el);
        if (cs.display === "none" || cs.visibility === "hidden" || el.disabled || el.type === "hidden" || el.closest("[hidden]")) continue;
        if (el.closest(".menu") ) continue; // hover menu, checked separately
        let r = el.getBoundingClientRect();
        if (!r.width || !r.height) continue;
        el.scrollIntoView({ block: "center" });
        r = el.getBoundingClientRect();
        const x = r.left + r.width / 2, y = r.top + r.height / 2;
        const top = document.elementFromPoint(x, y);
        const lab = el.labels ? [...el.labels] : [];
        if (!top || !(el === top || el.contains(top) || top.contains(el) || lab.some((l) => l === top || l.contains(top)))) out.push(`${el.tagName.toLowerCase()}#${el.id || el.name || el.textContent.trim().slice(0, 20)} covered by ${top ? top.tagName.toLowerCase() + "." + top.className : "nothing"}`);
        if (r.left < 0 || r.right > innerWidth + 1) out.push(`${el.tagName} off-screen horizontally`);
      }
      return out;
    });
    expect(blocked).toEqual([]);
    // Dropdown links must be reachable on hover.
    if (await page.locator(".dd > span").count()) {
      await page.hover(".dd > span");
      for (const a of await page.locator(".menu a").all()) expect(await topmost(a), "menu link covered").toBe(true);
    }
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  });
}

async function topmost(loc) {
  return loc.evaluate((el) => { const r = el.getBoundingClientRect(); const t = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2); return !!t && (t === el || el.contains(t)); });
}

test("links and scripts: only the intended 404", async ({ request }) => {
  const seenUrls = new Set(), linked = new Set(), broken = [];
  for (const p of pages) {
    const html = fs.readFileSync(path.join(sites, p), "utf8");
    for (const m of html.matchAll(/(?:href|src|action)="([^"#]+)(?:#[^"]*)?"/g)) {
      let u = m[1];
      if (/['+]/.test(u)) continue; // built in script strings (search results), not a real link
      if (/^(https?:)?\/\/|^mailto:|^javascript:|^data:/.test(u)) continue;
      u = new URL(u, "http://x" + p).pathname;
      linked.add(u);
      if (seenUrls.has(u)) continue;
      seenUrls.add(u);
      const r = await request.get(site.base + u, { maxRedirects: 5 });
      if (r.status() !== 200 && !INTENDED_404.has(u)) broken.push(`${u} (from ${p}) -> ${r.status()}`);
      if (INTENDED_404.has(u)) expect(r.status()).toBe(404);
    }
  }
  expect(broken).toEqual([]);
  for (const u of INTENDED_404) expect(linked.has(u), `${u} must be linked`).toBe(true);
});
