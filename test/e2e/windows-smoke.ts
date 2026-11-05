// the built Windows host run as a plain process on a CI runner and driven over its
// link: the token, ready, an app launched without a virtual display, a video's config
// and first frame. The runner has a display (Microsoft's basic adapter) and no GPU
// encoder, so the frame is the software encoder's. Then the desktop: the launched app
// listed with its window, a Notepad started here attached to and closed through its
// window, a file in the profile sent to the Recycle Bin and one deleted for good, one
// outside refused. Exits non-zero on any failure
// usage: bun test/e2e/windows-smoke.ts <afterglow-out dir>
// (AFTERGLOW_WRAP=wine runs it under wine, its paths given as Z:; wine has no desktop
// duplication, so there NO_VIDEO=1 takes the video's end, said why, as the outcome)

import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { configOf, frameOf, In, jsonOf, Link, Out } from "../../src/index.ts";

const out = process.argv[2] ?? "afterglow-out";
const dir = mkdtempSync(join(tmpdir(), "afterglow-smoke-"));
const port = 47150;
const token = "smoke".repeat(6);
const conf = join(dir, "afterglow.conf");
const log = join(dir, "afterglow.log");
const logArg = process.env.AFTERGLOW_WRAP ? `Z:${log}` : log;
writeFileSync(
  conf,
  `phos_link_port = ${port}\r\nphos_link_token = ${token}\r\nlog_path = ${logArg}\r\nmin_log_level = debug\r\n`,
);
const wrap = process.env.AFTERGLOW_WRAP?.split(" ").filter(Boolean) ?? [];
const host = Bun.spawn([...wrap, join(out, "afterglow.exe"), wrap.length ? `Z:${conf}` : conf], {
  stdout: "inherit",
  stderr: "inherit",
});
await Bun.sleep(1500);

let failed = "";
const got: Uint8Array[] = [];
const link = new Link({ port, token }, { message: (m) => got.push(m), closed: () => {} });
try {
  const ready = await link.open(5000);
  console.log("ready", ready);
  const launched = link.next(Out.launched, 30_000);
  link.json(In.launch, {
    cmd: "C:\\Windows\\System32\\notepad.exe",
    name: "Notepad",
    w: 1280,
    h: 720,
    fps: 30,
    virtual_display: false,
    fresh: true,
  });
  const l = jsonOf(await launched);
  console.log("launched", l);
  if (l.ok !== true) throw new Error(`launch: ${JSON.stringify(l)}`);
  link.json(In.video, { gen: 1, w: 1280, h: 720, fps: 30, kbps: 4000, yuv444: false });
  // the config and a frame, or the video's end and why
  const t0 = Date.now();
  const has = (t: number) => got.find((m) => m[0] === t);
  while (!has(Out.frame) && Date.now() - t0 < 30_000) {
    const ended = has(Out.videoEnded);
    if (ended && process.env.NO_VIDEO) break;
    if (ended) throw new Error(`video ended: ${JSON.stringify(jsonOf(ended))}`);
    await Bun.sleep(50);
  }
  const c = has(Out.config);
  const f = has(Out.frame);
  const ended = has(Out.videoEnded);
  if (ended && process.env.NO_VIDEO) console.log("video ended, as expected here:", jsonOf(ended));
  else if (!c || !f) throw new Error("no config and frame in 30s");
  else {
    const cfg = configOf(c);
    console.log("config", { gen: cfg.gen, w: cfg.w, h: cfg.h, avcc: cfg.avcc.length });
    console.log("frame", { ...frameOf(f), bytes: f.length });
  }
  // the desktop, where there is one to see (not under wine)
  if (!wrap.length) {
    const shown = await link.processes();
    console.log(
      "processes",
      shown.map((p) => `${p.name} (${p.pid}) ${JSON.stringify(p.windows)}`),
    );
    if (!shown.some((p) => p.name.toLowerCase() === "notepad.exe" && p.windows.length))
      throw new Error("the launched Notepad isn't listed with a window");
  }
  link.send(In.quit);
  await link.next(Out.exited, 15_000);
  console.log("exited");
  if (!wrap.length) await desktop();
} catch (e) {
  failed = String(e);
} finally {
  link.close();
  host.kill();
  await Bun.sleep(500);
  console.log(`--- ${log}\n${readFileSync(log, "utf8")}`);
}
// a Notepad started here (not by afterglow) attached to, its windows streamed, then
// asked to close as its close button would; files in the profile deleted
async function desktop() {
  if (!(link.ready?.features ?? []).includes("attach")) throw new Error("ready doesn't offer attach");
  const np = Bun.spawn(["C:\\Windows\\System32\\notepad.exe"]);
  let target: { pid: number; started: number } | undefined;
  for (let i = 0; i < 50 && !target; i++) {
    await Bun.sleep(200);
    const l = await link.processes();
    // the one started here, or (where notepad.exe hands off) any Notepad with a window
    const p =
      l.find((x) => x.pid === np.pid && x.windows.length) ?? l.find((x) => /notepad/i.test(x.name) && x.windows.length);
    if (p) target = { pid: p.pid, started: p.started };
  }
  if (!target) throw new Error("the Notepad started here never showed a window");
  const attached = link.next(Out.launched, 30_000);
  link.json(In.attach, { ...target, name: "Notepad", w: 1280, h: 720, fps: 30, virtual_display: false });
  const a = jsonOf(await attached);
  console.log("attached", a);
  if (a.ok !== true || a.attached !== true) throw new Error(`attach: ${JSON.stringify(a)}`);
  const ended = await link.end(target.pid, { started: target.started, waitMs: 10_000 });
  console.log("ended", ended);
  if (ended.how !== "closed" || !ended.gone) throw new Error(`end: ${JSON.stringify(ended)}`);
  await link.next(Out.exited, 15_000);
  console.log("the attached app's exit told");

  // files in the profile (the temp folder is in it): to the Recycle Bin, and for good
  const bin = join(dir, "to-the-bin.txt");
  const gone = join(dir, "for-good.txt");
  writeFileSync(bin, "x");
  writeFileSync(gone, "y");
  const r1 = await link.delete(bin);
  const r2 = await link.delete(gone, true);
  console.log("deleted", r1, r2);
  if (!r1.recycled || r2.recycled || existsSync(bin) || existsSync(gone)) throw new Error("the deletes didn't happen");
  const refused = await link.delete("C:\\Windows\\win.ini").then(
    () => "",
    (e) => String(e),
  );
  console.log("refused:", refused);
  if (!refused.includes("outside")) throw new Error("a delete outside the profile wasn't refused");
}

if (failed) {
  console.error(`smoke test failed: ${failed}`);
  process.exit(1);
}
console.log("smoke test passed");
