// glow.test.ts's browser: Chromium (Playwright, as the environment has it) opens the
// page and prints what it found as one JSON line. Node, not Bun: Playwright is Node's
const { chromium } = require("playwright");

(async () => {
  const b = await chromium.launch();
  try {
    const p = await b.newPage();
    p.on("console", (m) => process.stderr.write(`page: ${m.text()}\n`));
    await p.goto(process.argv[2]);
    await p.waitForFunction(() => window.result !== undefined, null, { timeout: 60_000 });
    console.log(JSON.stringify(await p.evaluate(() => window.result)));
  } finally {
    await b.close();
  }
})().catch((e) => {
  console.log(JSON.stringify({ ok: false, error: String(e?.message ?? e) }));
  process.exit(1);
});
