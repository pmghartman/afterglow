// what a glow viewer does with afterglow's datagrams, apart from any socket: pieces put
// together into units (fec.ts), what parity can't rebuild asked for again at once,
// units handed on in order from a keyframe. A unit that won't come (asked for too
// often, or for too long) is given up: the viewer asks for a keyframe and waits for
// one, as a decoder missing a frame must. Time is passed in, so tests run it on their
// own clock

import { Assembly, Flag, Kind, parseHead } from "./fec.ts";

// viewer to afterglow, datagrams (host/src/glow/mod.rs, `up`)
export const Up = { nack: 0x10, key: 0x11, report: 0x12, move: 0x20, ping: 0x21 } as const;

export type ReceiverOut = {
  // a whole unit, in order: the link's messages, as the link would have written them
  unit(bytes: Uint8Array, key: boolean): void;
  // a datagram for afterglow
  send(d: Uint8Array): void;
};

export type ReceiverStats = {
  units: number;
  // pieces rebuilt from parity, pieces that came again asked for, asks made
  rebuilt: number;
  resent: number;
  nacks: number;
  // keyframes asked for, units given up
  keys: number;
  lost: number;
};

const newer = (a: number, b: number) => a !== b && ((a - b) & 0xffff) < 0x8000;
const dist = (a: number, b: number) => (a - b) & 0xffff;

// how far ahead of the unit it waits for a viewer looks for what's missing
const Window = 64;
// a unit asked for this often, or outstanding this long, is given up
const MaxAsks = 5;

export class Receiver {
  // the round trip, ms, as the viewer last measured it
  rtt = 30;
  stats: ReceiverStats = { units: 0, rebuilt: 0, resent: 0, nacks: 0, keys: 0, lost: 0 };
  private units = new Map<number, Assembly>();
  // a unit's asks: the last, how many, the first
  private asked = new Map<number, { at: number; n: number; first: number }>();
  // last piece seen of each unit, and whether all of its parity has come
  private touched = new Map<number, number>();
  private next = -1;
  private newest = -1;
  // waiting for a keyframe: nothing else is handed on
  waitKey = true;
  private keyAt = Number.NEGATIVE_INFINITY;
  // pieces that came fresh, and pieces the units handed on were cut into, lately
  private fresh = 0;
  private expected = 0;
  private reportAt = 0;

  constructor(
    private out: ReceiverOut,
    private now: () => number = () => performance.now(),
  ) {}

  datagram(d: Uint8Array) {
    if (d[0] === Kind.sync && d.length >= 4) {
      const seq = d[2]! | (d[3]! << 8);
      if (this.next >= 0 && !this.waitKey && !newer(this.next, seq) && newer(seq, this.newest)) this.newest = seq;
      return this.check(true);
    }
    const h = parseHead(d);
    if (!h) return;
    const now = this.now();
    if (h.flags & Flag.resent) this.stats.resent++;
    if (this.waitKey) {
      // only a keyframe's unit, the newest one seen
      if (!(h.flags & Flag.key)) return;
      if (this.next < 0 || newer(h.seq, this.next)) {
        this.units.clear();
        this.asked.clear();
        this.touched.clear();
        this.next = h.seq;
        this.newest = h.seq;
      }
      if (h.seq !== this.next) return;
    } else if (newer(this.next, h.seq)) {
      return; // handed on already
    }
    if (newer(h.seq, this.newest)) this.newest = h.seq;
    let a = this.units.get(h.seq);
    if (!a) {
      a = new Assembly(h, now);
      this.units.set(h.seq, a);
    }
    const before = a.rebuilt;
    const later = h.seq !== this.next;
    a.put(h, d);
    this.stats.rebuilt += a.rebuilt - before;
    this.touched.set(h.seq, now);
    this.deliver();
    // a later unit, or this one's last parity, says what's missing before it
    const lastParity = h.kind === Kind.parity && h.idx === h.r - 1;
    if (later || lastParity) this.check(lastParity);
  }

  private deliver() {
    while (this.next >= 0) {
      const a = this.units.get(this.next);
      if (!a?.done) return;
      this.units.delete(this.next);
      this.asked.delete(this.next);
      this.touched.delete(this.next);
      this.fresh += a.fresh;
      this.expected += a.k + a.r;
      this.stats.units++;
      this.waitKey = false;
      this.next = (this.next + 1) & 0xffff;
      this.out.unit(a.buf, a.key);
    }
  }

  // what's missing from the unit waited for up to the newest, asked for; `incl`: the
  // newest too. A unit still getting its pieces is left a moment
  private check(incl: boolean) {
    // nothing seen past what's been handed on
    if (this.next < 0 || newer(this.next, this.newest)) return;
    const now = this.now();
    const retry = Math.max(15, this.rtt * 1.5);
    const giveUp = Math.max(250, this.rtt * 5);
    const ahead = dist(this.newest, this.next);
    if (ahead > 512) return this.lose();
    const span = Math.min(ahead, Window) + (incl ? 1 : 0);
    for (let i = 0; i < span; i++) {
      const s = (this.next + i) & 0xffff;
      const a = this.units.get(s);
      if (a?.done) continue;
      const last = this.touched.get(s);
      if (a && last !== undefined && s === this.newest && now - last < 4 && !incl) continue;
      const ask = this.asked.get(s);
      if (ask && now - ask.at < retry) continue;
      if (ask && (ask.n >= MaxAsks || now - ask.first > giveUp)) return this.lose();
      const idx = a ? a.wanted() : [];
      if (a && !idx.length) continue;
      const d = new Uint8Array(4 + idx.length * 2);
      const v = new DataView(d.buffer);
      d[0] = Up.nack;
      d[1] = a ? 0 : 1;
      v.setUint16(2, s, true);
      idx.forEach((x, j) => {
        v.setUint16(4 + j * 2, x, true);
      });
      this.asked.set(s, { at: now, n: (ask?.n ?? 0) + 1, first: ask?.first ?? now });
      this.stats.nacks++;
      this.out.send(d);
    }
  }

  // the unit waited for won't come: everything after it is useless to a decoder until
  // a keyframe, so wait for one and ask for it now
  private lose() {
    this.stats.lost++;
    this.units.clear();
    this.asked.clear();
    this.touched.clear();
    this.next = -1;
    this.waitKey = true;
    this.keyAt = Number.NEGATIVE_INFINITY;
    this.tick();
  }

  // a keyframe asked for (the decoder failed, say)
  wantKey() {
    if (!this.waitKey) this.lose();
  }

  // on a clock of a few ms: asks repeated, a keyframe asked again while waiting, the
  // loss told to afterglow twice a second
  tick() {
    const now = this.now();
    // waiting with no keyframe on its way: asked for again now and then
    if (this.waitKey && this.next < 0 && now - this.keyAt > Math.max(60, this.rtt * 2)) {
      this.keyAt = now;
      this.stats.keys++;
      this.out.send(new Uint8Array([Up.key]));
    }
    this.check(true);
    if (now - this.reportAt >= 500) {
      this.reportAt = now;
      if (this.expected > 0) {
        const loss = Math.max(0, Math.min(1, 1 - this.fresh / this.expected));
        const d = new Uint8Array([Up.report, 0, 0, 0]);
        new DataView(d.buffer).setUint16(2, Math.round(loss * 1000), true);
        this.out.send(d);
      }
      this.fresh = this.expected = 0;
    }
  }
}
