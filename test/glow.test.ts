// glow's viewer end, on its own clock: units cut as afterglow cuts them (fec.ts mirrors
// host/src/glow/fec.rs) put back together through loss, what parity can't rebuild asked
// for and sent again by a stand-in host, units handed on in order from a keyframe, a
// unit that won't come given up for a keyframe. And the driver's ask for a ticket

import { describe, expect, test } from "bun:test";
import { Assembly, cut, Flag, HeadLen, parityCount, parseHead, Receiver, Up } from "../src/glow/index.ts";
import { AfterglowSource, In, Link, Out } from "../src/index.ts";
import { FakeAfterglow, jsonMessage } from "../src/testing.ts";

const unit = (n: number, salt = 0) => Uint8Array.from({ length: n }, (_, i) => (i * 7 + (i >> 8) + salt) & 0xff);

// a seeded generator, so a lossy run is the same every time
const rng = (seed: number) => () => {
  seed = (seed * 1103515245 + 12345) & 0x7fffffff;
  return seed / 0x7fffffff;
};

describe("glow's pieces", () => {
  test("a head read as the host writes it; nonsense refused", () => {
    const ds = cut(unit(3000), 1000, 9, Flag.key, 2);
    expect(parseHead(ds[1]!)).toEqual({ kind: 1, flags: 1, seq: 9, idx: 1, k: 3, r: 2, len: 3000 });
    expect(parseHead(ds[4]!)!.kind).toBe(2);
    expect(parseHead(ds[0]!.subarray(0, 13))).toBeUndefined();
    const bad = ds[0]!.slice();
    bad[4] = 3;
    expect(parseHead(bad)).toBeUndefined();
  });

  test("parity as the host reckons it", () => {
    expect([parityCount(1, 0), parityCount(4, 0.3), parityCount(32, 0), parityCount(128, 0.05)]).toEqual([1, 1, 2, 28]);
  });

  test("bursts as long as the parity rebuilt; past it, just what's missing asked for", () => {
    const u = unit(40_000);
    const ds = cut(u, 1186, 0, 0, 4);
    const k = ds.length - 4;
    for (const start of [0, 7, k - 4]) {
      const a = new Assembly(parseHead(ds[0]!)!);
      ds.forEach((d, n) => {
        if (n < start || n >= start + 4) a.put(parseHead(d)!, d);
      });
      expect(a.done).toBe(true);
      expect(a.buf).toEqual(u);
    }
    const small = cut(unit(10_000), 1000, 0, 0, 2);
    const a = new Assembly(parseHead(small[0]!)!);
    small.forEach((d, n) => {
      if (![0, 2, 4].includes(n)) a.put(parseHead(d)!, d);
    });
    expect(a.done).toBe(false);
    expect(a.wanted()).toEqual([2, 4]);
    for (const i of a.wanted()) a.put(parseHead(small[i]!)!, small[i]!);
    expect(a.done).toBe(true);
    expect(a.buf).toEqual(unit(10_000));
  });

  test("a one-piece unit lost comes back from its copy", () => {
    const ds = cut(unit(300), 1186, 0, 0);
    expect(ds.length).toBe(2);
    const a = new Assembly(parseHead(ds[1]!)!);
    a.put(parseHead(ds[1]!)!, ds[1]!);
    expect(a.done).toBe(true);
    expect(a.buf).toEqual(unit(300));
    expect(HeadLen).toBe(14);
  });
});

// a stand-in host: units cut and kept, NACKs answered from what's kept (as long as it's
// kept), datagrams through a lossy path to the receiver
class Host {
  seq = 0;
  kept = new Map<number, Uint8Array[]>();
  keys = 0;
  sentAgain = 0;
  constructor(
    private path: (d: Uint8Array) => void,
    private keep = 256,
  ) {}
  send(u: Uint8Array, key: boolean) {
    const ds = cut(u, 1186, this.seq, key ? Flag.key : 0);
    this.kept.set(this.seq, ds);
    this.kept.delete((this.seq - this.keep) & 0xffff);
    this.seq = (this.seq + 1) & 0xffff;
    for (const d of ds) this.path(d);
  }
  up(d: Uint8Array) {
    if (d[0] === Up.key) this.keys++;
    if (d[0] !== Up.nack) return;
    const v = new DataView(d.buffer, d.byteOffset);
    const ds = this.kept.get(v.getUint16(2, true));
    if (!ds) return;
    const k = parseHead(ds[0]!)!.k;
    const idx =
      d[1]! & 1
        ? [...Array(k).keys()]
        : [...Array((d.length - 4) >> 1).keys()].map((j) => v.getUint16(4 + j * 2, true));
    for (const i of idx) {
      const again = ds[i]!.slice();
      again[1] = again[1]! | Flag.resent;
      this.sentAgain++;
      this.path(again);
    }
  }
}

describe("glow's receiver", () => {
  test("in order and whole through a fifth of the datagrams lost, from the first keyframe", () => {
    let t = 0;
    const got: { bytes: Uint8Array; key: boolean }[] = [];
    const random = rng(7);
    let host: Host;
    const rx = new Receiver(
      { unit: (b, key) => got.push({ bytes: b.slice(), key }), send: (d) => host.up(d) },
      () => t,
    );
    // fresh datagrams lost a fifth of the time; those sent again a fifth too
    host = new Host((d) => {
      if (random() >= 0.2) rx.datagram(d);
    });
    // a delta before any keyframe: not handed on
    host.send(unit(500, 99), false);
    const sizes = [20_000, 300, 3000, 300, 12_000, 300, 300, 5000];
    const want: Uint8Array[] = [];
    for (let n = 0; n < 200; n++) {
      const u = unit(sizes[n % sizes.length]!, n);
      want.push(u);
      host.send(u, n === 0);
      for (let i = 0; i < 4; i++) {
        t += 2;
        rx.tick();
      }
    }
    for (let i = 0; i < 200; i++) {
      t += 5;
      rx.tick();
    }
    expect(got.length).toBe(200);
    expect(got[0]!.key).toBe(true);
    got.forEach((g, i) => {
      expect(g.bytes).toEqual(want[i]!);
    });
    expect(rx.stats.lost).toBe(0);
    expect(rx.stats.rebuilt).toBeGreaterThan(50);
    expect(rx.stats.nacks).toBeGreaterThan(10);
  });

  test("a unit that can't be had is given up: a keyframe asked for, and taken up from it", () => {
    let t = 0;
    const got: boolean[] = [];
    let host: Host;
    let lose = false;
    const rx = new Receiver({ unit: (_b, key) => got.push(key), send: (d) => host.up(d) }, () => t);
    host = new Host((d) => {
      if (!lose) rx.datagram(d);
    }, 1);
    host.send(unit(3000), true);
    host.send(unit(3000, 1), false);
    expect(got).toEqual([true, false]);
    // one lost whole, and the host keeps nothing to send again
    lose = true;
    host.send(unit(3000, 2), false);
    host.kept.clear();
    lose = false;
    host.send(unit(3000, 3), false);
    for (let i = 0; i < 100; i++) {
      t += 5;
      rx.tick();
    }
    expect(rx.stats.lost).toBe(1);
    expect(rx.waitKey).toBe(true);
    expect(host.keys).toBeGreaterThan(0);
    // deltas aren't taken while it waits; the keyframe is
    host.send(unit(3000, 4), false);
    host.send(unit(3000, 5), true);
    host.send(unit(3000, 6), false);
    expect(got).toEqual([true, false, true, false]);
  });

  test("the loss told to the host", () => {
    let t = 0;
    const sent: Uint8Array[] = [];
    const rx = new Receiver({ unit: () => {}, send: (d) => sent.push(d) }, () => t);
    const ds = cut(unit(20_000), 1186, 0, Flag.key, 2);
    ds.forEach((d, n) => {
      if (n !== 3) rx.datagram(d);
    });
    t += 600;
    rx.tick();
    const report = sent.find((d) => d[0] === Up.report)!;
    expect(new DataView(report.buffer).getUint16(2, true)).toBe(Math.round((1 / ds.length) * 1000));
  });
});

describe("a ticket for glow", () => {
  test("asked of the link, matched by id; refused where there's no glow", async () => {
    const fake = new FakeAfterglow("t0ken-t0ken-t0ken-9", (f, type, body) => {
      const j = body.length ? JSON.parse(new TextDecoder().decode(body)) : {};
      if (type === In.launch) f.send(jsonMessage(Out.launched, { ok: true, features: ["notes"] }));
      if (type === In.direct)
        f.send(
          jsonMessage(Out.direct, {
            ok: true,
            id: j.id,
            port: j.port ?? 47101,
            hash: "ab".repeat(32),
            ticket: "cd".repeat(16),
            expires_ms: 60_000,
          }),
        );
    });
    fake.readyExtra = { direct: true };
    const link = new Link({ port: fake.port, token: fake.token }, { message: () => {}, closed: () => {} });
    expect((await link.open()).direct).toBe(true);
    expect(await link.direct(50_000)).toEqual({
      port: 50_000,
      hash: "ab".repeat(32),
      ticket: "cd".repeat(16),
      expiresMs: 60_000,
    });
    link.close();

    const src = new AfterglowSource({
      link: { port: fake.port, token: fake.token },
      target: "x",
      virtualDisplay: true,
    });
    const sink = { meta: () => {}, say: () => {}, exit: () => {} };
    await src.open({ argv: [], name: "x", w: 640, h: 480, sw: 640, sh: 480, scale: 1, full: false, dpi: 96 }, sink);
    expect(src.canDirect).toBe(true);
    expect(await src.direct()).toMatchObject({ port: 47101, features: ["notes"] });
    src.quit();
    fake.readyExtra = {};
    const old = new AfterglowSource({
      link: { port: fake.port, token: fake.token },
      target: "x",
      virtualDisplay: true,
    });
    await old.open({ argv: [], name: "x", w: 640, h: 480, sw: 640, sh: 480, scale: 1, full: false, dpi: 96 }, sink);
    await expect(old.direct()).rejects.toThrow("no glow");
    old.quit();
    fake.stop();
  });
});
