<h1 align="center">afterglow</h1>

<p align="center">
  Stream a Windows machine's apps to a browser as low-latency H.264.<br>
  A Rust host driven over one loopback socket, and a GameStream client for hosts that already run Sunshine or Apollo.
</p>

<p align="center">
  <a href="https://github.com/pmghartman/afterglow/releases/latest"><img alt="release" src="https://img.shields.io/github/v/release/pmghartman/afterglow?style=flat-square&color=a371f7"></a>
  <img alt="license" src="https://img.shields.io/badge/license-Apache--2.0%20%2B%20GPL--3.0-f0883e?style=flat-square">
</p>

<p align="center">
  <a href="#whats-here">What's here</a> ·
  <a href="#using-the-client">Using the client</a> ·
  <a href="#the-host">The host</a> ·
  <a href="#development">Development</a> ·
  <a href="#releases">Releases</a> ·
  <a href="#licence">Licence</a>
</p>

---

afterglow is the Windows app-streaming stack of [Phosphorus](https://github.com/pmghartman/phosphorus). Phosphorus installs the host on every Windows machine in a mesh and drives it with the client here; nothing in this repo depends on Phosphorus, and any Bun program can drive it the same way.

## What's here

| Path | What it is | Licence |
| --- | --- | --- |
| [`host/`](host) | **afterglow**, the host, in Rust: DXGI desktop capture, Media Foundation H.264 (the GPU's encoder, or Windows' software one), SendInput and the SudoVDA virtual display, served over one token-checked loopback link. Ported from [Apollo](https://github.com/ClassicOldSong/Apollo) (itself a fork of [Sunshine](https://github.com/LizardByte/Sunshine)). `afterglow.exe` plus the `afterglowsvc.exe` service wrapper. An X11 backend runs the same link on Linux, for tests. | GPL-3.0-only |
| [`src/afterglow/`](src/afterglow) | `AfterglowSource`, the link's client: launch an app, get its frames already shaped as AVCC, send input back. Also the config and link files an installer writes. | Apache-2.0 |
| [`src/glow/`](src/glow) | **glow**, a GameStream client for a host's own Sunshine or Apollo: pairing and app control (`lumen`), RTSP setup (`ignite`), the ENet control stream sealed with AES-GCM (`spark`), RTP to whole frames (`photon`). Written from the protocol's public description. | Apache-2.0 |
| [`src/source.ts`](src/source.ts) | The contract both sources keep with whatever drives them. | Apache-2.0 |
| [`docs/protocol.md`](docs/protocol.md) | The link's wire format. | |

### afterglow against GameStream

| | glow, over GameStream | afterglow |
| --- | --- | --- |
| Setup | a separate Sunshine or Apollo install, a certificate added to its state, `apps.json` edited, a restart or a PIN | installed with the app that drives it, nothing to pair |
| To the first frame | serverinfo, launch over HTTPS, RTSP OPTIONS/DESCRIBE/SETUP/ANNOUNCE/PLAY, ENet connect | one `launch` message, one `video` message |
| Each frame | RTP-packetized, FEC computed, sent as datagrams, rebuilt from them | the encoder's frame reshaped once to AVCC, one socket write, copied once on arrival |
| Control and input | ENet, AES-GCM sealed | plain messages on the same loopback socket |
| Audio | captured and Opus-encoded | not captured |
| Listening | web UI, nvhttp, RTSP, mDNS, UPnP, tray | the link on `127.0.0.1` only |
| Encoders | NVENC, AMF, Quick Sync, x264 | Media Foundation's: the GPU vendor's, else Windows' software H.264 |

Both give each app a virtual display sized to the viewer's window (where the host has SudoVDA), so a machine with no monitor streams; ask for a keyframe instead of restarting the video; and keep the app running between viewers.

Measured on loopback against Sunshine through glow: a changed pixel to a whole frame p50 14 ms, p95 18 ms at 120 fps; open to first frame 0.3 s.

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
import { AfterglowSource, FrameRoom, pathsOf, readLink } from "@phosphorus/afterglow";

const link = readLink(pathsOf("C:\\ProgramData\\myapp\\afterglow").link);
if (!link) throw new Error("afterglow isn't set up here");

const src = new AfterglowSource({ link, target: "C:\\Windows\\System32\\notepad.exe", virtualDisplay: true });
await src.open(
  { argv: [], name: "Notepad", w: 1280, h: 720, sw: 1920, sh: 1080, scale: 1, full: false, fps: 60, dpi: 96 },
  { meta: console.log, say: console.log, exit: (err) => console.log("exited", err) },
);
src.video({ w: 1280, h: 720, fps: 60, kbps: 20_000, full: false }, {
  config: (avcc, w, h) => decoder.configure({ codec: "avc1.640033", description: avcc, codedWidth: w, codedHeight: h }),
  frame: (key, data) => send(data.subarray(FrameRoom), key), // or write your own 8-byte header into data[0..8)
  ended: () => {},
});
src.input(["m", 640, 360], 1280, 720); // pointer to (640, 360)
src.keyframe();
src.quit();
```

`GlowSource` keeps the same contract against a GameStream host; `GlowHost` finds the local Sunshine or Apollo, adds the client's certificate without a PIN where it can write the host's state, and gives it one launcher app so no app list needs editing.

Input goes in as small arrays and leaves as GameStream input packets ([`src/glow/input.ts`](src/glow/input.ts)):

| Message | Meaning |
| --- | --- |
| `["m", x, y]` | pointer to (x, y) in video pixels |
| `["b", button, down]` | mouse button 1 to 9 pressed or released |
| `["w", dx, dy]` | wheel, in notches |
| `["k", down, code, key, chord]` | a key by its `KeyboardEvent.code` and `.key`; sent as text where the viewer's layout differs from US |
| `["x", text]` | text typed |
| `["clip", text]` | the viewer's clipboard: the next Ctrl+V types it |

`@phosphorus/afterglow/testing` exports `FakeAfterglow`, the link's server end on a real loopback socket, for testing a driver without Windows. The real host also runs on Linux under X11 (`bun run test:e2e`).

## The host

Each [release](https://github.com/pmghartman/afterglow/releases/latest) carries `afterglow-X.Y-windows-x86_64.zip`:

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

afterglow then listens on `127.0.0.1:47100` (`linkPort`) and nowhere else. See [`host/README.md`](host/README.md) for how it's built and laid out.

Verify a download:

```bash
gh attestation verify afterglow-0.1-windows-x86_64.zip --repo pmghartman/afterglow
```

## Development

Requires Bun 1.4.4 or later.

```bash
bun install
```

```bash
bun run check && bun run lint && bun test
```

The host needs a Rust toolchain (stable, 1.85 or later):

```bash
bun run test:host
```

```bash
bun run test:e2e
```

`bun test` runs the client's tests anywhere: glow's packet builders and parsers, ENet, the control stream's sealing, the RTP depacketizer, the link's framing, and `AfterglowSource` against `FakeAfterglow`. `test:host` runs the host's tests: the framing and frame shaping, input decoding and key repeat, and the link served on a real socket against a stand-in platform. `test:e2e` builds the host and runs it for real on an X server (needs `Xvfb`, `xterm` and `ffmpeg`): `AfterglowSource` launches xterm through it, ffmpeg decodes the stream, a typed command runs, the pointer is drawn where it moved, a resize, quit.

The Windows host builds on Windows with `cargo build --release`, or from Linux with mingw-w64 installed (`cargo build --target x86_64-pc-windows-gnu`). A release build, staged as the zip lays it out:

```bash
host/scripts/stage.sh "$PWD/afterglow-out" 0.0.0
```

Format with `bun run fmt`. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Releases

Manual, as Phosphorus's: run **release** from the Actions tab (optionally **Bump Major Version**, or **Build only** to try the Windows build without publishing). [`release.yml`](.github/workflows/release.yml) takes the version one past the highest `vMAJOR.MINOR` tag, runs the checks and tests (the host's end-to-end run included), builds the host at that version and smoke-runs it on Windows, and publishes `vX.Y` with the host zip, its `SHA256SUMS` and a build provenance attestation; the notes list the merged pull requests.

A `vX.Y` tag pushed by hand is released instead, built from its own commit, when it has no release yet.

Consumers pin the client by that tag. Phosphorus installs the host zip of the version its pin names, so bumping the pin moves both.

## Licence

Two licences, by directory:

- `host/` is **GPL-3.0-only** ([`host/LICENSE`](host/LICENSE), [`host/NOTICE`](host/NOTICE)): ported from Apollo and Sunshine, which are. Each build's `SOURCE.txt` names the commit it was built from.
- Everything else, including the TypeScript client, is **Apache-2.0** ([`LICENSE`](LICENSE)). The client talks to the host over a socket and contains none of its code; glow was written from GameStream's public description, nothing taken from Sunshine's or Moonlight's code.
