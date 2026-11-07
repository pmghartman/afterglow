// a glow unit's datagrams (docs/glow.md), the viewer's side of host/src/glow/fec.rs: a
// 14-byte head, then a piece. k data pieces of p = ceil(len / k) bytes (the last
// shorter), then r parity pieces of p bytes, parity j the XOR of every data piece i
// with i % r == j. A class missing one piece, its parity here, is rebuilt where it
// lands: no round trip. No DOM and no Bun here: it runs in a page's worker and in tests

export const Kind = { data: 0x01, parity: 0x02, sync: 0x04, pong: 0x05 } as const;
export const Flag = { key: 1, resent: 2 } as const;
export const HeadLen = 14;

export type Head = { kind: number; flags: number; seq: number; idx: number; k: number; r: number; len: number };

export function parseHead(d: Uint8Array): Head | undefined {
  if (d.length < HeadLen || (d[0] !== Kind.data && d[0] !== Kind.parity)) return;
  const v = new DataView(d.buffer, d.byteOffset, d.byteLength);
  const h = {
    kind: d[0]!,
    flags: d[1]!,
    seq: v.getUint16(2, true),
    idx: v.getUint16(4, true),
    k: v.getUint16(6, true),
    r: v.getUint16(8, true),
    len: v.getUint32(10, true),
  };
  const ok =
    h.k > 0 && h.r > 0 && h.idx < (h.kind === Kind.data ? h.k : h.r) && h.len >= h.k && Math.ceil(h.len / h.k) <= 65535;
  return ok ? h : undefined;
}

// a data piece's bytes within the unit
const range = (len: number, p: number, i: number): [number, number] => [
  Math.min(i * p, len),
  Math.min((i + 1) * p, len),
];

// one unit put back together from what arrives
export class Assembly {
  readonly flags: number;
  readonly k: number;
  readonly r: number;
  readonly len: number;
  readonly p: number;
  readonly buf: Uint8Array;
  private have: Uint8Array;
  private parity: (Uint8Array | undefined)[];
  private missing: number;
  // pieces that came fresh (not sent again), and pieces rebuilt from parity
  fresh = 0;
  rebuilt = 0;
  readonly born: number;

  constructor(h: Head, now = 0) {
    this.flags = h.flags;
    this.k = h.k;
    this.r = h.r;
    this.len = h.len;
    this.p = Math.max(1, Math.ceil(h.len / h.k));
    this.buf = new Uint8Array(h.len);
    this.have = new Uint8Array(h.k);
    this.parity = new Array(h.r);
    this.missing = h.k;
    this.born = now;
  }

  get done() {
    return this.missing === 0;
  }

  get key() {
    return (this.flags & Flag.key) !== 0;
  }

  // a piece of this unit (d is the whole datagram); true once the unit is whole
  put(h: Head, d: Uint8Array): boolean {
    if (h.k !== this.k || h.r !== this.r || h.len !== this.len) return this.done;
    if (!(h.flags & Flag.resent)) this.fresh++;
    const piece = d.subarray(HeadLen);
    if (h.kind === Kind.data) {
      const [a, b] = range(this.len, this.p, h.idx);
      if (!this.have[h.idx] && piece.length === b - a) {
        this.buf.set(piece, a);
        this.have[h.idx] = 1;
        this.missing--;
      }
    } else if (!this.parity[h.idx] && piece.length === this.p) {
      this.parity[h.idx] = piece.slice();
    }
    if (!this.done) this.rebuild();
    return this.done;
  }

  private rebuild() {
    for (let j = 0; j < this.r; j++) {
      const par = this.parity[j];
      if (!par) continue;
      let gone = -1;
      let n = 0;
      for (let i = j; i < this.k; i += this.r)
        if (!this.have[i]) {
          gone = i;
          n++;
        }
      if (n !== 1) continue;
      const acc = par.slice();
      for (let i = j; i < this.k; i += this.r) {
        if (i === gone) continue;
        const [a, b] = range(this.len, this.p, i);
        xor(acc, this.buf.subarray(a, b));
      }
      const [a, b] = range(this.len, this.p, gone);
      this.buf.set(acc.subarray(0, b - a), a);
      this.have[gone] = 1;
      this.missing--;
      this.rebuilt++;
    }
  }

  // the data pieces parity can't rebuild as things stand: what to ask for
  wanted(): number[] {
    const out: number[] = [];
    for (let j = 0; j < this.r; j++) {
      let skip = this.parity[j] ? 1 : 0;
      for (let i = j; i < this.k; i += this.r) {
        if (this.have[i]) continue;
        if (skip) skip = 0;
        else out.push(i);
      }
    }
    return out.sort((a, b) => a - b);
  }
}

export function xor(acc: Uint8Array, b: Uint8Array) {
  const n = b.length;
  let i = 0;
  if (acc.byteOffset % 4 === 0 && b.byteOffset % 4 === 0) {
    const w = n >> 2;
    const a32 = new Uint32Array(acc.buffer, acc.byteOffset, w);
    const b32 = new Uint32Array(b.buffer, b.byteOffset, w);
    for (; i < w; i++) a32[i]! ^= b32[i]!;
    i = w << 2;
  }
  for (; i < n; i++) acc[i]! ^= b[i]!;
}

// how many parity pieces a unit of k pieces gets at this loss, as the host reckons it
export function parityCount(k: number, loss: number): number {
  if (k <= 4) return 1;
  const f = 1 / 16 + 3 * Math.min(0.2, Math.max(0, loss));
  return Math.min(Math.max(1, Math.ceil(k * f)), Math.max(1, k >> 1));
}

// a unit cut into its datagrams as the host cuts it: data pieces, then parity
export function cut(unit: Uint8Array, max: number, seq: number, flags: number, r?: number): Uint8Array[] {
  const len = unit.length;
  const k = Math.max(1, Math.ceil(len / Math.max(1, max)));
  const p = Math.max(1, Math.ceil(len / k));
  const rr = r ?? parityCount(k, 0);
  const head = (kind: number, idx: number, size: number) => {
    const d = new Uint8Array(HeadLen + size);
    const v = new DataView(d.buffer);
    d[0] = kind;
    d[1] = flags;
    v.setUint16(2, seq & 0xffff, true);
    v.setUint16(4, idx, true);
    v.setUint16(6, k, true);
    v.setUint16(8, rr, true);
    v.setUint32(10, len, true);
    return d;
  };
  const out: Uint8Array[] = [];
  for (let i = 0; i < k; i++) {
    const [a, b] = range(len, p, i);
    const d = head(Kind.data, i, b - a);
    d.set(unit.subarray(a, b), HeadLen);
    out.push(d);
  }
  for (let j = 0; j < rr; j++) {
    const d = head(Kind.parity, j, p);
    const acc = d.subarray(HeadLen);
    for (let i = j; i < k; i += rr) {
      const [a, b] = range(len, p, i);
      xor(acc, unit.subarray(a, b));
    }
    out.push(d);
  }
  return out;
}
