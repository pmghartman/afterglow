// afterglow's local link: the framing against the layout host/src/wire.rs writes,
// and AfterglowSource driven directly against a stand-in afterglow on a real loopback
// socket: the token said first, the app launched, frames handed on with their room in
// front, an earlier video's frames dropped, a keyframe asked for without a restart,
// input as GameStream packets, the app's exit

import { afterEach, describe, expect, test } from "bun:test";
import {
  AfterglowSource,
  FrameRoom,
  In,
  LinkReader,
  linkMessage,
  type Opened,
  Out,
  type Sink,
  type VideoSink,
} from "../src/index.ts";
import { configMessage, FakeAfterglow, frameMessage, jsonMessage } from "../src/testing.ts";

describe("link framing", () => {
  test("messages out of a stream cut anywhere, each its own array", () => {
    const a = jsonMessage(Out.ready, { version: "1" });
    const b = frameMessage(3, true, [0, 0, 0, 1, 0x65]);
    const all = new Uint8Array([...a, ...b, ...a]);
    for (const cut of [1, 3, 7, all.length]) {
      const got: Uint8Array[] = [];
      const r = new LinkReader((m) => got.push(m));
      for (let i = 0; i < all.length; i += cut) r.feed(all.subarray(i, i + cut));
      expect(got.map((m) => m[0])).toEqual([Out.ready, Out.frame, Out.ready]);
      expect(Array.from(got[1]!)).toEqual([Out.frame, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0x65]);
      expect(got[1]!.byteOffset).toBe(0);
    }
  });
  test("a length past the limit breaks the link", () => {
    const r = new LinkReader(() => {});
    r.feed(new Uint8Array([0xff, 0xff, 0xff, 0xff, 1]));
    expect(r.broken).toBe(true);
  });
  test("a message bigger than the buffer", () => {
    const big = new Uint8Array(3 << 20).fill(7);
    const m = linkMessage(Out.frame, big);
    const got: Uint8Array[] = [];
    const r = new LinkReader((x) => got.push(x));
    for (let i = 0; i < m.length; i += 65536) r.feed(m.subarray(i, i + 65536));
    expect(got.length).toBe(1);
    expect(got[0]!.length).toBe(1 + big.length);
  });
});

// what a driver would be handed: the source's metadata, its video, its end
class Driver implements Sink, VideoSink {
  metas: Record<string, unknown>[] = [];
  exits: string[] = [];
  configs: { avcc: number[]; w: number; h: number }[] = [];
  frames: { key: boolean; data: Uint8Array; room?: boolean }[] = [];
  ends: boolean[] = [];
  meta(m: { t: string } & Record<string, unknown>) {
    this.metas.push(m);
  }
  say() {}
  exit(error: string) {
    this.exits.push(error);
  }
  config(avcc: Uint8Array, w: number, h: number) {
    this.configs.push({ avcc: Array.from(avcc), w, h });
  }
  frame(key: boolean, data: Uint8Array, room?: boolean) {
    this.frames.push({ key, data, room });
  }
  ended(got: boolean) {
    this.ends.push(got);
  }
}

const opened = (w: number, h: number): Opened => ({
  argv: [],
  name: "App",
  w,
  h,
  sw: 1920,
  sh: 1080,
  scale: 1,
  full: false,
  fps: 60,
  dpi: 96,
});
const settle = (ms = 30) => new Promise((r) => setTimeout(r, ms));
const bodyJson = (b: Uint8Array) => JSON.parse(new TextDecoder().decode(b));

let fake: FakeAfterglow | undefined;
afterEach(() => fake?.stop());

describe("afterglow source", () => {
  test("launch, frames with their room in place, gen filtering, a keyframe asked, input, exit", async () => {
    fake = new FakeAfterglow("t0ken-t0ken-t0ken-1", (f, type, body) => {
      if (type === In.launch) f.send(jsonMessage(Out.launched, { ok: true, resumed: false }));
      if (type === In.video) {
        const j = bodyJson(body);
        f.send(configMessage(j.gen, j.w, j.h, [1, 0x64, 0, 0x1f]));
        f.send(frameMessage(j.gen, true, [0, 0, 0, 2, 0x65, 0x88]));
        // a frame of a video already given up: dropped
        f.send(frameMessage(j.gen - 1, false, [0, 0, 0, 1, 0x41]));
      }
    });
    let done = 0;
    const src = new AfterglowSource({
      link: { port: fake.port, token: fake.token },
      target: "C:\\Users\\me\\Desktop\\App.lnk",
      virtualDisplay: true,
      done: () => done++,
    });
    const d = new Driver();
    await src.open(opened(1280, 720), d);
    expect(fake.hello).toBe(fake.token);
    expect(bodyJson(fake.sent(In.launch)[0]!.body)).toMatchObject({
      cmd: "C:\\Users\\me\\Desktop\\App.lnk",
      w: 1280,
      h: 720,
      fps: 60,
      virtual_display: true,
    });
    expect(d.metas).toContainEqual({ t: "title", s: "App" });

    // the open's video taken up as it is, a keyframe asked rather than a second video
    src.video({ w: 1280, h: 720, fps: 60, kbps: 20_000, full: false }, d);
    await settle();
    expect(fake.sent(In.video).length).toBe(1);
    expect(fake.sent(In.idr).length).toBe(1);
    expect(d.configs).toEqual([{ avcc: [1, 0x64, 0, 0x1f], w: 1280, h: 720 }]);
    expect(d.frames.length).toBe(1);
    expect(d.frames[0]!.key).toBe(true);
    expect(d.frames[0]!.room).toBe(true);
    expect(Array.from(d.frames[0]!.data.subarray(FrameRoom))).toEqual([0, 0, 0, 2, 0x65, 0x88]);

    // input: GameStream packets, a mouse move at the video's size
    src.input(["m", 640, 360], 1280, 720);
    src.input(["k", true, "KeyA", "a", false], 1280, 720);
    await settle();
    const inputs = fake.sent(In.input).map((g) => g.body);
    expect(inputs.length).toBe(2);
    const mv = new DataView(inputs[0]!.buffer, inputs[0]!.byteOffset);
    expect(mv.getUint32(4, true)).toBe(0x05);
    expect(mv.getInt16(8, false)).toBe(640);
    expect(mv.getInt16(14, false)).toBe(1279);

    // a keyframe asked: the video not restarted
    src.keyframe();
    await settle();
    expect(fake.sent(In.idr).length).toBe(2);
    expect(fake.sent(In.video).length).toBe(1);

    // the app quits on the host
    fake.send(jsonMessage(Out.exited, {}));
    await settle();
    expect(d.exits).toEqual([""]);
    expect(done).toBe(1);
  });

  test("a launch refused: the open fails with afterglow's reason", async () => {
    fake = new FakeAfterglow("t0ken-t0ken-t0ken-2", (f, type) => {
      if (type === In.launch) f.send(jsonMessage(Out.launched, { ok: false, error: "not an app this host can start" }));
    });
    const src = new AfterglowSource({
      link: { port: fake.port, token: fake.token },
      target: "C:\\x.lnk",
      virtualDisplay: false,
    });
    await expect(src.open(opened(640, 480), new Driver())).rejects.toThrow("not an app this host can start");
  });

  test("a wrong token: no answer, the open times out", async () => {
    fake = new FakeAfterglow("the-right-token-xx", () => {});
    const src = new AfterglowSource({
      link: { port: fake.port, token: "the-wrong-token-xx" },
      target: "C:\\x.lnk",
      virtualDisplay: false,
    });
    await expect(src.open(opened(640, 480), new Driver())).rejects.toThrow();
  }, 10_000);

  test("a new size: a fresh video, the old one's late frames dropped", async () => {
    let lastGen = 0;
    fake = new FakeAfterglow("t0ken-t0ken-t0ken-3", (f, type, body) => {
      if (type === In.launch) f.send(jsonMessage(Out.launched, { ok: true }));
      if (type === In.video) {
        const j = bodyJson(body);
        // the video before's frame still in flight
        if (lastGen) f.send(frameMessage(lastGen, true, [0, 0, 0, 1, 9]));
        lastGen = j.gen;
        f.send(configMessage(j.gen, j.w, j.h, [1, 2, 3]));
        f.send(frameMessage(j.gen, true, [0, 0, 0, 1, 0x65]));
      }
    });
    const src = new AfterglowSource({
      link: { port: fake.port, token: fake.token },
      target: "C:\\x.lnk",
      virtualDisplay: true,
    });
    const first = new Driver();
    await src.open(opened(800, 600), first);
    src.video({ w: 800, h: 600, fps: 60, kbps: 20_000, full: false }, first);
    await settle();
    const next = new Driver();
    src.video({ w: 1024, h: 768, fps: 60, kbps: 20_000, full: false }, next);
    await settle();
    expect(bodyJson(fake.sent(In.video).at(-1)!.body)).toMatchObject({ w: 1024, h: 768 });
    expect(next.configs).toEqual([{ avcc: [1, 2, 3], w: 1024, h: 768 }]);
    expect(next.frames.length).toBe(1);
    expect(next.frames[0]!.data[FrameRoom + 4]).toBe(0x65);
    src.quit();
  });
});
