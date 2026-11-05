// the latency bench: the real host (AFTERGLOW_BIN) driven over the real link, timed
// from the input written to the frame it caused arriving at the driver. Two loads:
// "echo" (a key typed into xterm, a few glyphs change) and "flip" (each key flips xterm
// to reverse video, every pixel changes), at 720p and 1080p, 120 fps, 20 Mb/s as
// phosphorus asks. Also open to first frame. `bun run bench`; needs Xvfb and xterm

import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AfterglowSource, type Opened, type Sink, type VideoSink } from "../../src/index.ts";

const bin = process.env.AFTERGLOW_BIN ?? "host/target/release/afterglow";
const n = Number(process.env.BENCH_N ?? 40);
const dir = mkdtempSync(join(tmpdir(), "afterglow-bench-"));
const port = 48100 + Math.floor(Math.random() * 800);
const token = "bench".repeat(5);
const settle = (ms: number) => new Promise((r) => setTimeout(r, ms));

const xterm = Bun.which("xterm")!;
const script = (name: string, body: string) => {
  const p = join(dir, name);
  writeFileSync(p, `#!/bin/sh\n${body}\n`);
  chmodSync(p, 0o755);
  return p;
};
const echo = script("echo.sh", `exec ${xterm} +bc -bg white -fg black`);
const flip = script(
  "flip.sh",
  `exec ${xterm} +bc -bg white -fg black -e bash -c 'stty -echo; while :; do IFS= read -rsn1 k; printf "\\033[?5h"; IFS= read -rsn1 k; printf "\\033[?5l"; done'`,
);

writeFileSync(
  join(dir, "afterglow.conf"),
  `phos_link_port = ${port}\nphos_link_token = ${token}\nlog_path = ${join(dir, "afterglow.log")}\nmin_log_level = ${process.env.BENCH_LOG ?? "info"}\n`,
);
const host = Bun.spawn([bin, join(dir, "afterglow.conf")], { stderr: "ignore", env: { ...process.env, DISPLAY: "" } });
await settle(300);

class V implements Sink, VideoSink {
  frames: { at: number; bytes: number; key: boolean }[] = [];
  meta() {}
  say() {}
  exit() {}
  config() {}
  frame(key: boolean, data: Uint8Array) {
    this.frames.push({ at: performance.now(), bytes: data.length - 8, key });
  }
  ended() {}
}

const opened = (w: number, h: number): Opened => ({
  argv: [],
  name: "bench",
  w,
  h,
  sw: w,
  sh: h,
  scale: 1,
  full: false,
  fps: 120,
  dpi: 96,
});

const pct = (a: number[], p: number) => {
  const s = [...a].sort((x, y) => x - y);
  return s[Math.min(s.length - 1, Math.floor((s.length * p) / 100))]!;
};
const fmt = (a: number[]) => `${pct(a, 50).toFixed(1)}/${pct(a, 90).toFixed(1)}/${Math.max(...a).toFixed(1)}`;

const rows: string[] = [];
async function run(load: string, target: string, w: number, h: number) {
  const v = new V();
  const src = new AfterglowSource({ link: { port, token }, target, virtualDisplay: true });
  const t0 = performance.now();
  await src.open(opened(w, h), v);
  src.video({ w, h, fps: 120, kbps: 20_000, full: false }, v);
  while (!v.frames.length) await settle(1);
  const first = v.frames[0]!.at - t0;
  await settle(1500);
  const lat: number[] = [];
  const done: number[] = [];
  const sizes: number[] = [];
  for (let i = 0; i < n; i++) {
    const had = v.frames.length;
    const at = performance.now();
    src.input(["x", "a"], w, h);
    const end = at + 1000;
    while (v.frames.length === had && performance.now() < end) await Bun.sleep(0);
    // the burst it caused: frames until 50 ms of quiet
    while (performance.now() < end) {
      const last = v.frames.at(-1)!.at;
      if (v.frames.length > had && performance.now() - last > 50) break;
      await Bun.sleep(1);
    }
    const burst = v.frames.slice(had);
    if (burst.length) {
      lat.push(burst[0]!.at - at);
      done.push(burst.at(-1)!.at - at);
      sizes.push(burst.reduce((a, f) => a + f.bytes, 0));
    }
    await settle(20 + Math.random() * 40);
  }
  src.quit();
  await settle(300);
  const kb = sizes.reduce((a, b) => a + b, 0) / Math.max(1, sizes.length) / 1024;
  rows.push(
    `${load.padEnd(5)} ${`${w}x${h}`.padEnd(9)} open→frame ${first.toFixed(0).padStart(4)} ms | p50/p90/max ms: first ${fmt(lat)} | settled ${fmt(done)} | ${kb.toFixed(1)} KiB | ${lat.length}/${n}`,
  );
  console.log(rows.at(-1));
}

try {
  for (const [w, h] of [
    [1280, 720],
    [1920, 1080],
  ] as const) {
    await run("echo", echo, w, h);
    await run("flip", flip, w, h);
  }
} finally {
  host.kill();
  await host.exited;
  if (process.env.BENCH_KEEP) console.log(`kept ${dir}`);
  else rmSync(dir, { recursive: true, force: true });
}
