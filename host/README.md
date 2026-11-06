# afterglow host

The streaming host, in Rust, for Windows and Linux: one app at a time launched, its display captured and encoded as H.264, the viewer's input injected, all driven over one loopback link ([`docs/protocol.md`](../docs/protocol.md)). The Windows side was ported from [Apollo](https://github.com/ClassicOldSong/Apollo) (itself a fork of LizardByte's [Sunshine](https://github.com/LizardByte/Sunshine)), keeping only what the link uses: no GameStream servers, web UI, tray, audio, gamepads, touch or pen.

## Layout

| Path | What it is |
| --- | --- |
| `src/wire.rs` | the link's framing: frames shaped into AVCC behind the driver's room, the pointer's message, no platform code |
| `src/avc/` | afterglow's own H.264 encoder: only the macroblocks that changed coded, inter with a zero vector or Intra 16x16, every other one skipped; transforms and quantisation for all of a macroblock's blocks side by side (AVX2), CAVLC, the reconstruction kept as FFmpeg decodes it (tested); row slices on every core; the rate's qp from what a change has to say, refinement once still (`mod.rs`, `mb.rs`, `bits.rs`, `tables.rs`, `tests.rs`) |
| `src/convert.rs` | BGRX to I420 with AVX2, only what changed, big rectangles across cores; scaled for a display of another size |
| `src/h264.rs` | Annex B split, the avcC record, every SPS rewritten so nothing is held back for reordering |
| `src/png.rs` | the pointer's shape as a PNG |
| `src/link.rs` | the link: the token, one driver at a time, launch, videos and their generations, input, the app's exit |
| `src/input.rs` | input packets into events; held keys repeated, everything let go when a video stops |
| `src/conf.rs` | `afterglow.conf` (`key = value`); unknown keys ignored, so a config written for the C++ host works |
| `src/host.rs` | what the link asks of a platform |
| `src/platform/windows/` | DXGI desktop duplication with the pointer as shapes or drawn in, its dirty rectangles mirrored into memory the CPU reads (`capture.rs`), the D3D11 video processor and Media Foundation's H.264 encoder, asked for (`encode.rs`), SendInput (`input.rs`), the app in a job in the console user's session (`process.rs`), SudoVDA (`vdisplay.rs`), the app's windows kept to its display (`wm.rs`), display modes and the primary display (`display.rs`), and the service (`svc.rs`) |
| `src/platform/linux/` | an Xvfb per app with its cookie, its framebuffer on `/dev/shm` and RandR resizing (`xserver.rs`); the window manager that keeps the app's windows filling it, its titles, windows, close and clipboard (`wm.rs`); XDamage rectangles read in place from that framebuffer or with MIT-SHM, the pointer from XFixes (`capture.rs`); OpenH264 through its C API, asked for (`encode.rs`); XTest, a spare Xvfb and the app as a process group (`mod.rs`) |
| `tests/link.rs` | the link on a real socket against a stand-in platform |

## On Windows

`afterglow.exe <afterglow.conf>` serves the link on `127.0.0.1:<phos_link_port>`. As a service, `tools\afterglowsvc.exe "<afterglow.conf>"` runs as SYSTEM and keeps `afterglow.exe` running as SYSTEM in the session at the console, restarting it when it exits or the console moves; stopping the service asks it to stop (Ctrl-C), so the app is closed and its display removed. It starts afterglow at once, and waits longer each time (to a minute) while afterglow keeps exiting soon after it starts.

An app is started as the console's signed-in user (a limited token for an admin under UAC), in a job so everything it starts is followed. With SudoVDA installed and `virtual_display` asked for, it gets a display of its own at the viewer's size, made primary with `dd_configuration_option = ensure_primary`; a machine with no monitor streams. Its main windows are kept filling that display, over the taskbar, and its dialogs centred on it (`wm.rs`), so the video shows the app and not the desktop. The display follows each video's size (SudoVDA offers the size a display was added at and a few standard ones, so for any other it's added again at that size and the app's windows follow it) and is removed with the app. While a video runs the machine and its displays are kept on, and at its start displays that had gone off are woken, so a locked console streams its lock screen rather than black.

Capture runs on whichever desktop has the input, so UAC prompts and the lock screen are streamed (the window follower works on the user's own desktop whatever has the input). The pointer's shape goes to the viewer as it changes (or, asked, is drawn in), so a move is no frame and no compose. The capture's D3D device runs at the GPU's highest thread priority, so a game filling the GPU doesn't queue it, and the video thread is MMCSS "Capture" work. Each frame duplication hands over, its dirty rectangles and its moves' destinations are copied on the GPU into a staging texture that mirrors the desktop; a change maps it once and reads only those rectangles, converted to I420 (BT.709 limited range) and coded by afterglow's own encoder (`src/avc`, its slice threads at the highest thread priority so a busy app doesn't hold a frame): a typed glyph is a few hundred bytes across the bus and microseconds of encoding, and no hardware encoder holds a frame back. A display that isn't the video's size is read whole and scaled. With `encoder = hardware`, or the pointer drawn in, frames go through the GPU's video processor (BGRA to NV12, scaled and letterboxed) into the encoder Media Foundation lists for that GPU (NVENC, AMF or Quick Sync register one) in low latency mode, CBR at the asked bitrate; with `encoder = software`, Windows' own. H.264 4:4:4 isn't offered, so `yuv444` gets 4:2:0.

| Key | Value |
| --- | --- |
| `phos_link_port` | the link's port on `127.0.0.1` |
| `phos_link_token` | what a driver says first, 16 characters or more |
| `log_path` | the log file, truncated each start |
| `min_log_level` | `debug`, `info`, `warning` or `error` |
| `dd_configuration_option` | `ensure_primary`: an app's virtual display is the primary one while it streams |
| `adapter_name` | the GPU SudoVDA renders on, by name |
| `encoder` | `hardware`: Media Foundation's encoder for the GPU rather than afterglow's own; `software`: Media Foundation's software one |

The service runs as SYSTEM; keep the config and token readable by SYSTEM and Administrators only.

## On Linux

`afterglow <afterglow.conf>`, run by systemd as the user the apps run as (the client's `unitText`); or `afterglow --stdio`, one app for one driver on stdin and stdout, as Phosphorus runs it over ssh as the viewer's login. It needs no desktop session. With `virtual_display` an app gets an Xvfb of its own: started at 3840x2160 with a random MIT-MAGIC-COOKIE-1 in a file only its user can read, set to the viewer's size with RandR, and run by a window manager inside afterglow that makes the app's windows fill it, centres its dialogs and gives it the keyboard. Without one, it runs on `$DISPLAY`, the session's own.

Capture sleeps on the X connection until XDamage reports drawing, as rectangles that come as events, no round trip to ask what changed. Only those rectangles are read: on an app's own Xvfb straight out of its framebuffer, a file on `/dev/shm` (`-fbdir`) mapped read-only, with no request and no copy (the damage subtract is synced first, so no drawing slips between a read and the next report); on the session's display through MIT-SHM. They're converted into an I420 frame (BT.709 limited) kept between frames, by an AVX2 kernel where the CPU has it, big ones across cores. The pointer's shape comes from XFixes as it changes, and is sent to the viewer (Xvfb's own is hidden, as it draws it into the framebuffer) or drawn in. Afterglow's own encoder (`src/avc`) codes only the macroblocks those rectangles cover whose pixels changed; every other one is a skip. A change's qp is the finest that keeps its frame near two frames' share of the bitrate; once the screen is still for 60 ms, what was coded coarsely is coded again (to qp 26 where it can't be read yet, then 14), a frame's share at a time. With `encoder = openh264`, OpenH264 encodes the whole frame through its C API instead: camera real-time mode, low complexity, a slice per thread. A spare Xvfb is kept started, so a launch doesn't wait for one. Input goes through XTest; text the keyboard map lacks gets a spare keycode for the moment it's typed. Stopping (SIGTERM) asks the app's process group to end, kills it after 5 s, then its Xvfb.

| Key | Value |
| --- | --- |
| `xvfb` | the Xvfb to run; `Xvfb` on the `PATH` when unset |
| `encoder` | `openh264`: OpenH264 rather than afterglow's own encoder (`AFTERGLOW_ENCODER` over `--stdio`) |

## Building

```bash
cargo test
cargo build --release
```

Each builds its own platform's host. From Linux, `--target x86_64-pc-windows-gnu` cross-builds the Windows one with mingw-w64 installed, and wine runs its tests (`CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine`). The Linux build needs a C++ compiler (OpenH264 is built from source) and, optionally, `nasm` for its assembly.

`scripts/stage.sh OUT_DIR VERSION [TARGET]` tests, builds and stages a release as the zip lays it out. The SudoVDA driver package isn't built here: the release workflow takes it from Apollo's installer and checks it against its signed catalog.

## Licence

GPL-3.0-only ([`LICENSE`](LICENSE), [`NOTICE`](NOTICE)), as Apollo and Sunshine. Each build's `SOURCE.txt` names the repository and commit it was built from. The TypeScript client in the rest of this repo is Apache-2.0 and contains none of this code.
