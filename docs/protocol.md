# The afterglow link

One TCP connection on `127.0.0.1`, from the driver (`src/afterglow/link.ts`) to afterglow (`host/src/link.rs`). The framing is `host/src/wire.rs`, tested with it; `host/tests/link.rs` serves the link on a real socket.

afterglow serves the link on the port its config's `phos_link_port` names, with `phos_link_token` as the token. It serves nothing else and captures no audio.

## Framing

```
u32 length (little-endian, of what follows, 1 to 64 MiB) | u8 type | body
```

A length outside that range breaks the link. One driver at a time: a new connection that says the token takes over and the old one is closed. The app runs on between connections and videos. JSON bodies are UTF-8 objects. Every `u16` below is little-endian.

## Driver to afterglow

| Type | Name | Body |
| --- | --- | --- |
| `0x01` | `hello` | the token, as text. Must be first; anything else, or a wrong token, closes the connection |
| `0x02` | `launch` | JSON `{cmd, name, w, h, fps, virtual_display, fresh}`: the app's path or shortcut, its window size, the capture rate, whether it gets a virtual display of its own |
| `0x03` | `video` | JSON `{gen, w, h, fps, kbps, yuv444}`: start a video, replacing any running |
| `0x04` | `stop_video` | none |
| `0x05` | `idr` | none: the next frame is a keyframe |
| `0x06` | `input` | one GameStream input packet (`host/src/input.rs`): mouse moves, buttons, wheels, keys as US-layout virtual keys, UTF-8 text; ignored while no video runs |
| `0x07` | `quit` | none: ask the app to close |
| `0x08` | `ping` | `u32`, echoed in `pong` |

## afterglow to driver

| Type | Name | Body |
| --- | --- | --- |
| `0x81` | `ready` | JSON `{version, virtual_display, hostname, running}`, after a good `hello` |
| `0x82` | `launched` | JSON `{ok, resumed, ms}` or `{ok: false, error, code}` |
| `0x83` | `exited` | JSON `{}`: the app is gone |
| `0x84` | `video_ended` | JSON `{gen, why}`: the capture stopped by itself |
| `0x88` | `pong` | `u32` |
| `0x90` | `config` | `[pad][u16 gen][u16 w][u16 h]` then the avcC record |
| `0x91` | `frame` | `[u8 key][u16 gen][pad 4]` then the frame as AVCC |

## Generations

Each `video` carries a `gen` (16 bits, wrapping). `config`, `frame` and `video_ended` carry the gen of the video they belong to, and the driver drops any whose gen isn't the latest it asked for, so frames of a video already replaced never reach a decoder.

## Frames

A `frame` message's first 8 bytes, counting its type byte, are the room `FrameRoom` names: a driver overwrites them with its own header and sends the message on as it is. What follows is AVCC (each NAL unit behind a 4-byte big-endian length), what WebCodecs' `VideoDecoder` takes with the `config` message's avcC as its description. afterglow reshapes each encoded frame from Annex B once, writing it straight into this layout; the driver copies it once, out of the socket's bytes.

## Security

The link listens on loopback only. The token (16 characters or more; `phos_link_token`) is, as Phosphorus writes it, 48 hex characters from a CSPRNG, written by the installer into a folder only SYSTEM and Administrators can read. Anyone holding it can launch apps and type into the console's session, so treat it as a credential.
