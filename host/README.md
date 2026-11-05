# afterglow host

The streaming host, in Rust: one app at a time launched, its display captured and encoded as H.264, the viewer's input injected, all driven over one loopback link ([`docs/protocol.md`](../docs/protocol.md)). Ported from [Apollo](https://github.com/ClassicOldSong/Apollo) (itself a fork of LizardByte's [Sunshine](https://github.com/LizardByte/Sunshine)), keeping only what the link uses: no GameStream servers, web UI, tray, audio, gamepads, touch or pen.

## Layout

| Path | What it is |
| --- | --- |
| `src/wire.rs` | the link's framing and the Annex B to AVCC reshaping, no platform code |
| `src/link.rs` | the link: the token, one driver at a time, launch, videos and their generations, input, the app's exit |
| `src/input.rs` | GameStream input packets into events; held keys repeated, everything let go when a video stops |
| `src/conf.rs` | `afterglow.conf` (`key = value`); unknown keys ignored, so a config written for the C++ host works |
| `src/host.rs` | what the link asks of a platform |
| `src/platform/windows/` | DXGI desktop duplication with the pointer drawn in (`capture.rs`), the D3D11 video processor and Media Foundation's H.264 encoder (`encode.rs`), SendInput (`input.rs`), the app in a job in the console user's session (`process.rs`), SudoVDA (`vdisplay.rs`), display modes and the primary display (`display.rs`), and the service (`svc.rs`) |
| `src/platform/x11.rs` | the same on an X server: GetImage and XFixes, OpenH264, XTest. What the end-to-end test runs |
| `tests/link.rs` | the link on a real socket against a stand-in platform |

## On Windows

`afterglow.exe <afterglow.conf>` serves the link on `127.0.0.1:<phos_link_port>`. As a service, `tools\afterglowsvc.exe "<afterglow.conf>"` runs as SYSTEM and keeps `afterglow.exe` running as SYSTEM in the session at the console, restarting it when it exits or the console moves; stopping the service asks it to stop (Ctrl-C), so the app is closed and its display removed.

An app is started as the console's signed-in user (a limited token for an admin under UAC), in a job so everything it starts is followed. With SudoVDA installed and `virtual_display` asked for, it gets a display of its own at the viewer's size, made primary with `dd_configuration_option = ensure_primary`; a machine with no monitor streams. The display follows each video's size and is removed with the app.

Capture runs on whichever desktop has the input, so UAC prompts and the lock screen are streamed. Frames go through the GPU's video processor (BGRA to NV12, scaled and letterboxed, BT.709 limited range) into the encoder Media Foundation lists for that GPU (NVENC, AMF or Quick Sync register one) in low latency mode, CBR at the asked bitrate; with no hardware encoder, or `encoder = software`, Windows' own. H.264 4:4:4 isn't offered, so `yuv444` gets 4:2:0.

| Key | Value |
| --- | --- |
| `phos_link_port` | the link's port on `127.0.0.1` |
| `phos_link_token` | what a driver says first, 16 characters or more |
| `log_path` | the log file, truncated each start |
| `min_log_level` | `debug`, `info`, `warning` or `error` |
| `dd_configuration_option` | `ensure_primary`: an app's virtual display is the primary one while it streams |
| `adapter_name` | the GPU SudoVDA renders on, by name |
| `encoder` | `software`: skip hardware encoders |

The service runs as SYSTEM; keep the config and token readable by SYSTEM and Administrators only.

## Building

```bash
cargo test
cargo build --release
```

On Windows that's the host; from Linux, `--target x86_64-pc-windows-gnu` cross-builds it with mingw-w64 installed, and wine runs its tests (`CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine`). On Linux it's the X11 backend: `DISPLAY` names the X server to capture.

`scripts/stage.sh OUT_DIR VERSION [TARGET]` tests, builds and stages a release as the zip lays it out. The SudoVDA driver package isn't built here: the release workflow takes it from Apollo's installer and checks it against its signed catalog.

## Licence

GPL-3.0-only ([`LICENSE`](LICENSE), [`NOTICE`](NOTICE)), as Apollo and Sunshine. Each build's `SOURCE.txt` names the repository and commit it was built from. The TypeScript client in the rest of this repo is Apache-2.0 and contains none of this code.
