// the real host (host/, built) driven by AfterglowSource over the real link. xterm on a
// display of its own (the host's Xvfb): filling it, streamed as H.264 that ffmpeg
// decodes, a command typed into it that runs, the pointer drawn where it went, a
// keyframe asked for, a resize the display follows, quit. Then xterm on the session's
// own display (an Xvfb this test starts). Runs where AFTERGLOW_BIN names a built
// afterglow and Xvfb, xterm and ffmpeg are installed (`bun run test:e2e`); skipped otherwise

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
  metas: Record<string, unknown>[] = [];
  meta(m: Record<string, unknown>) {
    this.metas.push(m);
  }
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

// the viewer's latest picture, gray
const shotOf = (v: Viewer, w: number, h: number) =>
  lastFrame(
    annexB(
      v.configs[0]!.avcc,
      v.frames.map((f) => f.avcc),
    ),
    w,
    h,
  );

// pixels around (x0, y0) that pass
function count(shot: Uint8Array, w: number, x0: number, y0: number, r: number, f: (p: number) => boolean) {
  let n = 0;
  for (let y = y0 - r; y < y0 + r; y++) for (let x = x0 - r; x < x0 + r; x++) if (f(shot[y * w + x]!)) n++;
  return n;
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

    // xterm maps its window: the host's window manager makes it fill the display
    await settle(1500);
    const filled = shotOf(v, 1280, 720);
    expect(count(filled, 1280, 1200, 680, 20, (p) => p > 200)).toBeGreaterThan(1000);

    // a command typed as text and an Enter key: the window manager gave xterm the keyboard
    const proof = join(dir, "typed");
    const before = v.frames.length;
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

    // the pointer is the viewer's: its shape came as a PNG, and a move draws nothing
    // into the frames (the host's own pointer kept out of what it reads)
    const shape = v.metas.find((m) => m.t === "cursor" && typeof m.png === "string");
    expect(shape).toBeDefined();
    expect(Buffer.from(String(shape!.png), "base64").subarray(1, 4).toString()).toBe("PNG");
    src.input(["m", 900, 500], 1280, 720);
    // a frame after the move, asked for, to look at
    await settle(200);
    const kf = v.frames.filter((f) => f.key).length;
    src.keyframe();
    await until(() => v.frames.filter((f) => f.key).length > kf, 2000, "a keyframe after the move");
    expect(count(shotOf(v, 1280, 720), 1280, 900, 500, 12, (p) => p < 100)).toBe(0);

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
    await settle(500);
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
    // the display followed the video: xterm fills 960x540, not a corner of 1280x720
    const resized = shotOf(next, 960, 540);
    expect(count(resized, 960, 900, 500, 20, (p) => p > 200)).toBeGreaterThan(1000);
    // and the whole screen read again after the resize has no pointer in it: the host's
    // own is hidden, the viewer draws it
    expect(count(resized, 960, 900, 500, 12, (p) => p < 100)).toBe(0);

    // a still screen: a frame a second to keep the stream alive, and the host idle (a
    // refinement with nothing left to send once left it spinning a core, no frame sent)
    await settle(1000);
    const cpu = () => {
      const f = readFileSync(`/proc/${host!.pid}/stat`, "utf8").split(") ")[1]!.split(" ");
      return (Number(f[11]) + Number(f[12])) / 100;
    };
    const stillFrom = next.frames.length;
    const c0 = cpu();
    await settle(3000);
    const used = cpu() - c0;
    const still = next.frames.length - stillFrom;
    console.log(`still for 3 s: ${still} frames, ${used.toFixed(2)} s of the host's CPU`);
    expect(still).toBeGreaterThanOrEqual(2);
    expect(still).toBeLessThanOrEqual(6);
    expect(used).toBeLessThan(0.5);

    // only the bitrate changed: the running capture carries on, its config and a
    // keyframe under the new video's gen at once
    // as Phosphorus's rate control does it: the video stopped, the next asked
    const retuned = new Viewer();
    const asked = performance.now();
    src.stopVideo();
    src.video({ w: 960, h: 540, fps: 30, kbps: 1500, full: false }, retuned);
    await until(() => retuned.frames.length > 0, 2000, "the retuned video's first frame");
    console.log(`a new bitrate to its first frame: ${(retuned.frames[0]!.at - asked).toFixed(1)} ms`);
    expect(retuned.configs[0]).toMatchObject({ w: 960, h: 540 });
    expect(retuned.frames[0]!.key).toBe(true);
    const d3 = decode(
      annexB(
        retuned.configs[0]!.avcc,
        retuned.frames.map((f) => f.avcc),
      ),
      960,
      540,
    );
    expect(d3.err).toBe("");

    // the app asked to quit: the host closes it and says so
    src.close();
    await until(() => v.exits.length > 0 || next.exits.length > 0 || done > 0, 8000, "the app's exit");
    expect(done).toBe(1);
  },
  60_000,
);

test.skipIf(!run)(
  "a still, flat screen: a frame a second, the host idle",
  async () => {
    // xterm drawn black on black: every macroblock flat, so refinement finds nothing
    // left to send (which once left the video loop refining again with no wait, a core
    // spinning and the keepalive never sent)
    const v = new Viewer();
    const src = new AfterglowSource({
      link: { port, token },
      target: "",
      argv: [Bun.which("xterm")!, "-bg", "black", "-fg", "black", "-cr", "black"],
      virtualDisplay: true,
    });
    await src.open(opened(640, 360), v);
    src.video({ w: 640, h: 360, fps: 30, kbps: 4000, full: false }, v);
    await until(() => v.frames.length > 0, 5000, "the first frame");
    await settle(1500);
    const cpu = () => {
      const f = readFileSync(`/proc/${host!.pid}/stat`, "utf8").split(") ")[1]!.split(" ");
      return (Number(f[11]) + Number(f[12])) / 100;
    };
    const from = v.frames.length;
    const c0 = cpu();
    await settle(3000);
    const used = cpu() - c0;
    const still = v.frames.length - from;
    console.log(`flat and still for 3 s: ${still} frames, ${used.toFixed(2)} s of the host's CPU`);
    expect(still).toBeGreaterThanOrEqual(2);
    expect(still).toBeLessThanOrEqual(6);
    expect(used).toBeLessThan(0.5);
    src.quit();
    await settle(500);
  },
  30_000,
);

test.skipIf(!run)(
  "the pointer drawn in where it moved, nowhere else",
  async () => {
    const v = new Viewer();
    const src = new AfterglowSource({
      link: { port, token },
      target: Bun.which("xterm")!,
      virtualDisplay: true,
      drawCursor: true,
    });
    await src.open(opened(1280, 720), v);
    src.video({ w: 1280, h: 720, fps: 30, kbps: 4000, full: false }, v);
    await until(() => v.frames.length > 0, 5000, "the first frame");
    await settle(1500);
    expect(v.metas).toContainEqual({ t: "cursor", name: "blank" });
    const moved = v.frames.length;
    src.input(["m", 900, 500], 1280, 720);
    await until(() => v.frames.length > moved, 2000, "a frame of the pointer's move");
    let shot = shotOf(v, 1280, 720);
    expect(count(shot, 1280, 900, 500, 12, (p) => p < 100)).toBeGreaterThan(5);
    expect(count(shot, 1280, 640, 360, 12, (p) => p < 100)).toBe(0);
    // moved again: drawn at the new place, the old one put back
    const again = v.frames.length;
    src.input(["m", 300, 200], 1280, 720);
    await until(() => v.frames.length > again, 2000, "a frame of the second move");
    await settle(100);
    shot = shotOf(v, 1280, 720);
    expect(count(shot, 1280, 300, 200, 12, (p) => p < 100)).toBeGreaterThan(5);
    expect(count(shot, 1280, 900, 500, 12, (p) => p < 100)).toBe(0);
    src.quit();
    await settle(500);
  },
  30_000,
);

test.skipIf(!run)(
  "xterm on the session's own display, then a missing app refused",
  async () => {
    const v = new Viewer();
    const src = new AfterglowSource({
      link: { port, token },
      target: Bun.which("xterm")!,
      virtualDisplay: false,
      drawCursor: true,
    });
    await src.open(opened(1280, 720), v);
    src.video({ w: 640, h: 360, fps: 30, kbps: 2000, full: false }, v);
    await until(() => v.frames.length > 0, 5000, "a frame of the session's display");
    // the session's display isn't resized: its 1280x720 scaled into the video
    expect(v.configs[0]).toMatchObject({ w: 640, h: 360 });
    expect(
      decode(
        annexB(
          v.configs[0]!.avcc,
          v.frames.map((f) => f.avcc),
        ),
        640,
        360,
      ).size,
    ).toBe("640,360");
    src.quit();
    await settle(300);

    const missing = new AfterglowSource({ link: { port, token }, target: "/nonexistent/app", virtualDisplay: false });
    await expect(missing.open(opened(640, 480), new Viewer())).rejects.toThrow("the app didn't start");
  },
  30_000,
);
