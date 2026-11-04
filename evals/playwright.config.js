import { defineConfig } from "@playwright/test";

// Headless only. One worker: each spec file starts its own server on a random port.
export default defineConfig({
  testDir: "./reference",
  testMatch: /.*\.spec\.js/,
  workers: 1,
  fullyParallel: false,
  timeout: 30000,
  expect: { timeout: 5000 },
  reporter: [["list"]],
  use: { actionTimeout: 8000, headless: true, viewport: { width: 1280, height: 800 }, launchOptions: { headless: true } },
});
