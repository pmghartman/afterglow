// glow, phosphorus's GameStream client: its packet builders and parsers against the
// layouts the protocol documents, the depacketizer on synthetic RTP, the control
// stream's sealing, the host's files as phosphorus edits them

import { describe, expect, test } from "bun:test";
import { createCipheriv, createDecipheriv, createPrivateKey, verify, X509Certificate } from "node:crypto";
import dgram from "node:dgram";
import { EnetPeer } from "../src/glow/enet.ts";
import { announceSdp, parseResponse } from "../src/glow/ignite.ts";
import { PktHead, RtpLen } from "../src/glow/photon.ts";
import {
  addClient,
  addLauncher,
  asText,
  avccOf,
  Depacketizer,
  FrameRoom,
  filesOf,
  makeIdentity,
  nalsOf,
  pack,
  parseConf,
  Sealer,
  signatureOf,
  utf8Chunks,
  vkOf,
} from "../src/index.ts";

const hex = (b: Uint8Array) => Buffer.from(b).toString("hex");

describe("cert", () => {
  const id = makeIdentity("phosphorus");
  test("a self-signed x.509 certificate node reads", () => {
    const x = new X509Certificate(id.cert);
    expect(x.subject).toBe("CN=phosphorus");
    expect(x.issuer).toBe("CN=phosphorus");
    expect(x.verify(x.publicKey)).toBe(true);
    expect(new Date(x.validTo).getTime()).toBeGreaterThan(Date.now() + 19 * 365 * 86_400_000);
    expect(x.publicKey.asymmetricKeyDetails?.modulusLength).toBe(2048);
  });
  test("its signature, as pairing hashes it", () => {
    expect(hex(signatureOf(id.cert))).toBe(hex(id.sig));
    expect(id.sig.length).toBe(256);
    expect(id.uniqueid).toMatch(/^[0-9A-F]{16}$/);
    expect(createPrivateKey(id.key).asymmetricKeyType).toBe("rsa");
  });
});

describe("rtsp", () => {
  test("an answer whole, in pieces, with a body", () => {
    const r =
      "RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: DEADBEEFCAFE;timeout = 90\r\nTransport: server_port=47998\r\nX-SS-Ping-Payload: C51FD57F41812618\r\n\r\n";
    const p = parseResponse(r)!;
    expect(p.status).toBe(200);
    expect(p.headers.transport).toBe("server_port=47998");
    expect(p.headers["x-ss-ping-payload"]).toBe("C51FD57F41812618");
    expect(parseResponse(r.slice(0, 20))).toBeUndefined();
    const withBody = "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: 5\r\n\r\nab";
    expect(parseResponse(withBody)).toBeUndefined();
    expect(parseResponse(`${withBody}cde`)!.body).toBe("abcde");
    expect(() => parseResponse("HTTP/1.1 200 OK\r\n\r\n")).toThrow();
  });
  test("announce: the stream's terms", () => {
    const s = announceSdp(
      { w: 1920, h: 1080, fps: 120, kbps: 30000, packetSize: 32768, minFec: 0, yuv444: true, slices: 1 },
      "127.0.0.1",
      true,
    );
    for (const l of [
      "a=x-nv-video[0].clientViewportWd:1920 ",
      "a=x-nv-video[0].clientViewportHt:1080 ",
      "a=x-nv-video[0].maxFPS:120 ",
      "a=x-nv-video[0].packetSize:32768 ",
      "a=x-nv-vqos[0].bw.maximumBitrateKbps:30000 ",
      "a=x-nv-vqos[0].fec.minRequiredFecPackets:0 ",
      "a=x-ss-general.encryptionEnabled:1 ",
      "a=x-ss-video[0].chromaSamplingType:1 ",
    ])
      expect(s).toContain(`${l}\r\n`);
    expect(s.startsWith("v=0\r\no=android 0 14 IN IPv4 127.0.0.1\r\n")).toBe(true);
    expect(s.endsWith("t=0 0\r\nm=video 47998  \r\n")).toBe(true);
  });
});

describe("input packets", () => {
  // the documented examples, header (type 0x0206, length) and all
  const framed = (b: Uint8Array) => {
    const out = new Uint8Array(4 + b.length);
    const v = new DataView(out.buffer);
    v.setUint16(0, 0x0206, true);
    v.setUint16(2, b.length, true);
    out.set(b, 4);
    return hex(out);
  };
  test("absolute mouse: big-endian, the far edge as width-1", () => {
    expect(framed(pack.mouseAbs(0x05b8, 0x0210, 1920, 1080))).toBe(
      "06021200" + "0000000e" + "05000000" + "05b8" + "0210" + "0000" + "077f" + "0437",
    );
  });
  test("relative mouse", () => {
    expect(framed(pack.mouseRel(-1, 0))).toBe("06020c000000000807000000ffff0000");
  });
  test("buttons", () => {
    expect(framed(pack.button(true, 1))).toBe("060209000000000508000000" + "01");
    expect(framed(pack.button(false, 3))).toBe("060209000000000509000000" + "03");
  });
  test("keys: the virtual key with its high bit, the modifiers", () => {
    expect(framed(pack.key(true, 0xa4, 0x04))).toBe("06020e000000000a03000000" + "00a480040000");
    expect(framed(pack.key(false, 0x41, 0))).toBe("06020e000000000a04000000" + "004180000000");
  });
  test("scroll, one notch down; horizontal", () => {
    expect(framed(pack.scroll(-120))).toBe("06020e000000000a0a000000" + "ff88ff880000");
    expect(framed(pack.hscroll(30))).toBe("06020a00000000060100005500" + "1e".padStart(2, "0"));
  });
  test("text", () => {
    expect(framed(pack.utf8("hé"))).toBe("06020b000000000717000000" + "68c3a9");
  });
});

describe("control sealing", () => {
  const key = new Uint8Array(16).fill(7);
  test("v2: a 12-byte IV, the sequence then who sent it; the host opens ours", () => {
    const s = new Sealer(key, true);
    const p = s.seal(0x0302, new Uint8Array([0, 0]));
    const v = new DataView(p.buffer);
    expect(v.getUint16(0, true)).toBe(0x0001);
    expect(v.getUint16(2, true)).toBe(p.length - 4);
    expect(v.getUint32(4, true)).toBe(0);
    // as the host would: IV seq || 0.. || 'C' 'C'
    const iv = new Uint8Array(12);
    iv[10] = 0x43;
    iv[11] = 0x43;
    const d = createDecipheriv("aes-128-gcm", key, iv);
    d.setAuthTag(p.subarray(8, 24));
    const plain = Buffer.concat([d.update(p.subarray(24)), d.final()]);
    expect(hex(plain)).toBe("0203" + "0200" + "0000");
    // the next one under the next sequence number
    expect(new DataView(s.seal(0x0302, new Uint8Array(2)).buffer).getUint32(4, true)).toBe(1);
  });
  test("the host's packets opened, a forged one refused", () => {
    const s = new Sealer(key, true);
    const iv = new Uint8Array(12);
    new DataView(iv.buffer).setUint32(0, 5, true);
    iv[10] = 0x48;
    iv[11] = 0x43;
    const c = createCipheriv("aes-128-gcm", key, iv);
    const plain = new Uint8Array([0x09, 0x01, 0x04, 0x00, 0x80, 0x03, 0x00, 0x23]);
    const ct = Buffer.concat([c.update(plain), c.final()]);
    const pkt = new Uint8Array(24 + ct.length);
    const v = new DataView(pkt.buffer);
    v.setUint16(0, 1, true);
    v.setUint16(2, 20 + ct.length, true);
    v.setUint32(4, 5, true);
    pkt.set(c.getAuthTag(), 8);
    pkt.set(ct, 24);
    const m = s.open(pkt)!;
    expect(m.type).toBe(0x0109);
    expect(hex(m.payload)).toBe("80030023");
    pkt[30]! ^= 1;
    expect(s.open(pkt)).toBeUndefined();
  });
});

describe("enet", () => {
  test("connect, verify, a reliable send acked, the host's reliable acked", async () => {
    const got: Buffer[] = [];
    const host = dgram.createSocket("udp4") as dgram.Socket & { on(e: string, f: (...a: any[]) => void): void };
    host.on("message", (m: Buffer, r: { port: number }) => {
      got.push(m);
      const v = new DataView(m.buffer, m.byteOffset, m.byteLength);
      if ((m[4]! & 0x0f) === 2) {
        // VERIFY_CONNECT back: our peer id 7, sessions 1 and 2, the client's connect id
        const out = Buffer.alloc(4 + 44);
        const o = new DataView(out.buffer);
        o.setUint16(0, 0x8fff);
        o.setUint16(2, 0);
        out[4] = 3 | 0x80;
        out[5] = 0xff;
        o.setUint16(6, 1);
        o.setUint16(8, 7);
        out[10] = 1;
        out[11] = 2;
        o.setUint32(44, v.getUint32(4 + 40));
        host.send(out, r.port, "127.0.0.1");
      }
    });
    await new Promise<void>((ok) => host.bind(0, "127.0.0.1", () => ok()));
    const data: Uint8Array[] = [];
    const peer = new EnetPeer("127.0.0.1", host.address().port, 0x20, {
      data: (_c, p) => void data.push(p.slice()),
      close: () => {},
    });
    await peer.connect(0x47524f57);
    const c = got[0]!;
    const cv = new DataView(c.buffer, c.byteOffset, c.byteLength);
    expect(cv.getUint16(0) & 0x0fff).toBe(0x0fff);
    expect(c[4]).toBe(2 | 0x80);
    expect(c[5]).toBe(0xff);
    expect(cv.getUint32(4 + 16)).toBe(0x20);
    expect(cv.getUint32(4 + 44)).toBe(0x47524f57);
    // our sends carry the host's peer id and session
    peer.send(2, new Uint8Array([1, 2, 3]));
    await Bun.sleep(30);
    const s = got.find((m) => (m[4]! & 0x0f) === 6 || (m.length > 12 && (m[12]! & 0x0f) === 6))!;
    const sv = new DataView(s.buffer, s.byteOffset, s.byteLength);
    expect(sv.getUint16(0) & 0x0fff).toBe(7);
    expect((sv.getUint16(0) >> 12) & 3).toBe(2);
    expect(hex(s.subarray(s.length - 3))).toBe("010203");
    peer.close();
    host.close();
  });
});

// --- video ---------------------------------------------------------------------------

const sps = new Uint8Array([0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9]);
const pps = new Uint8Array([0x68, 0xeb, 0xe3, 0xcb]);
const sc4 = [0, 0, 0, 1];
// a host frame: its 8-byte header, then Annex B
function hostFrame(nals: number[][], type = 1, short = false): Uint8Array {
  const body: number[] = [];
  nals.forEach((n, i) => body.push(...(short && i > 0 ? [0, 0, 1] : sc4), ...n));
  return new Uint8Array([0x01, 0x10, 0x00, type, 0, 0, 0, 0, ...body]);
}
// the frame cut into the host's datagrams: RTP, the video header, shardLen bytes each
function packetize(frame: Uint8Array, index: number, shardLen: number, blocks = 1, parity = 1): Uint8Array[] {
  const shards = Math.ceil(frame.length / shardLen);
  const last = frame.length - (shards - 1) * shardLen;
  const f = frame.slice();
  new DataView(f.buffer).setUint16(4, last, true);
  const per = Math.ceil(shards / blocks);
  const out: Uint8Array[] = [];
  let seq = 0;
  for (let b = 0; b < blocks; b++) {
    const lo = b * per;
    const hi = Math.min(shards, lo + per);
    const n = hi - lo;
    for (let i = 0; i < n + parity; i++) {
      const p = new Uint8Array(PktHead + shardLen);
      const v = new DataView(p.buffer);
      p[0] = 0x90;
      v.setUint16(2, seq++);
      v.setUint32(RtpLen + 4, index, true);
      const sh = lo + i;
      p[RtpLen + 8] = i < n ? 1 | (sh === 0 ? 4 : 0) | (sh === shards - 1 ? 2 : 0) : 0;
      p[RtpLen + 11] = (b << 4) | ((blocks - 1) << 6);
      v.setUint32(RtpLen + 12, (n << 22) | (i << 12) | (20 << 4), true);
      if (i < n) p.set(f.subarray(sh * shardLen, Math.min(f.length, (sh + 1) * shardLen)), PktHead);
      out.push(p);
    }
  }
  return out;
}

type Out = { cfg: Uint8Array[]; frames: { msg: Uint8Array; key: boolean; index: number }[]; lost: number[] };
const collect = (shard: number): [Depacketizer, Out] => {
  const o: Out = { cfg: [], frames: [], lost: [] };
  const d = new Depacketizer(shard, {
    config: (a) => void o.cfg.push(a),
    frame: (msg, key, index) => void o.frames.push({ msg: msg.slice(), key, index }),
    lost: (i) => void o.lost.push(i),
  });
  return [d, o];
};
const avccNals = (m: Uint8Array) => {
  const v = new DataView(m.buffer, m.byteOffset, m.byteLength);
  const out: string[] = [];
  for (let o = FrameRoom; o < m.length; ) {
    const n = v.getUint32(o);
    out.push(hex(m.subarray(o + 4, o + 4 + n)));
    o += 4 + n;
  }
  return out;
};

describe("video", () => {
  test("nal units found, padding and 3- or 4-byte start codes", () => {
    const b = new Uint8Array([0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0, 0, 0]);
    expect(nalsOf(b).map((n) => [n.type, hex(b.subarray(n.start, n.end))])).toEqual([
      [7, "670102"],
      [8, "6803"],
      [5, "650909"],
    ]);
  });
  test("avcC from SPS and PPS", () => {
    expect(hex(avccOf(sps, pps))).toBe(`01 64001f ffe1 0006 ${hex(sps)} 01 0004 ${hex(pps)}`.replace(/ /g, ""));
  });
  test("an IDR: parameter sets to config, the slice alone in the frame", () => {
    const [d, o] = collect(64);
    const idr = [0x65, ...Array.from({ length: 150 }, (_, i) => (i % 250) + 1)];
    for (const p of packetize(hostFrame([[...sps], [...pps], [0x06, 5, 0x80], idr], 2), 1, 64)) d.push(p);
    expect(o.cfg.length).toBe(1);
    expect(hex(o.cfg[0]!)).toBe(hex(avccOf(sps, pps)));
    expect(o.frames.length).toBe(1);
    expect(o.frames[0]!.key).toBe(true);
    expect(avccNals(o.frames[0]!.msg)).toEqual(["060580", hex(new Uint8Array(idr))]);
  });
  test("a P-frame built in place over the host's header, one copy", () => {
    const [d, o] = collect(1000);
    const p = [0x41, 0x9a, 0x26, 0x22, 5, 6, 7];
    const pkts = packetize(hostFrame([p]), 9, 1000);
    expect(pkts.length).toBe(2);
    d.push(pkts[0]!);
    expect(o.frames.length).toBe(1);
    expect(o.frames[0]!.key).toBe(false);
    expect(o.frames[0]!.msg.length).toBe(FrameRoom + 4 + p.length);
    expect(avccNals(o.frames[0]!.msg)).toEqual([hex(new Uint8Array(p))]);
    // the parity after it changes nothing
    d.push(pkts[1]!);
    expect(o.frames.length).toBe(1);
  });
  test("two FEC blocks, a 3-byte start code between slices", () => {
    const [d, o] = collect(32);
    const a = [0x41, ...Array.from({ length: 100 }, (_, i) => i + 1)];
    const b = [0x01, ...Array.from({ length: 90 }, (_, i) => 200 - i)];
    for (const p of packetize(hostFrame([a, b], 1, true), 3, 32, 2)) d.push(p);
    expect(o.frames.length).toBe(1);
    expect(avccNals(o.frames[0]!.msg)).toEqual([hex(new Uint8Array(a)), hex(new Uint8Array(b))]);
  });
  test("a frame missing a packet: dropped, a keyframe asked for; old ones ignored", () => {
    const [d, o] = collect(32);
    const a = [0x41, ...Array.from({ length: 100 }, (_, i) => i + 1)];
    const pk = packetize(hostFrame([a]), 4, 32);
    for (const p of pk.filter((_, i) => i !== 1)) d.push(p);
    for (const p of packetize(hostFrame([[0x41, 1]]), 5, 32)) d.push(p);
    expect(o.lost).toEqual([4]);
    expect(o.frames.map((f) => f.index)).toEqual([5]);
    for (const p of packetize(hostFrame([[0x41, 2]]), 3, 32)) d.push(p);
    expect(o.frames.length).toBe(1);
    expect(d.dropped).toBe(1);
  });
});

describe("keys", () => {
  test("virtual keys, US layout", () => {
    expect(vkOf("KeyA")).toBe(0x41);
    expect(vkOf("Digit0")).toBe(0x30);
    expect(vkOf("F12")).toBe(0x7b);
    expect(vkOf("ShiftRight")).toBe(0xa1);
    expect(vkOf("Numpad5")).toBe(0x65);
    expect(vkOf("Quote")).toBe(0xde);
    expect(vkOf("Nope")).toBe(0);
  });
  test("text when the viewer's layout differs on that key", () => {
    expect(asText("KeyA", "a", false, false)).toBe(false);
    expect(asText("KeyA", "A", false, true)).toBe(false);
    expect(asText("Digit2", "@", false, true)).toBe(false);
    // AZERTY: the key US calls Q types a; German: Digit2 shifted is "
    expect(asText("KeyQ", "a", false, false)).toBe(true);
    expect(asText("Digit2", '"', false, true)).toBe(true);
    expect(asText("Digit2", "é", false, false)).toBe(true);
    // shortcuts and named keys stay keys
    expect(asText("KeyQ", "a", true, false)).toBe(false);
    expect(asText("Enter", "Enter", false, false)).toBe(false);
  });
  test("text in 32-byte packets of whole characters", () => {
    expect(utf8Chunks("abc")).toEqual(["abc"]);
    const c = utf8Chunks("é".repeat(20));
    expect(c.map((x) => Buffer.byteLength(x))).toEqual([32, 8]);
    expect(utf8Chunks("😀".repeat(9)).every((x) => Buffer.byteLength(x) <= 32)).toBe(true);
  });
});

describe("the host's files", () => {
  test("sunshine.conf, its paths under the config dir unless absolute", () => {
    const c = parseConf("# x\nport = 47989\nfile_state = state.json\ncert=C:\\x\\c.pem\n");
    expect(c.port).toBe("47989");
    const f = filesOf("/cfg", "file_state = s.json\ncert = /abs/c.pem\n", "sunshine", "");
    expect(f.state).toBe("/cfg/s.json");
    expect(f.apps).toBe("/cfg/apps.json");
    expect(f.cert).toBe("/abs/c.pem");
    expect(filesOf("C:\\cfg", "cert = D:\\c.pem", "apollo", "ApolloService").cert).toBe("D:\\c.pem");
  });
  test("our certificate among the paired clients, once; the state given an id", () => {
    const id = makeIdentity();
    const a = addClient("", id.cert);
    expect(a.changed).toBe(true);
    const st = JSON.parse(a.text);
    expect(st.root.uniqueid).toMatch(/^[0-9A-F-]{36}$/);
    expect(st.root.named_devices[0].cert).toBe(id.cert);
    expect(addClient(a.text, id.cert).changed).toBe(false);
    const other = JSON.stringify({ root: { uniqueid: "U", named_devices: [{ name: "phone", cert: "X", uuid: "1" }] } });
    const b = JSON.parse(addClient(other, id.cert).text);
    expect(b.root.uniqueid).toBe("U");
    expect(b.root.named_devices.map((d: any) => d.name)).toEqual(["phone", "phosphorus"]);
    expect(() => addClient("{", id.cert)).toThrow();
  });
  test("the launcher in the app list, the host's own apps kept", () => {
    const base = JSON.stringify({ env: { A: "1" }, apps: [{ name: "Desktop" }] });
    const a = addLauncher(base, "cmd /c x");
    const j = JSON.parse(a.text);
    expect(j.apps.map((x: any) => x.name)).toEqual(["Desktop", "phosphorus"]);
    expect(j.env.A).toBe("1");
    expect(addLauncher(a.text, "cmd /c x").changed).toBe(false);
    expect(JSON.parse(addLauncher(a.text, "cmd /c y").text).apps[1].cmd).toBe("cmd /c y");
  });
});

test("pairing's signature checks out against the key", () => {
  // what the host checks of our pairing secret
  const id = makeIdentity();
  const secret = new Uint8Array(16).fill(3);
  const { sign } = require("node:crypto");
  const sig = sign("sha256", secret, createPrivateKey(id.key));
  expect(verify("sha256", secret, new X509Certificate(id.cert).publicKey, sig)).toBe(true);
});
