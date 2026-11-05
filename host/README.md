# afterglow host

A fork of [Apollo](https://github.com/ClassicOldSong/Apollo) (itself a fork of LizardByte's [Sunshine](https://github.com/LizardByte/Sunshine)), forked at Apollo `adc5c5a0bd80831ce495434bb16aee2cd4175fb8`, cut to its Windows build: no web UI, Linux, macOS, docs or packaging.

Apollo's capture (DXGI desktop duplication, Windows.Graphics.Capture), its encoders (NVENC, AMF, Quick Sync, x264), its input and its SudoVDA virtual display are kept as they are. What sat between them and a client is gone: a driver talks to afterglow over one loopback link ([`src/phos_link.cpp`](src/phos_link.cpp), framing in [`src/phos_wire.h`](src/phos_wire.h), described in [`docs/protocol.md`](../docs/protocol.md)) instead of GameStream.

## What changed from Apollo

- `src/phos_link.cpp`, `src/phos_link.h`, `src/phos_wire.h`: the link. With `phos_link_port` set, `afterglow.exe` starts none of nvhttp, confighttp, RTSP, mDNS, UPnP or the tray, and captures no audio.
- Each encoded frame is reshaped once from Annex B into AVCC with 8 bytes of room in front and written to one socket. Input comes back as GameStream input packets, straight into `input::passthrough`. A keyframe is a message.
- Each video carries a generation, so frames of a replaced one are dropped.
- Apollo's service wrapper is `afterglowsvc.exe`, which starts `afterglow.exe` with the config named on its command line.
- AMF's H.264 profile is fixed at high (Apollo's `config_t` has no profile field).
- The CMake targets keep Apollo's names (`sunshine`, `sunshinesvc`); their outputs are `afterglow.exe` and `afterglowsvc.exe`.

## Config

`afterglow.exe <afterglow.conf>` reads Apollo's config format. The keys that matter here, as [`confText`](../src/afterglow/config.ts) writes them:

| Key | Value |
| --- | --- |
| `phos_link_port` | the link's port on `127.0.0.1` (47100) |
| `phos_link_token` | the token a driver says first, 16 characters or more |
| `file_apps` | an empty `apps.json`: the driver names each app as it launches it |
| `dd_configuration_option` | `ensure_primary`: an app's virtual display is the primary display while it streams |
| `sw_preset`, `nvenc_preset`, `qsv_preset`, `amd_usage` | each encoder's lowest-latency preset |

## On a machine

Apps launch in the session signed in at the console. With SudoVDA installed, each gets a virtual display at the viewer's window size, so a machine with no monitor streams. The service runs as SYSTEM; keep the config and token readable by SYSTEM and Administrators only.

## Building

Windows only, in MSYS2 UCRT64. The `windows` job in [`.github/workflows/release.yml`](../.github/workflows/release.yml) lists the packages, then:

```bash
git submodule update --init --recursive
```

```bash
host/scripts/build.sh "$PWD/afterglow-out" 0.0.0
```

The submodules under `third-party/` are Apollo's pins. SudoVDA's signed driver DLL and `nefconc.exe` aren't in Apollo's tree; the workflow takes the driver package from Apollo's release installer and checks it against its signed catalog.

`test/wire_test.cpp` needs no Windows or Apollo headers and builds anywhere:

```bash
bun run test:wire
```

## Licence

GPL-3.0-only, as Apollo and Sunshine ([`LICENSE`](LICENSE), [`NOTICE`](NOTICE)). Each build's `SOURCE.txt` names the repository and commit it was built from. The TypeScript client in the rest of this repo is Apache-2.0 and contains none of this code.
