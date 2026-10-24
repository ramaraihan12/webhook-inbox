// Seeds the running compose stack with made-up deliveries and saves the three
// screenshots in docs/. Start from an empty database so the list is tidy:
//
//   docker compose down -v && docker compose up --build -d
//   npm install --no-save playwright && npx playwright install chromium
//   node scripts/screenshots.mjs
//
// If playwright lives somewhere else, set PLAYWRIGHT_DIR to that package folder.
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_DIR || "playwright");

const base = process.env.BASE || "http://127.0.0.1:8020";
const token = process.env.ADMIN_TOKEN || "demo-admin-token";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function send(kind) {
  execFileSync("bash", [join(root, "examples/send.sh"), kind], {
    env: { ...process.env, BASE: base },
    stdio: "ignore",
  });
}

for (const kind of ["github", "github", "github", "midtrans", "xendit", "partner", "partner", "bad", "github"]) {
  send(kind);
  await sleep(150);
}
// The partner target is down, so those two go dead after 5 tries, and the
// midtrans billing target fails twice before it works.
await sleep(20000);

const browser = await chromium.launch();
const context = await browser.newContext({
  viewport: { width: 1200, height: 760 },
  httpCredentials: { username: "demo", password: token },
});
const page = await context.newPage();

await page.goto(`${base}/ui`);
await page.waitForSelector("table.grid");
await page.screenshot({ path: join(root, "docs/deliveries.png") });

await page.goto(`${base}/ui/dead`);
await page.waitForSelector("table.grid");
await page.screenshot({ path: join(root, "docs/dead-letters.png") });

await page.goto(`${base}/ui?source=midtrans`);
await page.waitForSelector("table.grid");
await page.click("table.grid tbody tr a");
await page.waitForSelector(".facts");
await page.screenshot({ path: join(root, "docs/delivery-detail.png"), fullPage: true });

await browser.close();
