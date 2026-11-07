# glow

afterglow straight to the viewer. The link ([`protocol.md`](protocol.md)) stays the driver's: launch, video, notes, the desktop. With glow, the video's frames and the pointer's shape go from afterglow to the viewer itself over WebTransport (HTTP/3 on QUIC, on UDP), and its input comes back the same way. Nothing in between copies a frame, and a lost packet holds up nothing behind it the way it does on TCP.

The host's side is [`host/src/glow`](../host/src/glow) (the server, and the FEC in `fec.rs`), tested on loopback with a WebTransport client through 25% loss ([`host/tests/glow.rs`](../host/tests/glow.rs)). The viewer's side is [`src/glow`](../src/glow) (`GlowViewer`, `@phosphorus/afterglow/glow`), for a page or its worker: no Bun, no Node. [`test/e2e/glow.test.ts`](../test/e2e/glow.test.ts) runs the two together, Chromium against a real afterglow.

## Why

Before glow, a frame reached a browser over afterglow's loopback link, then the driver's WebSocket, then (for a host other than the one the viewer reached) a second WebSocket between the two drivers: three TCP connections and two copies through a JavaScript process, and on the internet one lost packet stalled every frame and keypress behind it until TCP retransmitted it (200 ms and more after a timeout). With a public hub forwarding any TCP or UDP port to a host, the viewer can reach afterglow directly, so glow gives:

- **One hop.** The browser's QUIC connection ends in afterglow. The video thread hands each datagram straight to QUIC; nothing goes between threads or processes.
- **No head-of-line blocking.** Frames are datagrams. A lost piece of a frame is rebuilt from parity where it lands, or asked for again on its own, and nothing waits for it but the frames that need it to decode.
- **Loss recovered without a round trip.** Every frame carries parity. A typed glyph is one datagram with a copy one millisecond behind it; a keyframe has about one parity piece in sixteen, more as the viewer reports loss.
- **Short queues.** QUIC's congestion control is BBR, which keeps the path's buffers short instead of filling them the way TCP's Cubic does. The encoder's bitrate follows what the path takes. The old way restarted the video at a lower rate, a keyframe each time.
- **No relay on the way up.** A key goes from the page to afterglow in one packet.

## Getting a viewer there

1. The driver asks the link (`direct`, 0x10, JSON `{port, id}`; `Link.direct()`, `AfterglowSource.direct()`). afterglow starts glow if it isn't running and answers (`direct`, 0x8a) `{ok, id, port, hash, ticket, expires_ms}`:
   - `port`: the UDP port glow listens on;
   - `hash`: the SHA-256 of its certificate, hex;
   - `ticket`: 32 hex characters, good for one session within `expires_ms` (60 s).
2. The driver hands these to its viewer over whatever already authenticated it (Phosphorus's session), with the address the viewer reaches the port at: the hub's public address, with the hub forwarding a UDP port to the host's glow port.
3. The viewer opens `new WebTransport("https://<address>:<port>/<ticket>", {serverCertificateHashes: [{algorithm: "sha-256", value: <hash>}]})` (`GlowViewer.connect({url, hash, ticket})`).

The certificate is afterglow's own: self-signed ECDSA P-256, valid 13 days (WebTransport pins none valid for longer than 14), made when glow starts and replaced after 6 days. Because the page pins it by its hash, there's no CA and no host name to match, and the hub never sees inside the connection. The ticket is the session's path. Without a live one the session is refused (HTTP 403) before anything else is said. One viewer at a time: a new one takes over, and the old one is closed. When the driver that asked for the ticket leaves (its link closes, or a new driver takes over), its viewer is sent away too.

While a viewer is connected, the running video's frames and pointer shapes go to it instead of the link, starting at a keyframe with its config. The link is told with a note: `{t: "direct", on: true, from}` when a viewer comes, `{t: "direct", on: false, why}` when it goes. When the viewer goes, the video carries on over the link from a keyframe, and anything the viewer held down is let go. `video`, `stop_video` and everything else stay on the link, the driver's.

Where glow binds: `phos_direct_bind` (default: every address) and `phos_direct_port` in `afterglow.conf` (`confText` writes 47101, `directPort`). Over `--stdio` it's `AFTERGLOW_DIRECT_BIND` and `AFTERGLOW_DIRECT_PORT`. Without a port, glow binds the port the first `direct` names, or any port. `ready` carries `direct: true`, and `--features` lists `direct`.

## Frames: units of datagrams

Each access unit becomes one **unit**: the same bytes the link would have written for it, `[u32 len][type][body]` messages in a row. That's the `config` message first when the parameter sets change, then the `frame` message (8 bytes of room, then AVCC). A viewer reads a unit with the link's own message parser.

A unit is cut into datagrams, each a 14-byte head and a piece (all little-endian):

```
u8 kind | u8 flags | u16 seq | u16 idx | u16 k | u16 r | u32 len
```

| Field | |
| --- | --- |
| `kind` | `0x01` a data piece, `0x02` a parity piece |
| `flags` | `1`: the unit starts with a keyframe. `2`: sent again, asked for |
| `seq` | the unit's number, one more for each unit sent, wrapping |
| `idx` | the piece's number among the data pieces (`0..k`) or the parity pieces (`0..r`) |
| `k`, `r` | how many data pieces, and how many parity pieces |
| `len` | the unit's length |

The data pieces are `p = ceil(len / k)` bytes each, the last one shorter, with `k` as small as the path's datagram size allows. Parity piece `j` is `p` bytes: the XOR of every data piece `i` with `i % r == j`, the last piece zero-padded. So a viewer missing one piece of a class (`i % r`), with that class's parity, rebuilds it. That covers any run of up to `r` lost pieces, and a unit of one piece (`r = 1`) whose parity is simply a copy. `r` is 1 for up to 4 pieces. Above that it's `ceil(k * (1/16 + 3 * loss))`, never more than `k / 2`, where `loss` is what the viewer last reported.

Data pieces go at once. Parity goes 1 ms later, so it isn't packed into the same QUIC packet as the data it protects. 5 ms and 30 ms after the newest unit, if nothing came after it, a **sync** datagram, `[0x04][0][u16 seq]`, says what that newest unit was, so a viewer notices one lost whole.

The viewer hands units on in order, from a unit flagged as a keyframe. When pieces are missing that parity can't rebuild, it asks for them (NACK, below). It asks as soon as a later unit, the unit's own last parity piece, or a sync shows they're missing, and again after `max(15 ms, 1.5 × rtt)`. afterglow keeps its last 256 units (32 MiB at most) for a second and sends what's asked for again, flagged `2`, out of a bucket that refills at 8 MiB/s. A unit asked for 5 times, or missing for `max(250 ms, 5 × rtt)`, is given up. From then on the viewer takes nothing but a keyframe (a decoder can't use the frames after a missing one) and asks for one.

Behind it all, QUIC paces the datagrams, under BBR. afterglow watches what QUIC holds unsent and the path's round trip against its minimum. When either grows, it tells the encoder to drop to three quarters of its bitrate, at most once a round trip. After a second of calm it climbs back a tenth at a time, up to the video's own `kbps`. afterglow's own encoder retargets at the next frame without a keyframe (`Ctl.kbps`). If more than 150 ms is queued anyway, deltas are dropped until a keyframe can go.

## The control stream

The viewer opens one bidirectional stream as soon as it's connected. Both ways it carries the link's framing, `[u32 len][type][body]`.

afterglow to viewer:

| Type | Body |
| --- | --- |
| `0x8b` welcome | JSON `{glow, version}`, first. `glow` is this protocol's version, 1 |
| `0x92` cursor | as the link's `cursor`: the pointer's shape as it changes |
| `0x88` pong | the ping's body |

Viewer to afterglow:

| Type | Body |
| --- | --- |
| `0x06` input | one input packet, as the link's ([protocol.md](protocol.md#input)) |
| `0x05` idr | none: a keyframe |
| `0x09` clip | the viewer's clipboard, UTF-8 |
| `0x08` ping | any 4 bytes, echoed in a pong |
| `0x10` to `0x21` | any of the viewer's datagrams below, its first byte the type and the rest the body |

Input is handled only while a video runs, as on the link.

## The viewer's datagrams

| Kind | Datagram |
| --- | --- |
| `0x10` NACK | `[kind][flags][u16 seq][u16 idx]...`: data pieces of a unit to send again. `flags` 1: all of it, the unit never seen |
| `0x11` key | `[kind]`: a keyframe, the viewer lost its place |
| `0x12` report | `[kind][0][u16 loss]`: the pieces lost lately, in thousandths, which sets how much parity goes with each unit |
| `0x20` move | `[kind][0][u16 seq][input packet]`: the pointer moved. One older than a move already applied is dropped |
| `0x21` ping | `[kind][0][u32 n]`: answered by a pong datagram, `[0x05][0][u32 n]` |

`GlowViewer` sends pointer moves as datagrams, so none waits behind a lost one. The last move of a gesture also goes on the control stream 40 ms later in case its datagram was lost, and so does the last move before any other input, so a click lands where the pointer was. Everything else goes on the control stream in order. NACKs, keyframe asks and reports go as datagrams.

## Measured

Chromium (Playwright's headless shell) against afterglow on Linux (Xvfb, xterm at 1280x720, 120 fps at 20 Mb/s, 4 cores), on loopback, `test/e2e/glow.test.ts` with `GLOW_KEYS=40`. The time is from the page's key to the frame showing its echo coming out of WebCodecs' decoder:

| | median | max |
| --- | --- | --- |
| no loss | 2.4 ms | 4.7 ms |
| 10% of the frames' datagrams dropped | 2.5 ms | 6.6 ms |
| 25% dropped (15 pieces rebuilt from parity, 3 asked for again, none lost) | 2.3 ms | 8.7 ms |

It connects in 6 to 10 ms, and the first frame is decoded 8 to 13 ms after the page started. Over a network, add the round trip. Nothing else in the path waits on it.
