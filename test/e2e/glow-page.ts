// the page glow.test.ts opens in Chromium: a viewer come straight to afterglow with the
// ticket in its hash, each frame decoded by WebCodecs as a page would, then keys typed
// into the app one at a time and timed from the key to the frame showing its echo
// decoded. What it found goes on window.result

import { codecOf, GlowViewer } from "../../src/glow/index.ts";

type Ask = { url: string; hash: string; ticket: string; keys: number; drop?: number };
const ask = JSON.parse(decodeURIComponent(location.hash.slice(1))) as Ask;
const w = window as unknown as { result?: unknown; log: string[] };
w.log = [];

let decoded = 0;
let lastDecoded = 0;
let configured = false;
let ts = 0;
const t0 = performance.now();
let firstFrame = 0;
let w0 = 0;
let h0 = 0;
const dec = new VideoDecoder({
  output: (f) => {
    decoded++;
    lastDecoded = performance.now();
    if (!firstFrame) firstFrame = lastDecoded - t0;
    w0 = f.displayWidth;
    h0 = f.displayHeight;
    f.close();
  },
  error: (e) => {
    w.log.push(`decoder: ${e.message}`);
    viewer.keyframe();
  },
});

const viewer = new GlowViewer(
  {
    config: (avcc, cw, ch) => {
      dec.configure({
        codec: codecOf(avcc),
        description: avcc,
        codedWidth: cw,
        codedHeight: ch,
        optimizeForLatency: true,
      });
      configured = true;
    },
    frame: (key, msg) => {
      if (!configured) return;
      dec.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: ts++, data: msg.subarray(8) }));
    },
    closed: (why) => w.log.push(`closed: ${why}`),
  },
  { dropForTest: ask.drop },
);

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function main() {
  await viewer.connect({ url: ask.url, hash: ask.hash, ticket: ask.ticket });
  const connected = performance.now() - t0;
  while (!decoded) {
    if (performance.now() - t0 > 10_000) throw new Error("no frame decoded");
    await sleep(5);
  }
  // the first frames settled, the screen still
  await sleep(800);
  const lat: number[] = [];
  const keys = "afterglow".repeat(8).slice(0, ask.keys);
  for (const c of keys) {
    const before = decoded;
    const at = performance.now();
    viewer.input(["k", true, `Key${c.toUpperCase()}`, c, false], 1280, 720);
    viewer.input(["k", false, `Key${c.toUpperCase()}`, c, false], 1280, 720);
    while (decoded === before && performance.now() - at < 2000) await new Promise((r) => setTimeout(r, 0));
    if (decoded > before) lat.push(lastDecoded - at);
    // the echo's refinement done before the next key
    await sleep(250);
  }
  const stats = viewer.stats;
  viewer.close();
  w.result = { ok: true, connected, firstFrame, decoded, lat, size: [w0, h0], stats, log: w.log };
}

main().catch((e) => {
  w.result = { ok: false, error: String(e?.message ?? e), log: w.log, stats: viewer.stats };
});
