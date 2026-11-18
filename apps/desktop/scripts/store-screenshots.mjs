// Store screenshots, taken from the desktop UI running on its mock backend: `npm run dev`, then
// `node scripts/store-screenshots.mjs windows` (Microsoft Store) or `… mac` (Mac App Store).
// They look like the store builds: ?store drops the streaming tab and the updater, and the user
// agent picks the platform (Windows: no title-bar padding; Mac: room for the traffic lights).
// Both show glass, the default look (set explicitly, so a look picked in this browser profile
// doesn't leak in). Covers are the mock's own abstract art
// (src/lib/mockCovers.ts).
//   windows: 1440×810 at 2× = 2880×1620 (16:9) into store/screenshots/windows
//   mac:     1440×900 at 2× = 2880×1800 (16:10) into store/screenshots/mac
import { chromium } from "playwright-core";

const PLATFORMS = {
  windows: {
    look: "glass",
    size: { width: 1440, height: 810 },
    ua: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0",
  },
  mac: {
    look: "glass",
    size: { width: 1440, height: 900 },
    ua: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
  },
};
const os = process.argv[2] ?? "windows";
const platform = PLATFORMS[os];
if (!platform) throw new Error(`usage: store-screenshots.mjs windows|mac [url] [out dir]`);
const url = process.argv[3] ?? "http://127.0.0.1:1420";
const out = process.argv[4] ?? new URL(`../store/screenshots/${os}`, import.meta.url).pathname;
const browser = await chromium.launch({ channel: "chrome" }); // the installed Google Chrome
const page = await browser.newPage({ viewport: platform.size, deviceScaleFactor: 2, userAgent: platform.ua, locale: "en-US" });
await page.addInitScript((l) => localStorage.setItem("look", JSON.stringify(l)), platform.look);
const tab = (name) => page.getByRole("button", { name, exact: true }).first().click();
const shot = async (file, wait = 1500) => {
  await page.mouse.move(1, platform.size.height - 5);
  await page.waitForTimeout(wait);
  await page.screenshot({ path: `${out}/${file}` });
  console.log(file);
};

await page.goto(`${url}/?store`);
await page.waitForTimeout(1500);

await tab("Library");
await page.getByRole("button").filter({ hasText: "Strings in Winter" }).first().click();
await page.getByRole("button", { name: "Play", exact: true }).click();
await page.waitForTimeout(500);

// Equalizer on (the mock starts with it on): the EQ screen and the processing path.
await tab("Equalizer");
await shot("3-eq.png");
await tab("Now Playing");
await shot("4-processing.png", 4000);

// Equalizer off: bit-perfect.
await tab("Equalizer");
await page.getByRole("switch").first().evaluate((el) => el.click());
await tab("Now Playing");
await shot("1-bitperfect.png", 4000);

await tab("Library");
await page.getByRole("button", { name: "Back" }).click().catch(() => {});
await shot("2-library.png");

await browser.close();
