# The afterglow link

One TCP connection on `127.0.0.1`, from the driver (`src/afterglow/link.ts`) to afterglow (`host/src/link.rs`). The framing is `host/src/wire.rs`, tested with it; `host/tests/link.rs` serves the link on a real socket.

afterglow serves the link on the port its config's `phos_link_port` names, with `phos_link_token` as the token. It serves nothing else and captures no audio.

Or `afterglow --stdio` serves one driver on its stdin and stdout, the same messages either way: what Phosphorus runs over ssh, as the viewer's login on a Linux host. There's no port and no token (ssh let the driver in; any `hello` is taken); stdout carries nothing but the link, the log goes to stderr (warnings and worse, or `AFTERGLOW_LOG`'s level) or `AFTERGLOW_LOG_FILE`. When stdin closes, or on SIGTERM, the app is ended and afterglow exits. `afterglow --features` prints `afterglow <version>` and what it serves (`stdio argv notes clipboard raise close cursor`), for a driver probing a host.

## Framing

```
u32 length (little-endian, of what follows, 1 to 64 MiB) | u8 type | body
```

A length outside that range breaks the link. One driver at a time: a new connection that says the token takes over and the old one is closed. The app runs on between connections and videos. JSON bodies are UTF-8 objects. Every `u16` below is little-endian.

## Driver to afterglow

| Type | Name | Body |
| --- | --- | --- |
| `0x01` | `hello` | the token, as text. Must be first; anything else, or a wrong token, closes the connection |
| `0x02` | `launch` | JSON `{cmd, argv, name, w, h, fps, dpi, scale, dbus, virtual_display, fresh}`: the app's path (Windows: a `.exe`, `.lnk` or `.url`; Linux: a program), or on Linux `argv`, the program and its arguments; its window size, the capture rate, the display's dpi and the viewer's pixel ratio (1.5 and up: GTK and Qt told to scale), on Linux `dbus` for a session bus of its own; whether it gets a display of its own; `fresh` starts it again even when it runs |
| `0x03` | `video` | JSON `{gen, w, h, fps, kbps, yuv444, draw_cursor}`: start a video, replacing any running. `draw_cursor` draws the pointer into the frames; without it the pointer's shape comes as `cursor` messages |
| `0x04` | `stop_video` | none |
| `0x05` | `idr` | none: the next frame is a keyframe |
| `0x06` | `input` | one input packet (below); ignored while no video runs |
| `0x07` | `quit` | none: end the app |
| `0x08` | `ping` | `u32`, echoed in `pong` |
| `0x09` | `clip` | the viewer's clipboard, UTF-8: the app's own paste pastes it |
| `0x0a` | `raise` | `u32` LE: a window from a `windows` note to the front |
| `0x0b` | `close` | none: ask the app to close as its own close button does (it may ask to save first); `exited` follows when it has. Where the host can't, as `quit` |

## afterglow to driver

| Type | Name | Body |
| --- | --- | --- |
| `0x81` | `ready` | JSON `{version, virtual_display, hostname, running}`, after a good `hello` |
| `0x82` | `launched` | JSON `{ok, resumed, ms, features}` or `{ok: false, error, code}`. `features`: what this app can be asked besides input, some of `notes`, `clipboard`, `raise`, `close` |
| `0x83` | `exited` | JSON `{code, log, error}`, any of them: the app is gone, its exit code, the last of what it wrote, why when it never showed a window |
| `0x84` | `video_ended` | JSON `{gen, why}`: the capture stopped by itself |
| `0x85` | `note` | JSON: `{t: "title", s}` the main window's title, `{t: "windows", list: [[id, title]], main}` the app's windows, `{t: "clip", s}` what the app copied. Each as it changes, and all again when a driver comes |
| `0x88` | `pong` | `u32` |
| `0x90` | `config` | `[pad][u16 gen][u16 w][u16 h]` then the avcC record |
| `0x91` | `frame` | `[u8 key][u16 gen][pad 4]` then the frame as AVCC |
| `0x92` | `cursor` | `[u8 visible][u16 hot x][u16 hot y][u16 w][u16 h]` then the pointer's shape as a PNG (straight-alpha RGBA), nothing when hidden |

## Input

An `input` body is one packet: `u32` big-endian size of what follows it, `u32` little-endian kind, the event. Coordinates and amounts are big-endian `i16`. The host (`host/src/input.rs`) tracks what's held: a held key repeats after 500 ms every 40 ms, and everything held is let go when a video stops.

| Kind | Event | After the kind |
| --- | --- | --- |
| `0x03`, `0x04` | key down, up | `u8` flags, `u16` LE virtual key with `0x8000` set, `u8` modifiers (1 shift, 2 ctrl, 4 alt, 8 meta), `u16` 0 |
| `0x05` | pointer to | `x`, `y`, `0`, `w - 1`, `h - 1`: (x, y) within the video's w by h |
| `0x07` | pointer by | `dx`, `dy` |
| `0x08`, `0x09` | button down, up | `u8`: 1 left, 2 middle, 3 right, 4 and 5 back and forward |
| `0x0a` | wheel | amount (120 a notch, up positive), twice, then `0` |
| `0x55000001` | horizontal wheel | amount (right positive) |
| `0x17` | text | UTF-8, any length |

Keys are US-layout Windows virtual-key codes for the physical key: Windows sends them as scan codes, Linux maps them to X keysyms. A character the viewer's layout makes that US doesn't on that key goes as text.

## The app on Linux

With a display of its own, afterglow's window manager runs the app's windows: the main ones fill the display, a dialog (transient, or typed dialog, utility, splash, toolbar or notification) keeps its size, centred, and the newest has the keyboard. `notes` tell the driver the title and the windows, `raise` brings one forward, `close` sends the topmost `WM_DELETE_WINDOW` (or kills its client where it takes none). The viewer's `clip` is owned as CLIPBOARD and PRIMARY, so Ctrl+V and Shift+Insert paste it; what the app copies to CLIPBOARD comes back as a `clip` note.

The app runs with X11 asked of every toolkit (`GDK_BACKEND=x11`, `QT_QPA_PLATFORM=xcb` and the like, no Wayland variables), from its home when started by `argv`, its output kept for `exited`. It's running while its process group is, or while a window of it is up; a launcher that exits after starting it has 3 s to show one.

## Displays

With `virtual_display` the app gets a display of its own at `w` by `h`: on Windows from SudoVDA (made the primary display with `dd_configuration_option = ensure_primary`), its main windows kept filling it and over the taskbar, its dialogs centred on it, on Linux an Xvfb that only afterglow and the app hold the cookie for, run by a window manager inside afterglow that keeps the app's windows filling it and gives it the keyboard. Each `video` sets that display to its own size first, so the app redraws at the viewer's. Without one, the app runs on the console's primary display (Windows) or the session's `$DISPLAY` (Linux), scaled into the video. `ready`'s `virtual_display` says whether this host can give one.

## Generations

Each `video` carries a `gen` (16 bits, wrapping). `config`, `frame` and `video_ended` carry the gen of the video they belong to, and the driver drops any whose gen isn't the latest it asked for, so frames of a video already replaced never reach a decoder.

## The pointer

Without `draw_cursor` the host never draws its pointer into a frame. Its shape goes out as a `cursor` message at the start of each video and whenever it changes, for the viewer to show as its own pointer (a CSS cursor, say): a move costs no frame and is seen with no latency. What the host's pointer hovers still changes in the frames. A shape that inverts the screen under it, which a PNG can't say, comes black. With `draw_cursor` the shape is drawn in where the pointer is, and every move is a frame.

## Frames

A `frame` message's first 8 bytes, counting its type byte, are the room `FrameRoom` names: a driver overwrites them with its own header and sends the message on as it is. What follows is AVCC (each NAL unit behind a 4-byte big-endian length), what WebCodecs' `VideoDecoder` takes with the `config` message's avcC as its description. afterglow writes each encoded frame straight into this layout once; the driver copies it once, out of the socket's bytes.

The avcC's SPS always carries VUI `bitstream_restriction` with `max_num_reorder_frames = 0` (afterglow rewrites what the encoder made, the rest of it bit for bit). Without it a decoder infers a reorder depth from the DPB's size and holds decoded pictures back: Chromium's own decoder held two, hardware decoders hold up to sixteen. With it each frame is shown the moment it's decoded. No stream afterglow makes has B-frames, so the restriction is true.

A frame is made when the screen changes, within the video's rate (the first change after a quiet spell at once), and again once a second when nothing does. `idr` and `stop_video` take effect at once, not at the next frame.

## Security

The link listens on loopback only. The token (16 characters or more; `phos_link_token`) is, as Phosphorus writes it, 48 hex characters from a CSPRNG, written by the installer into a folder only SYSTEM and Administrators can read. Anyone holding it can launch apps and type into the console's session, so treat it as a credential.
