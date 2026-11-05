<h1 align="center">afterglow</h1>

<p align="center">
  Native apps streamed as low-latency H.264, driven over one loopback socket.<br>
  A Rust host for Windows and Linux, and the TypeScript client that drives it.
</p>

<p align="center">
  <a href="https://github.com/pmghartman/afterglow/releases/latest"><img alt="release" src="https://img.shields.io/github/v/release/pmghartman/afterglow?style=flat-square&color=a371f7"></a>
  <img alt="license" src="https://img.shields.io/badge/license-Apache--2.0%20%2B%20GPL--3.0-f0883e?style=flat-square">
</p>

<p align="center">
  <a href="#whats-here">What's here</a> ·
  <a href="#the-protocol">The protocol</a> ·
  <a href="#using-the-client">Using the client</a> ·
  <a href="#the-host">The host</a> ·
  <a href="#development">Development</a> ·
  <a href="#releases">Releases</a> ·
  <a href="#licence">Licence</a>
</p>

---

afterglow is how [Phosphorus](https://github.com/pmghartman/phosphorus) streams a machine's native apps. Phosphorus installs the host on each Windows and Linux machine in a mesh and drives it with the client here; nothing in this repo depends on Phosphorus, and any Bun program can drive it the same way.

## What's here

| Path | What it is | Licence |
| --- | --- | --- |
| [`host/`](host) | **afterglow**, the host, in Rust. It launches one app at a time, captures and encodes its display as H.264, and injects the viewer's input. Its own H.264 encoder ([`host/src/avc`](host/src/avc)) codes only the macroblocks that changed, so a frame costs what changed in it, not its size. On Windows: DXGI capture reading back only its dirty rectangles (or Media Foundation's encoders, asked for), SendInput, a SudoVDA virtual display per app, and the `afterglowsvc.exe` service. On Linux: an Xvfb per app (or the session's display), its framebuffer read in place where XDamage says it changed (or OpenH264, asked for), XTest. | GPL-3.0-only |
| [`src/afterglow/`](src/afterglow) | `AfterglowSource`, the client: launch an app, get its frames already shaped as AVCC, send input back. Also the config, link file, service line and systemd unit an installer writes. | Apache-2.0 |
| [`src/source.ts`](src/source.ts) | The contract a source keeps with whatever drives it. | Apache-2.0 |
| [`docs/protocol.md`](docs/protocol.md) | The link's wire format. | |

## The protocol

One TCP connection on `127.0.0.1`: length-prefixed messages, a token said first, then JSON to launch an app and start a video, and binary for everything per frame ([`docs/protocol.md`](docs/protocol.md)).

- **To the first frame:** one `launch`, one `video`. No pairing, TLS, RTSP or session setup.
- **Each frame:** the encoder's output reshaped once into AVCC, the form WebCodecs' `VideoDecoder` takes, behind 8 bytes of room. One socket write on the host, one copy on the client, and the driver overwrites the room with its own header and forwards it as it is. No packetization, FEC or reassembly.
- **When frames are made:** a frame is captured and encoded when the screen changes (DXGI's frame updates on Windows, XDamage on Linux), within the video's rate, and again once a second when nothing does. Only what changed is read and converted: on Linux straight out of the app's X framebuffer, on Windows the dirty rectangles copied out of GPU memory. A keyframe is a message, not a restart.
- **Encoding what changed:** afterglow's own encoder codes the macroblocks whose pixels changed and skips every other one in a few bits, so a typed glyph is encoded in about 10 µs at 720p or 1080p alike (OpenH264 took 1.6 and 3.7 ms). Each change is coded at the quality its frame can have and still keep to about two frames' share of the bitrate (a frame's size is latency too), and once the screen has been still for 60 ms what was coded coarsely is coded again, sharper, a frame's share at a time: on screen at once, sharp a moment later. Its reconstruction is FFmpeg's to the bit, so it never drifts.
- **The pointer:** its shape goes to the viewer as a PNG when it changes, to show as the viewer's own pointer: moving it costs no frame and lags not at all. Or, asked, it's drawn into the frames.
- **Decoding:** every SPS says nothing is reordered, so a decoder shows each frame as it arrives instead of holding pictures back.
- **Input:** small binary events on the same socket, injected as they arrive.
- **Displays:** each app gets a display at the viewer's window size, so a machine with no monitor streams. The display follows the viewer through resizes, and the app keeps running between videos and viewers.

Measured on Linux (Xvfb, xterm, 120 fps at 20 Mb/s as Phosphorus asks, 4 cores; `bun run bench`, p50; OpenH264, as before, in brackets):

| | 1280x720 | 1920x1080 |
| --- | --- | --- |
| a key typed to the frame echoing it, at the driver | 0.8 to 1.0 ms (3.6) | 0.8 to 1.0 ms (5.8) |
| a key that changes every pixel, to its frame | 4.4 to 4.8 ms (7.6) | 7.8 to 9.1 ms (13.0) |
| open to first frame | 8 to 21 ms (25 to 35) | 12 to 18 ms (45 to 55) |
| a key typed to its frame decoded in Chromium, through Phosphorus's session (OpenH264's; not yet measured with afterglow's encoder) | 7.7 ms | 12.0 ms |

Of a typed key's 0.9 ms, about 80 µs is afterglow's: the rest is xterm, its shell and Xvfb drawing the glyph. Encoding alone: a glyph 10 µs at 720p and 12 µs at 1080p; a terminal flipped to reverse video 1.0 ms and 1.7 ms.

Windows hasn't been measured on GPU hardware yet.

## Using the client

The client is a [Bun](https://bun.sh) TypeScript package, consumed from this repo by release tag:

```json
{
  "dependencies": {
    "@phosphorus/afterglow": "github:pmghartman/afterglow#v0.1"
  }
}
```

Drive afterglow on the same machine:

```ts
import { AfterglowSource, codecOf, FrameRoom, pathsOf, readLink } from "@phosphorus/afterglow";

const link = readLink(pathsOf("C:\\ProgramData\\myapp\\afterglow").link);
if (!link) throw new Error("afterglow isn't set up here");

const src = new AfterglowSource({ link, target: "C:\\Windows\\System32\\notepad.exe", virtualDisplay: true });
await src.open(
  { argv: [], name: "Notepad", w: 1280, h: 720, sw: 1920, sh: 1080, scale: 1, full: false, fps: 60, dpi: 96 },
  { meta: console.log, say: console.log, exit: (err) => console.log("exited", err) },
);
src.video({ w: 1280, h: 720, fps: 60, kbps: 20_000, full: false }, {
  config: (avcc, w, h) => decoder.configure({ codec: codecOf(avcc), description: avcc, codedWidth: w, codedHeight: h }),
  frame: (key, data) => send(data.subarray(FrameRoom), key), // or write your own 8-byte header into data[0..8)
  ended: () => {},
});
src.input(["m", 640, 360], 1280, 720); // pointer to (640, 360)
src.keyframe();
src.quit();
```

On Linux the target is a program's path (`/usr/bin/gedit`), or `argv` the program and its arguments; with `virtualDisplay` it runs on an Xvfb of its own, otherwise on the session's `$DISPLAY`.

Or drive `afterglow --stdio` over any byte stream, ssh's as the viewer's login say: no port, no token, the app ended when the stream closes.

```ts
const src = new AfterglowSource({
  transport: () => sshChannel("/var/lib/phosphorus/afterglow/afterglow --stdio"), // { write, onData, onClose, close }
  target: "",
  argv: ["/usr/bin/gedit", "--new-window"],
  dbus: true,
  virtualDisplay: true,
});
```

The app's title and windows come as `sink.meta({ t: "title" | "windows" })`, what it copies as `sink.say({ t: "clip" })`; `["clip", text]` makes the viewer's clipboard the app's, `["raise", id]` brings a window forward, `close()` asks it to close as its close button would.

Input goes in as small arrays and leaves as afterglow's input packets ([`src/afterglow/input.ts`](src/afterglow/input.ts)):

| Message | Meaning |
| --- | --- |
| `["m", x, y]` | pointer to (x, y) in video pixels |
| `["b", button, down]` | mouse button 1 to 9 pressed or released |
| `["w", dx, dy]` | wheel, in notches |
| `["k", down, code, key, chord]` | a key by its `KeyboardEvent.code` and `.key`; sent as text where the viewer's layout differs from US |
| `["x", text]` | text typed |
| `["clip", text]` | the viewer's clipboard: the app's own (Linux, a display of its own), or typed at the next Ctrl+V |
| `["raise", id]` | one of the app's windows (from a `windows` note) to the front |

The pointer's shape comes as `sink.meta({ t: "cursor", png, x, y })` (base64 PNG, hotspot), or `{ t: "cursor", name: "none" }` when hidden, for the page to set as its CSS cursor. `drawCursor: true` has the host draw it into the frames instead (`{ t: "cursor", name: "blank" }` then hides the page's).

`@phosphorus/afterglow/testing` exports `FakeAfterglow`, the link's server end on a real loopback socket, for testing a driver without a host.

## The host

Each [release](https://github.com/pmghartman/afterglow/releases/latest) carries a build for each platform.

`afterglow-X.Y-windows-x86_64.zip`:

```
afterglow.exe              the host
tools/afterglowsvc.exe     the service wrapper: runs afterglow.exe as SYSTEM in the console's session
drivers/sudovda/           the signed SudoVDA virtual display driver, nefconc.exe, install.bat
LICENSE.txt NOTICE.txt SOURCE.txt VERSION
```

To install it, an app (Phosphorus's is [`packages/host/host/src/win/afterglow.ts`](https://github.com/pmghartman/phosphorus/blob/main/packages/host/host/src/win/afterglow.ts)):

1. writes `afterglow.conf` and `link.json` with `confText` and `linkText` into a folder only SYSTEM and Administrators can read (whoever reads the token can type into the console's session);
2. installs SudoVDA with `drivers\sudovda\install.bat` unless the machine already has it;
3. creates a service running `tools\afterglowsvc.exe "<path to afterglow.conf>"` (`svcBinPath`).

`afterglow-X.Y-linux-x86_64.tar.gz` and `afterglow-X.Y-linux-aarch64.tar.gz` hold `afterglow` and the same text files. It needs glibc 2.36 or later, X11 and, for apps on displays of their own, `Xvfb` (Debian and Ubuntu: `xvfb`). To install it, an app:

1. writes `afterglow.conf` and `link.json` with `confText` and `linkText`, readable by the user the apps run as and no one else;
2. installs a systemd unit from `unitText(<path to afterglow>, <path to afterglow.conf>, <user>)`, enabled and started.

It needs no desktop session: each app gets an Xvfb that only afterglow and the app hold the cookie for, and a window manager inside afterglow keeps the app's windows filling it.

afterglow then listens on `127.0.0.1:47100` (`linkPort`) and nowhere else. The token is a credential: whoever holds it can launch apps and type into them. See [`host/README.md`](host/README.md) for how the host is built and laid out.

Verify a download:

```bash
gh attestation verify afterglow-0.1-linux-x86_64.tar.gz --repo pmghartman/afterglow
```

## Development

Requires Bun 1.4.4 or later and a Rust toolchain (stable, 1.85 or later).

```bash
bun install
```

```bash
bun run check && bun run lint && bun test
```

```bash
bun run test:host && bun run test:e2e
```

```bash
bun run bench
```

- `bun test` runs the client's tests anywhere: the input packets and key mapping, the link's framing, `AfterglowSource` against `FakeAfterglow`, and the installer's files.
- `test:host` runs the host's tests: framing and frame shaping, input decoding and key repeat, the link on a real socket against a stand-in platform, capture's colour conversion, and the encoder: its streams decoded by `ffmpeg` (where it's installed) to exactly the pictures it reconstructed, keyframes, skips, slices, cropping, refinement and every qp.
- `test:e2e` builds the host and runs it for real (needs `Xvfb`, `xterm` and `ffmpeg`). `AfterglowSource` launches xterm on an Xvfb of the host's own and checks that:
  - it fills the display, and ffmpeg decodes the stream;
  - a typed command runs; the pointer's shape comes as a PNG and is never in the frames;
  - after a resize the display follows;
  - quit closes it.

  Then the pointer drawn in where it moved and nowhere else, and xterm on the session's own display. Over `--stdio`: an app by its argv, its title and windows as notes, the viewer's clipboard pasted by its own paste, a polite close, an app that dies before a window with its code and output.
- `bench` builds the host and times input to frame: a key echoed and a key that flips the whole screen, at 720p and 1080p, and open to first frame. With `BENCH_LOG=debug BENCH_KEEP=1` the host's log says where each frame's time went and how long from an input to the app's drawing and to its frame.

The Windows host builds on Windows with `cargo build --release`, or from Linux with mingw-w64 installed (`cargo build --target x86_64-pc-windows-gnu`). A release build, staged as the archive lays it out:

```bash
host/scripts/stage.sh "$PWD/afterglow-out" 0.0.0
```

Format with `bun run fmt` and `cargo fmt --manifest-path host/Cargo.toml`. A change to the link's wire format touches both ends (`host/src/wire.rs`, `host/src/link.rs`, `host/src/input.rs` and `src/afterglow/`) and `docs/protocol.md`.

## Releases

Manual, as Phosphorus's: run **release** from the Actions tab (optionally **Bump Major Version**, or **Build only** to try the builds without publishing). [`release.yml`](.github/workflows/release.yml):
- takes the version one past the highest `vMAJOR.MINOR` tag;
- runs the checks and tests, the host's end-to-end run on Linux included;
- builds the host for Windows (smoke-run on the runner) and Linux;
- publishes `vX.Y` with both archives, their `SHA256SUMS` and build provenance attestations. The notes list the merged pull requests.

A `vX.Y` tag pushed by hand is released instead, built from its own commit, when it has no release yet.

Every run ends by purging the repo's Actions storage:
- all caches: nothing in the workflow writes one, and older runs' are removed;
- all artifacts except a build-only or failed run's own builds, which stay downloadable for the day they're kept.

Consumers pin the client by that tag. Phosphorus installs the host of the version its pin names, so bumping the pin moves both.

## Licence

Two licences, by directory:

- `host/` is **GPL-3.0-only** ([`host/LICENSE`](host/LICENSE), [`host/NOTICE`](host/NOTICE)): its Windows side was ported from Apollo and Sunshine, which are. Each build's `SOURCE.txt` names the commit it was built from.
- Everything else, including the TypeScript client, is **Apache-2.0** ([`LICENSE`](LICENSE)). The client talks to the host over a socket and contains none of its code.
