# afterglow host

The streaming host, in Rust, for Windows and Linux: one app at a time launched, its display captured and encoded as H.264, the viewer's input injected, all driven over one loopback link ([`docs/protocol.md`](../docs/protocol.md)). The Windows side was ported from [Apollo](https://github.com/ClassicOldSong/Apollo) (itself a fork of LizardByte's [Sunshine](https://github.com/LizardByte/Sunshine)), keeping only what the link uses: no GameStream servers, web UI, tray, audio, gamepads, touch or pen.

## Layout

| Path | What it is |
| --- | --- |
| `src/wire.rs` | the link's framing and the Annex B to AVCC reshaping, no platform code |
| `src/link.rs` | the link: the token, one driver at a time, launch, videos and their generations, input, the app's exit |
| `src/input.rs` | input packets into events; held keys repeated, everything let go when a video stops |
| `src/conf.rs` | `afterglow.conf` (`key = value`); unknown keys ignored, so a config written for the C++ host works |
| `src/host.rs` | what the link asks of a platform |
| `src/platform/windows/` | DXGI desktop duplication with the pointer drawn in (`capture.rs`), the D3D11 video processor and Media Foundation's H.264 encoder (`encode.rs`), SendInput (`input.rs`), the app in a job in the console user's session (`process.rs`), SudoVDA (`vdisplay.rs`), the app's windows kept to its display (`wm.rs`), display modes and the primary display (`display.rs`), and the service (`svc.rs`) |
| `src/platform/linux/` | an Xvfb per app with its cookie, RandR resizing and a window manager that keeps the app's windows filling it (`xserver.rs`); MIT-SHM capture driven by XDamage, the pointer from XFixes, BGRX to I420 (`capture.rs`); OpenH264, XTest and the app as a process group (`mod.rs`) |
| `tests/link.rs` | the link on a real socket against a stand-in platform |

## On Windows

`afterglow.exe <afterglow.conf>` serves the link on `127.0.0.1:<phos_link_port>`. As a service, `tools\afterglowsvc.exe "<afterglow.conf>"` runs as SYSTEM and keeps `afterglow.exe` running as SYSTEM in the session at the console, restarting it when it exits or the console moves; stopping the service asks it to stop (Ctrl-C), so the app is closed and its display removed.

An app is started as the console's signed-in user (a limited token for an admin under UAC), in a job so everything it starts is followed. With SudoVDA installed and `virtual_display` asked for, it gets a display of its own at the viewer's size, made primary with `dd_configuration_option = ensure_primary`; a machine with no monitor streams. Its main windows are kept filling that display, over the taskbar, and its dialogs centred on it (`wm.rs`), so the video shows the app and not the desktop. The display follows each video's size and is removed with the app.

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

## On Linux

`afterglow <afterglow.conf>`, run by systemd as the user the apps run as (the client's `unitText`). It needs no desktop session. With `virtual_display` an app gets an Xvfb of its own: started at 3840x2160 with a random MIT-MAGIC-COOKIE-1 in a file only its user can read, set to the viewer's size with RandR, and run by a window manager inside afterglow that makes the app's windows fill it, centres its dialogs and gives it the keyboard. Without one, it runs on `$DISPLAY`, the session's own.

Capture waits on XDamage, so nothing is read while nothing is drawn, then reads the screen through MIT-SHM, shared memory with the X server and no copy over its socket. The pointer comes from XFixes and is drawn in. Frames are converted to I420 (BT.709 limited) and encoded by OpenH264 in screen-content real-time mode on up to four threads. Input goes through XTest; text the keyboard map lacks gets a spare keycode for the moment it's typed. Stopping (SIGTERM) asks the app's process group to end, kills it after 5 s, then its Xvfb.

| Key | Value |
| --- | --- |
| `xvfb` | the Xvfb to run; `Xvfb` on the `PATH` when unset |

## Building

```bash
cargo test
cargo build --release
```

Each builds its own platform's host. From Linux, `--target x86_64-pc-windows-gnu` cross-builds the Windows one with mingw-w64 installed, and wine runs its tests (`CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine`). The Linux build needs a C++ compiler (OpenH264 is built from source) and, optionally, `nasm` for its assembly.

`scripts/stage.sh OUT_DIR VERSION [TARGET]` tests, builds and stages a release as the zip lays it out. The SudoVDA driver package isn't built here: the release workflow takes it from Apollo's installer and checks it against its signed catalog.

## Licence

GPL-3.0-only ([`LICENSE`](LICENSE), [`NOTICE`](NOTICE)), as Apollo and Sunshine. Each build's `SOURCE.txt` names the repository and commit it was built from. The TypeScript client in the rest of this repo is Apache-2.0 and contains none of this code.
