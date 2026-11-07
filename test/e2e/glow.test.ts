// glow end to end in a browser: afterglow --stdio runs xterm on a display of its own,
// the driver asks for a ticket, and Chromium comes straight to afterglow over
// WebTransport with it, the certificate pinned by its hash. Frames are decoded by
// WebCodecs as a page would, keys typed into xterm through glow and timed from the key
// to its echo decoded. Again with a tenth, then a quarter, of the frames' datagrams dropped:
// parity and the viewer's asks put every unit back. Runs where AFTERGLOW_BIN names a
// built afterglow, Xvfb and xterm are installed and Playwright's Chromium is at hand
// (`bun run test:e2e`; `GLOW_KEYS=40` for a longer timing)

import { expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { AfterglowSource, type Opened, type Sink, type Transport, type VideoSink } from "../../src/index.ts";

const bin = process.env.AFTERGLOW_BIN ?? "";
const node = Bun.which("node");
const playwright = (() => {
  try {
    return !!node && Bun.spawnSync([node, "-e", "require.resolve('playwright')"]).exitCode === 0;
  } catch {
    return false;
  }
})();
const run = !!bin && existsSync(bin) && !!Bun.which("Xvfb") && !!Bun.which("xterm") && playwright;
const keys = Number(process.env.GLOW_KEYS ?? 12);

function spawnStdio() {
  const p = Bun.spawn([bin, "--stdio"], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "inherit",
    env: { ...process.env, DISPLAY: "", AFTERGLOW_DIRECT_BIND: "127.0.0.1" },
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

class Driver implements Sink, VideoSink {
  says: Record<string, unknown>[] = [];
  frames = 0;
  meta() {}
  say(m: Record<string, unknown>) {
    this.says.push(m);
  }
  exit() {}
  config() {}
  frame() {
    this.frames++;
  }
  ended() {}
}

const opened: Opened = { argv: [], name: "xterm", w: 1280, h: 720, sw: 1280, sh: 720, scale: 1, full: false, dpi: 96 };

type Result = {
  ok: boolean;
  error?: string;
  connected: number;
  firstFrame: number;
  decoded: number;
  lat: number[];
  size: [number, number];
  stats: { rebuilt: number; resent: number; nacks: number; lost: number; rtt: number; units: number };
  log: string[];
};

const median = (a: number[]) => [...a].sort((x, y) => x - y)[a.length >> 1] ?? Number.NaN;

async function browse(url: string): Promise<Result> {
  const p = Bun.spawn([node!, join(import.meta.dir, "glow-browser.cjs"), url], { stdout: "pipe", stderr: "inherit" });
  const out = await new Response(p.stdout).text();
  await p.exited;
  return JSON.parse(out.trim().split("\n").at(-1) ?? "{}");
}

test.skipIf(!run)(
  "a browser straight to afterglow over glow: frames decoded, keys echoed, loss put back",
  async () => {
    const page = await Bun.build({ entrypoints: [join(import.meta.dir, "glow-page.ts")], target: "browser" });
    expect(page.success).toBe(true);
    const js = await page.outputs[0]!.text();
    const server = Bun.serve({
      hostname: "127.0.0.1",
      port: 0,
      fetch: (req) =>
        new URL(req.url).pathname === "/page.js"
          ? new Response(js, { headers: { "content-type": "text/javascript" } })
          : new Response('<!doctype html><script type="module" src="/page.js"></script>', {
              headers: { "content-type": "text/html" },
            }),
    });
    const { p, t } = spawnStdio();
    const d = new Driver();
    const src = new AfterglowSource({ transport: () => t, target: "", argv: ["xterm"], virtualDisplay: true });
    try {
      await src.open(opened, d);
      src.video({ w: 1280, h: 720, fps: 120, kbps: 20_000, full: false }, d);
      expect(src.canDirect).toBe(true);
      for (const drop of [0, 0.1, 0.25]) {
        const tk = await src.direct();
        const ask = { url: `https://127.0.0.1:${tk.port}`, hash: tk.hash, ticket: tk.ticket, keys, drop };
        const r = await browse(`http://127.0.0.1:${server.port}/#${encodeURIComponent(JSON.stringify(ask))}`);
        if (!r.ok) throw new Error(`the page: ${r.error} ${JSON.stringify(r.log)}`);
        const s = r.stats;
        console.log(
          `glow${drop ? `, ${drop * 100}% of datagrams dropped` : ""}: connected in ${r.connected.toFixed(0)} ms, first frame decoded ${r.firstFrame.toFixed(0)} ms after the page opened it; key to echo decoded: median ${median(r.lat).toFixed(1)} ms, min ${Math.min(...r.lat).toFixed(1)}, max ${Math.max(...r.lat).toFixed(1)} (${r.lat.length} keys); ${s.units} units, ${s.rebuilt} pieces rebuilt, ${s.resent} sent again on ${s.nacks} asks, ${s.lost} lost, rtt ${s.rtt} ms`,
        );
        expect(r.size).toEqual([1280, 720]);
        expect(r.lat.length).toBe(keys);
        expect(median(r.lat)).toBeLessThan(100);
        expect(s.lost).toBe(0);
        if (drop) expect(s.rebuilt + s.resent).toBeGreaterThan(0);
        // the driver told the viewer came, and went
        expect(d.says.some((m) => m.t === "direct" && m.on === true)).toBe(true);
      }
      // the viewer gone, the driver told and the frames over the link again
      const end = Date.now() + 5000;
      const gone = () => d.says.filter((m) => m.t === "direct" && m.on === false).length >= 3;
      while (!gone() && Date.now() < end) await Bun.sleep(20);
      expect(gone()).toBe(true);
      const before = d.frames;
      while (d.frames === before && Date.now() < end + 3000) await Bun.sleep(20);
      expect(d.frames).toBeGreaterThan(before);
    } finally {
      src.quit();
      server.stop(true);
      setTimeout(() => p.kill(), 2000);
    }
  },
  90_000,
);
