<h1 align="center">afterglow</h1>

<p align="center">
  Stream a Windows machine's apps to a browser as low-latency H.264.<br>
  A fork of Apollo driven over one loopback socket, and a GameStream client for hosts that already run Sunshine or Apollo.
</p>

<p align="center">
  <a href="https://github.com/pmghartman/afterglow/actions/workflows/ci.yml"><img alt="ci" src="https://img.shields.io/github/actions/workflow/status/pmghartman/afterglow/ci.yml?branch=main&style=flat-square&label=ci"></a>
  <a href="https://github.com/pmghartman/afterglow/actions/workflows/host.yml"><img alt="host build" src="https://img.shields.io/github/actions/workflow/status/pmghartman/afterglow/host.yml?branch=main&style=flat-square&label=host%20build"></a>
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
| [`host/`](host) | **afterglow**, the host: [Apollo](https://github.com/ClassicOldSong/Apollo) (itself a fork of [Sunshine](https://github.com/LizardByte/Sunshine)) cut to its Windows capture, hardware encoders, input and SudoVDA virtual display, with GameStream's servers replaced by one token-checked loopback link. `afterglow.exe` plus the `afterglowsvc.exe` service wrapper. | GPL-3.0-only |
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

`@phosphorus/afterglow/testing` exports `FakeAfterglow`, the link's server end on a real loopback socket, for testing a driver without Windows.

## The host

Each [release](https://github.com/pmghartman/afterglow/releases/latest) carries `afterglow-X.Y-windows-x86_64.zip`:

```
afterglow.exe              the host
tools/afterglowsvc.exe     the service wrapper: runs afterglow.exe as SYSTEM in the console's session
assets/                    shaders compiled at run time
drivers/sudovda/           the signed SudoVDA virtual display driver, nefconc.exe, install.bat
zlib1.dll                  loaded by OpenSSL
LICENSE.txt NOTICE.txt SOURCE.txt VERSION
```

To install it, an app (Phosphorus's is [`packages/host/host/src/win/afterglow.ts`](https://github.com/pmghartman/phosphorus/blob/main/packages/host/host/src/win/afterglow.ts)):

1. writes `afterglow.conf`, `apps.json` and `link.json` with `confText`, `appsText` and `linkText` into a folder only SYSTEM and Administrators can read (whoever reads the token can type into the console's session);
2. installs SudoVDA with `drivers\sudovda\install.bat` unless the machine already has it;
3. creates a service running `tools\afterglowsvc.exe "<path to afterglow.conf>"` (`svcBinPath`).

afterglow then listens on `127.0.0.1:47100` (`linkPort`) and nowhere else. See [`host/README.md`](host/README.md) for what changed from Apollo and how to build it.

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

```bash
bun run test:wire
```

`bun test` runs the client's tests anywhere: glow's packet builders and parsers, ENet, the control stream's sealing, the RTP depacketizer, the link's framing, and `AfterglowSource` against `FakeAfterglow`. `test:wire` builds and runs the host's link framing tests (`host/test/wire_test.cpp`) with any C++20 compiler.

The host builds on Windows only, in MSYS2 UCRT64; [`.github/workflows/host.yml`](.github/workflows/host.yml) lists the packages:

```bash
git submodule update --init --recursive
```

```bash
host/scripts/build.sh "$PWD/afterglow-out" 0.0.0
```

Format with `bun run fmt`. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Releases

Manual, as Phosphorus's: run **release** from the Actions tab (optionally **Bump Major Version**). [`release.yml`](.github/workflows/release.yml) takes the version one past the highest `vMAJOR.MINOR` tag, runs CI, builds the host at that version, and publishes `vX.Y` with the host zip, its `SHA256SUMS` and a build provenance attestation; the notes list the merged pull requests.

Consumers pin the client by that tag. Phosphorus installs the host zip of the version its pin names, so bumping the pin moves both.

## Licence

Two licences, by directory:

- `host/` is **GPL-3.0-only** ([`host/LICENSE`](host/LICENSE), [`host/NOTICE`](host/NOTICE)), as Apollo and Sunshine are. Each build's `SOURCE.txt` names the commit it was built from.
- Everything else, including the TypeScript client, is **Apache-2.0** ([`LICENSE`](LICENSE)). The client talks to the host over a socket and contains none of its code; glow was written from GameStream's public description, nothing taken from Sunshine's or Moonlight's code.
