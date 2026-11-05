// afterglow --stdio driven by AfterglowSource through a byte stream, as phosphorus runs
// it over ssh as the viewer's login: an app by its argv on a display of its own, its
// title and windows as notes, a command typed, the viewer's clipboard pasted by the
// app's own paste, closed as its close button would, gone with its exit code; an app
// that dies before showing a window, its code and output told. Runs where AFTERGLOW_BIN
// names a built afterglow and Xvfb and xterm are installed (`bun run test:e2e`)

import { expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AfterglowSource, type Opened, type Sink, type Transport, type VideoSink } from "../../src/index.ts";

const bin = process.env.AFTERGLOW_BIN ?? "";
const run = !!bin && existsSync(bin) && !!Bun.which("Xvfb") && !!Bun.which("xterm");

const settle = (ms: number) => new Promise((r) => setTimeout(r, ms));
async function until(f: () => boolean, ms: number, what: string) {
  const end = Date.now() + ms;
  while (!f()) {
    if (Date.now() > end) throw new Error(`timed out: ${what}`);
    await settle(20);
  }
}

// `afterglow --stdio` as a child, its stdin and stdout the transport
function spawnStdio() {
  const p = Bun.spawn([bin, "--stdio"], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "inherit",
    env: { ...process.env, DISPLAY: "" },
  });
  const datas: ((b: Uint8Array) => void)[] = [];
  const closes: (() => void)[] = [];
  void (async () => {
    for await (const c of p.stdout) for (const f of datas) f(c);
    for (const f of closes) f();
  })();
  const t: Transport = {
    write: (b) => {
      p.stdin.write(b);
      p.stdin.flush();
    },
    onData: (f) => void datas.push(f),
    onClose: (f) => void closes.push(f),
    close: () => {
      try {
        p.stdin.end();
      } catch {}
    },
  };
  return { p, t };
}

class Viewer implements Sink, VideoSink {
  metas: Record<string, unknown>[] = [];
  says: Record<string, unknown>[] = [];
  exits: { error: string; log?: string; code?: number }[] = [];
  frames = 0;
  meta(m: Record<string, unknown>) {
    this.metas.push(m);
  }
  say(m: Record<string, unknown>) {
    this.says.push(m);
  }
  exit(error: string, log?: string, code?: number) {
    this.exits.push({ error, log, code });
  }
  config() {}
  frame() {
    this.frames++;
  }
  ended() {}
}

const opened: Opened = {
  argv: [],
  name: "xterm",
  w: 800,
  h: 600,
  sw: 800,
  sh: 600,
  scale: 1,
  full: false,
  fps: 30,
  dpi: 96,
};

test.skipIf(!run)(
  "an app by its argv over --stdio: notes, typing, the viewer's paste, a polite close, its exit",
  async () => {
    const dir = mkdtempSync(join(tmpdir(), "afterglow-stdio-"));
    const { p, t } = spawnStdio();
    const v = new Viewer();
    let done = 0;
    const src = new AfterglowSource({
      transport: () => t,
      target: "",
      argv: [Bun.which("xterm")!, "-title", "over stdio", "-e", "sh"],
      virtualDisplay: true,
      done: () => done++,
    });
    await src.open(opened, v);
    src.video({ w: 800, h: 600, fps: 30, kbps: 4000, full: false }, v);
    await until(() => v.frames > 0, 5000, "a frame");
    // the app's own title and its one window, as notes
    await until(() => v.metas.some((m) => m.t === "title" && m.s === "over stdio"), 5000, "the title");
    await until(
      () => v.metas.some((m) => m.t === "windows" && Array.isArray(m.list) && m.list.length === 1),
      5000,
      "the windows",
    );
    await settle(500);

    // a command typed runs, in the viewer's home as the app's working directory
    const typed = join(dir, "typed");
    src.input(["x", `echo typed > ${typed}\n`], 800, 600);
    await until(() => existsSync(typed), 5000, "the typed command");

    // the viewer's clipboard, pasted by the app's own Shift+Insert (PRIMARY)
    const pasted = join(dir, "pasted");
    src.input(["clip", "from the viewer"], 800, 600);
    await settle(200);
    src.input(["x", "echo "], 800, 600);
    src.input(["k", true, "ShiftLeft", "Shift", false], 800, 600);
    src.input(["k", true, "Insert", "Insert", false], 800, 600);
    src.input(["k", false, "Insert", "Insert", false], 800, 600);
    src.input(["k", false, "ShiftLeft", "Shift", false], 800, 600);
    // xterm's paste asks the owner and is answered later: the line finished after it
    await settle(500);
    src.input(["x", ` > ${pasted}\n`], 800, 600);
    await until(() => existsSync(pasted) && readFileSync(pasted, "utf8").length > 0, 5000, "the paste");
    expect(readFileSync(pasted, "utf8")).toBe("from the viewer\n");

    // the close button: xterm asked to delete its window, which ends it
    src.close();
    await until(() => v.exits.length > 0, 8000, "the app's exit");
    expect(v.exits[0]!.error).toBe("");
    expect(done).toBe(1);
    // and afterglow with it, its stdin closed
    const code = await Promise.race([p.exited, settle(5000).then(() => "still running")]);
    expect(code).toBe(0);
    rmSync(dir, { recursive: true, force: true });
  },
  40_000,
);

test.skipIf(!run)(
  "an app that dies before a window: its code, its output and why",
  async () => {
    const { p, t } = spawnStdio();
    const v = new Viewer();
    const src = new AfterglowSource({
      transport: () => t,
      target: "",
      argv: ["/bin/sh", "-c", "echo it went wrong >&2; exit 3"],
      virtualDisplay: true,
    });
    await src.open(opened, v);
    await until(() => v.exits.length > 0, 8000, "the exit");
    expect(v.exits[0]!.code).toBe(3);
    expect(v.exits[0]!.log).toContain("it went wrong");
    expect(v.exits[0]!.error).toContain("before opening a window");
    await Promise.race([p.exited, settle(5000)]);
    expect(p.exitCode).toBe(0);
  },
  20_000,
);

test.skipIf(!run)("a program that isn't there: refused at launch", async () => {
  const { p, t } = spawnStdio();
  const src = new AfterglowSource({
    transport: () => t,
    target: "",
    argv: ["no-such-app-anywhere"],
    virtualDisplay: true,
  });
  await expect(src.open(opened, new Viewer())).rejects.toThrow("isn't on this host's PATH");
  await Promise.race([p.exited, settle(5000)]);
  expect(p.exitCode).toBe(0);
});
