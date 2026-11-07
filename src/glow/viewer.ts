// a viewer come straight to afterglow over glow (docs/glow.md): WebTransport to the
// host, its certificate pinned by the hash the driver handed over with a one-time
// ticket. Frames arrive as datagrams, put together and handed on in order
// (receiver.ts) as the link's own config and frame messages, so whatever drew the
// link's frames draws these; the pointer's shape comes on the control stream. Input
// goes back the same way: pointer moves as datagrams (the newest wins, none waits
// behind a lost one), the last of a gesture and everything else on the control stream,
// in order; the viewer's asks for pieces and keyframes as datagrams. For a page or its
// worker: no Bun, no DOM beyond WebTransport

import { InputPacker } from "../afterglow/input.ts";
import { cursorOf, In, jsonOf, LinkReader, message, Out } from "../afterglow/link.ts";
import { Kind } from "./fec.ts";
import { Receiver, type ReceiverStats, Up } from "./receiver.ts";

// the welcome, first on the control stream
export const Welcome = 0x8b;

// what the driver got from Link.direct, and where the viewer reaches the host's port
export type GlowTicket = {
  // https://<host>:<port>, the port afterglow's (or one forwarded to it)
  url: string;
  // the certificate's SHA-256, hex
  hash: string;
  ticket: string;
};

export type GlowEvents = {
  // a video's parameter sets: decoder.configure({ codec: codecOf(avcc), description: avcc })
  config(avcc: Uint8Array, w: number, h: number, gen: number): void;
  // a frame: msg is the link's frame message, its first 8 bytes room, AVCC after them
  frame(key: boolean, msg: Uint8Array, gen: number): void;
  // the pointer's shape (a PNG, straight alpha) and hotspot, or hidden
  cursor?(c: { visible: boolean; x: number; y: number; w: number; h: number; png: Uint8Array }): void;
  closed?(why: string): void;
};

// rtt: the round trip, ms
export type GlowStats = ReceiverStats & { rtt: number; frames: number; bytes: number };

export type GlowOptions = {
  // what the app can be asked (the launch's features): with "clipboard" the viewer's
  // clipboard is the app's own; without, it's typed at the next Ctrl+V
  features?: string[];
  // tests: this fraction of the frames' datagrams that come fresh dropped on arrival, at
  // random (seeded, so a run repeats), as a lossy path would
  dropForTest?: number;
};

const hexBytes = (hex: string) => {
  const b = new Uint8Array(hex.length >> 1);
  for (let i = 0; i < b.length; i++) b[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return b;
};

// an input packet that moves the pointer to a point: kind 5 behind its size
const isMove = (p: Uint8Array) => p.length >= 8 && p[4] === 0x05 && p[5] === 0 && p[6] === 0 && p[7] === 0;

export class GlowViewer {
  static supported(): boolean {
    return typeof WebTransport !== "undefined";
  }

  private wt?: WebTransport;
  private dgw?: WritableStreamDefaultWriter<Uint8Array>;
  private ctl?: WritableStreamDefaultWriter<Uint8Array>;
  private rx: Receiver;
  private gen = -1;
  private closedWhy?: string;
  private timer?: ReturnType<typeof setInterval>;
  private pingAt = 0;
  private pingN = 0;
  private pings = new Map<number, number>();
  private frames = 0;
  private bytes = 0;
  private features: Set<string>;
  private drop: number;
  private seed = 12345;
  private keys: InputPacker;
  // the pointer: its moves numbered, the last one, and whether the control stream has it
  private moveSeq = 0;
  private lastMove?: Uint8Array;
  private moveOnStream = true;
  private settle?: ReturnType<typeof setTimeout>;
  private rttSeen = false;
  welcome?: Record<string, unknown>;

  constructor(
    private ev: GlowEvents,
    opts: GlowOptions = {},
  ) {
    this.features = new Set(opts.features ?? []);
    this.drop = Math.max(0, Math.min(1, opts.dropForTest ?? 0));
    this.rx = new Receiver({ unit: (u) => this.unit(u), send: (d) => this.datagram(d) });
    this.keys = new InputPacker((p) => this.packet(p));
  }

  // connected, the control stream open; throws when it can't be
  async connect(t: GlowTicket, timeoutMs = 5000): Promise<void> {
    const url = `${t.url.replace(/\/+$/, "")}/${t.ticket}`;
    const wt = new WebTransport(url, {
      serverCertificateHashes: [{ algorithm: "sha-256", value: hexBytes(t.hash) }],
      requireUnreliable: true,
      congestionControl: "low-latency",
    } as WebTransportOptions);
    this.wt = wt;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const late = new Promise<never>((_, rej) => {
      timer = setTimeout(() => rej(new Error("glow didn't connect in time")), timeoutMs);
    });
    try {
      await Promise.race([wt.ready, late]);
    } catch (e) {
      clearTimeout(timer);
      try {
        wt.close();
      } catch {}
      throw e instanceof Error ? e : new Error(String(e));
    }
    clearTimeout(timer);
    wt.closed.then(
      (i) => this.end(i?.reason || "closed"),
      (e) => this.end(String(e?.message ?? e)),
    );
    this.dgw = wt.datagrams.writable.getWriter();
    void this.readDatagrams(wt.datagrams.readable.getReader());
    const bidi = await wt.createBidirectionalStream();
    this.ctl = bidi.writable.getWriter();
    // what opens the stream at the host: a ping, its pong the first round trip
    this.ping();
    void this.readControl(bidi.readable.getReader());
    this.timer = setInterval(() => {
      this.rx.tick();
      if (performance.now() - this.pingAt > 1000) this.ping();
    }, 5);
  }

  // the round trip, on the control stream
  private ping() {
    this.pingAt = performance.now();
    this.pingN = (this.pingN + 1) >>> 0;
    this.pings.set(this.pingN, this.pingAt);
    if (this.pings.size > 8) this.pings.delete(this.pings.keys().next().value!);
    const b = new Uint8Array(4);
    new DataView(b.buffer).setUint32(0, this.pingN, true);
    this.control(In.ping, b);
  }

  private async readDatagrams(r: ReadableStreamDefaultReader<Uint8Array>) {
    try {
      for (;;) {
        const { value, done } = await r.read();
        if (done) return;
        if (this.drop && (value[0] === 1 || value[0] === 2) && !(value[1]! & 2) && this.lose()) continue;
        this.rx.datagram(value);
      }
    } catch (e) {
      this.end(String((e as Error)?.message ?? e));
    }
  }

  private async readControl(r: ReadableStreamDefaultReader<Uint8Array>) {
    const reader = new LinkReader((m) => {
      if (m[0] === Welcome) this.welcome = jsonOf(m);
      else if (m[0] === Out.pong && m.length >= 5) {
        const n = new DataView(m.buffer, m.byteOffset + 1).getUint32(0, true);
        const at = this.pings.get(n);
        this.pings.delete(n);
        if (at !== undefined) {
          const ms = performance.now() - at;
          this.rx.rtt = this.rttSeen ? this.rx.rtt * 0.75 + ms * 0.25 : ms;
          this.rttSeen = true;
        }
      } else if (m[0] === Out.cursor) this.ev.cursor?.(cursorOf(m));
    });
    try {
      for (;;) {
        const { value, done } = await r.read();
        if (done) return this.end("the control stream ended");
        reader.feed(value);
        if (reader.broken) return this.end("a message past the limit");
      }
    } catch (e) {
      this.end(String((e as Error)?.message ?? e));
    }
  }

  // one whole unit: the link's messages, each [u32 len][type][body]
  private unit(u: Uint8Array) {
    const v = new DataView(u.buffer, u.byteOffset, u.byteLength);
    for (let at = 0; at + 5 <= u.length; ) {
      const len = v.getUint32(at, true);
      const m = u.subarray(at + 4, at + 4 + len);
      at += 4 + len;
      if (m[0] === Out.config && m.length >= 8) {
        this.gen = m[2]! | (m[3]! << 8);
        this.ev.config(m.subarray(8), m[4]! | (m[5]! << 8), m[6]! | (m[7]! << 8), this.gen);
      } else if (m[0] === Out.frame && m.length > 8) {
        const gen = m[2]! | (m[3]! << 8);
        if (gen !== this.gen) continue;
        this.frames++;
        this.bytes += m.length;
        this.ev.frame(m[1] === 1, m, gen);
      }
    }
  }

  private lose() {
    this.seed = (this.seed * 1103515245 + 12345) & 0x7fffffff;
    return this.seed / 0x7fffffff < this.drop;
  }

  private datagram(d: Uint8Array) {
    this.dgw?.write(d).catch(() => {});
  }

  private control(type: number, body?: Uint8Array | string) {
    this.ctl?.write(message(type, body)).catch(() => {});
  }

  // an input packet out: a move as a datagram, the rest on the control stream after
  // the last move (so a click lands where the pointer was). A move is numbered: the
  // host drops one older than a move it has applied
  private packet(p: Uint8Array) {
    if (isMove(p)) {
      this.moveSeq = (this.moveSeq + 1) & 0xffff;
      const d = new Uint8Array(4 + p.length);
      d[0] = Up.move;
      d[2] = this.moveSeq & 0xff;
      d[3] = this.moveSeq >> 8;
      d.set(p, 4);
      this.lastMove = d;
      this.moveOnStream = false;
      this.datagram(d);
      // the last of a gesture on the stream too, should its datagram be lost
      clearTimeout(this.settle);
      this.settle = setTimeout(() => this.moveToStream(), 40);
      return;
    }
    this.moveToStream();
    this.control(In.input, p);
  }

  // the last move as a control message: the host takes any datagram of the viewer's as
  // a message of its type, the datagram after its first byte its body
  private moveToStream() {
    if (this.moveOnStream || !this.lastMove) return;
    this.moveOnStream = true;
    this.control(Up.move, this.lastMove.subarray(1));
  }

  // one input message as the link's driver takes it (`["m", x, y]`, `["k", ...]`); w, h
  // the video's size
  input(m: unknown[], w: number, h: number) {
    if (m[0] === "clip" && this.features.has("clipboard")) {
      return this.control(In.clip, String(m[1] ?? "").slice(0, 256 << 10));
    }
    if (m[0] === "raise") return;
    this.keys.input(m, w, h);
  }

  // the next frame a keyframe: the decoder lost its place
  keyframe() {
    this.rx.wantKey();
  }

  get stats(): GlowStats {
    return { ...this.rx.stats, rtt: Math.round(this.rx.rtt * 10) / 10, frames: this.frames, bytes: this.bytes };
  }

  get open() {
    return !!this.wt && !this.closedWhy;
  }

  private end(why: string) {
    if (this.closedWhy) return;
    this.closedWhy = why;
    clearInterval(this.timer);
    clearTimeout(this.settle);
    this.ev.closed?.(why);
  }

  close() {
    this.end("closed");
    try {
      this.wt?.close();
    } catch {}
  }
}
