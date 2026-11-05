// the real host (host/, built) on a real X server, driven by AfterglowSource over the
// real link: xterm launched, its screen streamed as H.264 that ffmpeg decodes, a
// command typed into it that runs, a keyframe asked for, a resize, quit. Runs where
// AFTERGLOW_BIN names a built afterglow and Xvfb, xterm and ffmpeg are installed
// (`bun run test:host`); skipped otherwise

import { afterAll, beforeAll, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AfterglowSource, FrameRoom, type Opened, type Sink, type VideoSink } from "../../src/index.ts";

const bin = process.env.AFTERGLOW_BIN ?? "";
const run = !!bin && existsSync(bin) && !!Bun.which("Xvfb") && !!Bun.which("xterm") && !!Bun.which("ffmpeg");

const dir = mkdtempSync(join(tmpdir(), "afterglow-e2e-"));
const display = `:${90 + Math.floor(Math.random() * 9)}`;
const port = 47100 + Math.floor(Math.random() * 800);
const token = "e2e".repeat(8);
let xvfb: ReturnType<typeof Bun.spawn> | undefined;
let host: ReturnType<typeof Bun.spawn> | undefined;

const settle = (ms: number) => new Promise((r) => setTimeout(r, ms));
async function until(f: () => boolean, ms: number, what: string) {
  const end = Date.now() + ms;
  while (!f()) {
    if (Date.now() > end) throw new Error(`timed out: ${what}`);
    await settle(20);
  }
}

beforeAll(async () => {
  if (!run) return;
  xvfb = Bun.spawn(["Xvfb", display, "-screen", "0", "1280x720x24", "-nolisten", "tcp"], { stderr: "ignore" });
  await settle(500);
  writeFileSync(
    join(dir, "afterglow.conf"),
    `phos_link_port = ${port}\r\nphos_link_token = ${token}\r\nlog_path = ${join(dir, "afterglow.log")}\r\nmin_log_level = debug\r\n`,
  );
  host = Bun.spawn([bin, join(dir, "afterglow.conf")], { env: { ...process.env, DISPLAY: display }, stderr: "ignore" });
  await settle(300);
});

afterAll(() => {
  host?.kill();
  xvfb?.kill();
  if (process.env.AFTERGLOW_KEEP) console.log(`kept ${dir}`);
  else rmSync(dir, { recursive: true, force: true });
});

class Viewer implements Sink, VideoSink {
  configs: { avcc: Uint8Array; w: number; h: number }[] = [];
  frames: { key: boolean; avcc: Uint8Array; at: number }[] = [];
  ends: boolean[] = [];
  exits: string[] = [];
  meta() {}
  say() {}
  exit(e: string) {
    this.exits.push(e);
  }
  config(avcc: Uint8Array, w: number, h: number) {
    this.configs.push({ avcc: avcc.slice(), w, h });
  }
  frame(key: boolean, data: Uint8Array, room?: boolean) {
    expect(room).toBe(true);
    this.frames.push({ key, avcc: data.slice(FrameRoom), at: performance.now() });
  }
  ended(got: boolean) {
    this.ends.push(got);
  }
}

// an avcC and AVCC frames back to an Annex B stream ffmpeg reads
function annexB(avcc: Uint8Array, frames: Uint8Array[]): Uint8Array {
  const sc = [0, 0, 0, 1];
  const out: number[] = [];
  const spsLen = (avcc[6]! << 8) | avcc[7]!;
  out.push(...sc, ...avcc.subarray(8, 8 + spsLen));
  const p = 8 + spsLen + 1;
  const ppsLen = (avcc[p]! << 8) | avcc[p + 1]!;
  out.push(...sc, ...avcc.subarray(p + 2, p + 2 + ppsLen));
  for (const f of frames) {
    const v = new DataView(f.buffer, f.byteOffset, f.byteLength);
    for (let i = 0; i < f.length; ) {
      const n = v.getUint32(i);
      out.push(...sc, ...f.subarray(i + 4, i + 4 + n));
      i += 4 + n;
    }
  }
  return new Uint8Array(out);
}

// decoded with ffmpeg: the frame count, each frame's gray pixels' md5
function decode(stream: Uint8Array, w: number, h: number) {
  const f = join(dir, `v-${w}x${h}.h264`);
  writeFileSync(f, stream);
  const r = Bun.spawnSync(["ffmpeg", "-v", "error", "-f", "h264", "-i", f, "-f", "framemd5", "-pix_fmt", "gray", "-"]);
  const err = r.stderr.toString();
  const md5s = r.stdout
    .toString()
    .split("\n")
    .filter((l) => l && !l.startsWith("#"))
    .map((l) => l.split(",").at(-1)!.trim());
  const probe = Bun.spawnSync(["ffprobe", "-v", "error", "-show_entries", "stream=width,height", "-of", "csv=p=0", f]);
  return { err, md5s, size: probe.stdout.toString().trim() };
}

// the stream's last picture as gray pixels
function lastFrame(stream: Uint8Array, w: number, h: number): Uint8Array {
  const f = join(dir, "last.h264");
  writeFileSync(f, stream);
  const r = Bun.spawnSync(["ffmpeg", "-v", "error", "-f", "h264", "-i", f, "-f", "rawvideo", "-pix_fmt", "gray", "-"]);
  const all = new Uint8Array(r.stdout);
  return all.subarray(all.length - w * h);
}

const opened = (w: number, h: number): Opened => ({
  argv: [],
  name: "xterm",
  w,
  h,
  sw: 1280,
  sh: 720,
  scale: 1,
  full: false,
  fps: 30,
  dpi: 96,
});

test.skipIf(!run)(
  "xterm streamed, typed into, resized and quit over the real link",
  async () => {
    const v = new Viewer();
    let done = 0;
    const src = new AfterglowSource({
      link: { port, token },
      target: Bun.which("xterm")!,
      virtualDisplay: true,
      done: () => done++,
    });
    const t0 = performance.now();
    await src.open(opened(1280, 720), v);
    src.video({ w: 1280, h: 720, fps: 30, kbps: 4000, full: false }, v);
    await until(() => v.frames.length > 0, 5000, "the first frame");
    const first = performance.now() - t0;
    console.log(`open to first frame: ${first.toFixed(0)} ms (link ${src.timings.link?.toFixed(1)} ms)`);
    expect(v.configs[0]).toMatchObject({ w: 1280, h: 720 });
    expect(v.frames[0]!.key).toBe(true);

    // xterm draws its window: frames follow. Then a command typed as text and an Enter key
    await settle(1500);
    const proof = join(dir, "typed");
    const before = v.frames.length;
    src.input(["m", 300, 200], 1280, 720);
    src.input(["x", `echo afterglow > ${proof}`], 1280, 720);
    src.input(["k", true, "Enter", "Enter", false], 1280, 720);
    src.input(["k", false, "Enter", "Enter", false], 1280, 720);
    await until(() => existsSync(proof), 5000, "the typed command to run");
    expect(readFileSync(proof, "utf8")).toBe("afterglow\n");
    await until(() => v.frames.length > before, 2000, "frames of the typing");

    // a keyframe asked of the running video
    const keysBefore = v.frames.filter((f) => f.key).length;
    src.keyframe();
    await until(() => v.frames.filter((f) => f.key).length > keysBefore, 2000, "the keyframe");

    // the pointer moved over the bare root window: drawn into the frames where it went
    const moved = v.frames.length;
    src.input(["m", 900, 500], 1280, 720);
    await until(() => v.frames.length > moved, 2000, "a frame of the pointer's move");
    const shot = lastFrame(
      annexB(
        v.configs[0]!.avcc,
        v.frames.map((f) => f.avcc),
      ),
      1280,
      720,
    );
    const lit = (x0: number, y0: number, r: number) => {
      let n = 0;
      for (let y = y0 - r; y < y0 + r; y++) for (let x = x0 - r; x < x0 + r; x++) if (shot[y * 1280 + x]! > 128) n++;
      return n;
    };
    expect(lit(900, 500, 12)).toBeGreaterThan(10);
    expect(lit(640, 360, 12)).toBe(0);

    // everything so far decodes, at the size asked
    const d = decode(
      annexB(
        v.configs[0]!.avcc,
        v.frames.map((f) => f.avcc),
      ),
      1280,
      720,
    );
    expect(d.err).toBe("");
    expect(d.size).toBe("1280,720");
    expect(d.md5s.length).toBe(v.frames.length);
    // the typing changed the picture
    expect(new Set(d.md5s).size).toBeGreaterThan(2);

    // a new size: a fresh video, its own config and keyframe
    const next = new Viewer();
    src.video({ w: 960, h: 540, fps: 30, kbps: 3000, full: false }, next);
    await until(() => next.frames.length > 0, 5000, "the resized video's first frame");
    expect(next.configs[0]).toMatchObject({ w: 960, h: 540 });
    expect(next.frames[0]!.key).toBe(true);
    // no window manager: the keyboard follows the pointer, so back over xterm first
    src.input(["m", 100, 100], 960, 540);
    await settle(300);
    const typedAt = performance.now();
    const had = next.frames.length;
    src.input(["x", "x"], 960, 540);
    await until(() => next.frames.length > had, 2000, "a frame after typing");
    console.log(`a key typed to its frame: ${(next.frames[had]!.at - typedAt).toFixed(1)} ms`);
    const d2 = decode(
      annexB(
        next.configs[0]!.avcc,
        next.frames.map((f) => f.avcc),
      ),
      960,
      540,
    );
    expect(d2.err).toBe("");
    expect(d2.size).toBe("960,540");

    // the app asked to quit: the host closes it and says so
    src.close();
    await until(() => v.exits.length > 0 || next.exits.length > 0 || done > 0, 8000, "the app's exit");
    expect(done).toBe(1);
  },
  60_000,
);

test.skipIf(!run)("a second driver finds the host free, a missing app refused", async () => {
  const src = new AfterglowSource({ link: { port, token }, target: "/nonexistent/app", virtualDisplay: false });
  await expect(src.open(opened(640, 480), new Viewer())).rejects.toThrow("the app didn't start");
});
