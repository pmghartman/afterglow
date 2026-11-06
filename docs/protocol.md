# The afterglow link

One TCP connection on `127.0.0.1`, from the driver (`src/afterglow/link.ts`) to afterglow (`host/src/link.rs`). The framing is `host/src/wire.rs`, tested with it; `host/tests/link.rs` serves the link on a real socket.

afterglow serves the link on the port its config's `phos_link_port` names, with `phos_link_token` as the token. It serves nothing else and captures no audio.

Or `afterglow --stdio` serves one driver on its stdin and stdout, the same messages either way: what Phosphorus runs over ssh, as the viewer's login on a Linux host. There's no port and no token (ssh let the driver in; any `hello` is taken); stdout carries nothing but the link, the log goes to stderr (warnings and worse, or `AFTERGLOW_LOG`'s level) or `AFTERGLOW_LOG_FILE`. When stdin closes, or on SIGTERM, the app is ended and afterglow exits. `afterglow --features` prints `afterglow <version> proto=<n>` and what it serves (`stdio argv notes clipboard raise close cursor processes delete`, and `icons` and `attach` on Windows), for a driver probing a host.

## Protocol number

`proto` is the link's version, an integer: 1 now. `ready` carries it (`proto`) and `--features` prints it (`proto=1`). It bumps on any incompatible change to the messages: framing, a message's type or body, or the meaning of a field. Adding a message, a field or a feature word is compatible and leaves it. A driver treats a missing `proto` as 1 and refuses a number outside the range it speaks. Releases state it too, as `proto` in the signed `afterglow.json`.

## Framing

```
u32 length (little-endian, of what follows, 1 to 64 MiB) | u8 type | body
```

A length outside that range breaks the link. One driver at a time: a new connection that says the token takes over and the old one is closed. The app runs on between connections and videos. JSON bodies are UTF-8 objects. Every `u16` below is little-endian.

## Driver to afterglow

| Type | Name | Body |
| --- | --- | --- |
| `0x01` | `hello` | the token, as text. Must be first; anything else, or a wrong token, closes the connection |
| `0x02` | `launch` | JSON `{cmd, argv, name, w, h, fps, dpi, scale, dbus, virtual_display, fresh}`: the app's path (Windows: a `.exe`, `.lnk` or `.url`; Linux: a program), or on Linux `argv`, the program and its arguments; its window size, the capture rate, the display's dpi and the viewer's pixel ratio (1.5 and up: GTK and Qt told to scale), on Linux `dbus` for a session bus of its own; whether it gets a display of its own; `fresh` starts it again even when it runs; on Windows `login` `{user, password}` signs in at the logon screen first when no one is signed in: the password typed once for the account the screen offers (`user` is for the log) and the launch waits up to a minute for the session. Never logged |
| `0x03` | `video` | JSON `{gen, w, h, fps, kbps, yuv444, draw_cursor}`: start a video, replacing any running. One that differs from the running video only in `kbps` (and `gen`) carries it on where the host can: the same capture at the new bitrate, its `config` and a keyframe first under the new `gen`, with no pause to set a capture up again. `draw_cursor` draws the pointer into the frames; without it the pointer's shape comes as `cursor` messages |
| `0x04` | `stop_video` | none |
| `0x05` | `idr` | none: the next frame is a keyframe |
| `0x06` | `input` | one input packet (below); ignored while no video runs |
| `0x07` | `quit` | none: end the app |
| `0x08` | `ping` | `u32`, echoed in `pong` |
| `0x09` | `clip` | the viewer's clipboard, UTF-8: the app's own paste pastes it |
| `0x0a` | `raise` | `u32` LE: a window from a `windows` note to the front |
| `0x0b` | `close` | none: ask the app to close as its own close button does (it may ask to save first); `exited` follows when it has. Where the host can't, as `quit` |
| `0x0c` | `processes` | JSON `{all, id}`: the user's processes, answered by `processes` (see [The desktop](#the-desktop)) |
| `0x0d` | `attach` | JSON `{pid, started, shell, name, w, h, fps, dpi, scale, virtual_display}`: a process already running taken as the app, nothing started; answered by `launched` |
| `0x0e` | `end` | JSON `{pid, started, kill, shell, wait_ms, id}`: one of the user's processes asked to close, or killed; answered by `ended` |
| `0x0f` | `delete` | JSON `{path, permanent, id}`: a file or folder in the user's folders deleted, to the Recycle Bin (Linux: the trash) unless `permanent`; answered by `deleted` |

## afterglow to driver

| Type | Name | Body |
| --- | --- | --- |
| `0x81` | `ready` | JSON `{version, proto, virtual_display, hostname, running, features}`, after a good `hello`. `features`: what the host's desktop can be asked, some of `processes`, `attach`, `delete` |
| `0x82` | `launched` | JSON `{ok, resumed, attached, ms, features}` or `{ok: false, attached, error, code}`, for a `launch` or an `attach` (`attached: true`). `features`: what this app can be asked besides input, some of `notes`, `clipboard`, `raise`, `close`, then what the desktop can (as `ready`'s) |
| `0x83` | `exited` | JSON `{code, log, error}`, any of them: the app is gone, its exit code, the last of what it wrote, why when it never showed a window |
| `0x84` | `video_ended` | JSON `{gen, why}`: the capture stopped by itself |
| `0x85` | `note` | JSON: `{t: "title", s}` the main window's title, `{t: "windows", list: [[id, title]], main}` the app's windows, `{t: "clip", s}` what the app copied. Each as it changes, and all again when a driver comes |
| `0x86` | `processes` | JSON `{ok, id, list}` or `{ok: false, id, error}` |
| `0x87` | `ended` | JSON `{ok, id, pid, how, gone}` or `{ok: false, id, pid, error}` |
| `0x88` | `pong` | `u32` |
| `0x89` | `deleted` | JSON `{ok, id, path, recycled}` or `{ok: false, id, path, error}` |
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

With a display of its own, afterglow's window manager runs the app's windows: the main ones fill the display, a dialog (transient, or typed dialog, utility, splash, toolbar or notification) or a window whose size hints can't reach the display's size keeps its size, centred, and the newest has the keyboard. It speaks EWMH, so a toolkit lays a filled window out as maximized (no client-side frame or resize border; GTK's restore button), takes fullscreen when the app asks for it, and frames nothing. It holds the display before the app starts, so no window gets past it. `notes` tell the driver the title and the windows, `raise` brings one forward, `close` sends the topmost `WM_DELETE_WINDOW` (or kills its client where it takes none). The viewer's `clip` is owned as CLIPBOARD and PRIMARY, so Ctrl+V and Shift+Insert paste it; what the app copies to CLIPBOARD comes back as a `clip` note.

The app runs with X11 asked of every toolkit (`GDK_BACKEND=x11`, `QT_QPA_PLATFORM=xcb` and the like, no Wayland variables), from its home when started by `argv`, its output kept for `exited`. It's running while its process group is, or while a window of it is up; a launcher that exits after starting it has 3 s to show one.

## Displays

With `virtual_display` the app gets a display of its own at `w` by `h`: on Windows from SudoVDA (made the primary display with `dd_configuration_option = ensure_primary`, through the display configuration API), its main windows kept filling it and over the taskbar, its dialogs centred on it (its windows are its job's processes', and for its first seconds, until one of those shows, those of any process started since the launch: an app already running that takes the launch), on Linux an Xvfb that only afterglow and the app hold the cookie for, run by a window manager inside afterglow that keeps the app's windows filling it and gives it the keyboard. Each `video` sets that display to its own size first, so the app redraws at the viewer's (on Windows, a size SudoVDA has no mode for gets the display added again at it, the app's windows following it there). Without one, the app runs on the console's primary display (Windows) or the session's `$DISPLAY` (Linux), scaled into the video. `ready`'s `virtual_display` says whether this host can give one.

## Generations

Each `video` carries a `gen` (16 bits, wrapping). `config`, `frame` and `video_ended` carry the gen of the video they belong to, and the driver drops any whose gen isn't the latest it asked for, so frames of a video already replaced never reach a decoder.

## The pointer

Without `draw_cursor` the host never draws its pointer into a frame. Its shape goes out as a `cursor` message at the start of each video and whenever it changes, for the viewer to show as its own pointer (a CSS cursor, say): a move costs no frame and is seen with no latency. What the host's pointer hovers still changes in the frames. A shape that inverts the screen under it, which a PNG can't say, comes black. With `draw_cursor` the shape is drawn in where the pointer is, and every move is a frame.

## Frames

A `frame` message's first 8 bytes, counting its type byte, are the room `FrameRoom` names: a driver overwrites them with its own header and sends the message on as it is. What follows is AVCC (each NAL unit behind a 4-byte big-endian length), what WebCodecs' `VideoDecoder` takes with the `config` message's avcC as its description. afterglow writes each encoded frame straight into this layout once; the driver copies it once, out of the socket's bytes.

The avcC's SPS always carries VUI `bitstream_restriction` with `max_num_reorder_frames = 0` (afterglow rewrites what the encoder made, the rest of it bit for bit). Without it a decoder infers a reorder depth from the DPB's size and holds decoded pictures back: Chromium's own decoder held two, hardware decoders hold up to sixteen. With it each frame is shown the moment it's decoded. No stream afterglow makes has B-frames, so the restriction is true.

A frame is made when the screen changes, within the video's rate (the first change after a quiet spell at once), and again once a second when nothing does. `idr` and `stop_video` take effect at once, not at the next frame.

The stream afterglow's own encoder makes is Constrained Baseline (`codecOf` gives its codec string from the `config`): one reference, no B-frames, no deblocking, each frame one or more slices of whole macroblock rows. A frame codes what changed and skips the rest, so its size follows the change. A change comes at the quality two frames' share of `kbps` allows, and once the screen has been still for 60 ms further frames, each a frame's share, code what was coarse again until it's sharp: a still screen's video goes on for a moment after the last change, then stops but for the once-a-second frame.

## Icons

`afterglow --icons` is not part of the link: it is a batch extractor for app icons, run as the viewer's login (over ssh, for Phosphorus). A build that has it lists `icons` in `--features` (Windows; a Linux build lists nothing and `--icons` answers every spec with `null`).

stdin is the specs, either one per line (blank lines skipped) or one JSON array of strings. stdout is one JSON line per spec, `{"k": <the spec as given>, "png": <base64 PNG> | null}`, written as each finishes (so not in input order; match on `k`). The icon is 48x48 RGBA with alpha. A miss, an unknown spec or a file that isn't there is `"png": null` and never an error; the exit code is 0 when the input was read, 2 for a malformed JSON array. Specs run on a few threads at once; `AFTERGLOW_ICON_THREADS` sets how many (1 to 32).

| Spec | Icon |
| --- | --- |
| `f:<path>` | the file's shell icon. A OneDrive or other cloud placeholder (offline, recall on open or on data access) is read by its attributes only and never hydrated |
| `a:<path>` | the icon for the path's file attributes and extension, the file never touched |
| `b:` | the recycle bin |
| `u:<AUMID>` | a Store app's tile logo: the `Square44x44Logo` of `<family>!<app>` in its package manifest, the 48px unplated file first, then 48px, scale 200, scale 100, any. The file's own PNG bytes, not resized |
| `l:<path>,<index>` | an icon resource (`.exe`, `.dll`, `.ico`) at 48px. The index may be negative (a resource id); without `,<index>` it is 0. Environment variables are not expanded |

## The desktop

Besides the app, a driver can see the user's processes, take one already running as the app, end one, and delete a file the desktop shows (an icon's file, say, as `--icons` drew it). A host that can lists the words in `ready`'s and `launched`'s `features` and in `--features`: `processes` (`processes` and `end`), `attach` (Windows), `delete`. A host without them answers each with `ok: false`. Like everything on the link, they're served only after a good `hello`: the token on TCP, or over `--stdio`, where ssh let the driver in.

`processes`, `end` and `delete` are each answered on their own once done, while input and video go on, with the ask's `id` (any JSON value) echoed so a driver with several out matches each reply to its ask (`Link.processes`, `Link.end`, `Link.delete` do).

Everything is done with the console user's rights and touches nothing else. On Windows afterglow runs as SYSTEM under the service: their processes are those whose token is the console user's, in the console's session; a process is opened to be ended while impersonating them, so what they couldn't end, afterglow can't; a delete is done by `afterglow --delete` started as them, so the Recycle Bin is theirs. On Linux afterglow runs as the user (over ssh, or as their service), and their processes are those whose owner they are. Each action is logged (what was asked, of what, and how it went; never the token).

### `processes`

`{all}`: without `all`, the processes with a visible top-level window, a main window or dialog of an app (Windows: not a tool window, not cloaked; Linux: those the window manager manages on the app's display and the session's `$DISPLAY`, by `_NET_WM_PID` or, where an app set none, the X server's own word for its client); with `all`, every process of the user's after them. The list's entries:

| Field | |
| --- | --- |
| `pid`, `ppid` | the process and its parent |
| `exe` | its executable's full path; empty when it can't be read |
| `name` | its executable's file name (Windows) or its command name (Linux, as `ps` shows) |
| `started` | when it started, ms since the Unix epoch: with the pid, which process it is |
| `windows` | `[[id, title]]`, topmost first: the HWND on Windows, the X window on Linux |

Processes with windows come first, in the order of their topmost window, then the rest by name.

### `attach`

`{pid, started, shell}` and a launch's own fields (`name`, `w`, `h`, `fps`, `dpi`, `scale`, `virtual_display`): the process (one of the user's, with a window up) becomes the app, as if it had been launched, and nothing is started. Any app before it is ended (or, attached, let go) first. With `virtual_display` its windows are moved to a display of its own and kept there as a launched app's are (main windows fill it, dialogs centred); without, the console's display is streamed. It's answered by `launched`, `attached: true`, its `features` including `close`. Windows only.

It's running while the process is (and, on a display of its own, while a window of it is up there). Not started by afterglow, it's never ended by it unasked: `quit`, a new `launch` or `attach`, or afterglow stopping lets it go, its windows put back where they were and the display removed, still running. `close` asks it to close (`WM_CLOSE` to its windows); `end` ends it. Either way `exited` follows when it's gone.

### `end`

`{pid, started, kill, shell, wait_ms}`. Without `kill`, asked to close as its close buttons would: on Windows `WM_CLOSE` to its windows (refused when it has none: ask with `kill`), on Linux `_NET_CLOSE_WINDOW` through the display's window manager (afterglow's own takes it too), or `WM_DELETE_WINDOW` to each window where there's none, and `SIGTERM` for a process with no window. With `kill`, ended at once (`TerminateProcess`; `SIGKILL`). Either may be refused by the process: it may ask to save first, or ignore `SIGTERM`. `wait_ms` (default 5000, 2000 with `kill`, at most 30000) is how long afterglow waits for it to go before it answers.

`ended` says `how` (`closed`, `terminated`, `killed`) and whether it was `gone` by the end of the wait. With `started` (from the list), a process by that pid that started at another time isn't the one meant and is left alone (`pid N is another process now`).

Never ended: a process that isn't the user's (another user's, SYSTEM's, root's); one of the session's own (`csrss`, `winlogon`, `lsass`, `lsaiso`, `dwm`, `smss`, `services`, `wininit`, `fontdrvhost`, `logonui`, `sihost`, `svchost`, `userinit`; `systemd`, `sshd`, `dbus-daemon`, the X server, the session manager); afterglow itself, and on Linux what it runs under (ending ssh's shell would cut the link). The desktop shell (`explorer.exe`, `gnome-shell`, `plasmashell`, `kwin`, ...) only with `shell: true`. The same goes for `attach`.

### `delete`

`{path, permanent}`: an absolute path, nothing `.` or `..` in it. Without `permanent` it goes to the Recycle Bin (Windows: the shell's own delete, `IFileOperation` with `FOFX_RECYCLEONDELETE`, as Explorer's), or on Linux to the user's trash as a file manager puts it there (`$XDG_DATA_HOME/Trash`, a `.trashinfo` beside it). Where there's no bin (a network share) or the trash is on another filesystem, it's refused: ask with `permanent`. On Windows, something too big for the Recycle Bin brings up the shell's own question on the host's screen before it would be deleted for good. With `permanent`, it's deleted for good, a folder with all it holds. `deleted` says whether it was `recycled`.

Only what's within the user's folders: on Windows their profile, their Desktop (wherever it's redirected) and the Public Desktop; on Linux their home. The config's `delete_roots` adds folders (a list as `PATH` is: `;` between on Windows, `:` on Linux; over `--stdio`, `AFTERGLOW_DELETE_ROOTS`). The path's folder is resolved first (links, junctions, short names) and the item itself isn't: a link is deleted, not what it names. One of those folders itself, or what holds one, is never deleted.

## Security

The link listens on loopback only. The token (16 characters or more; `phos_link_token`) is, as Phosphorus writes it, 48 hex characters from a CSPRNG, written by the installer into a folder only SYSTEM and Administrators can read. Anyone holding it can launch apps and type into the console's session, end the console user's processes and delete their files, so treat it as a credential.
