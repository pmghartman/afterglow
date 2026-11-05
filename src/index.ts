// @phosphorus/afterglow: a host's native apps streamed to a viewer as low-latency H.264.
// AfterglowSource drives afterglow (host/, Windows and Linux) over one loopback link
// (docs/protocol.md) and keeps the contract a driver expects of a source (source.ts):
// H.264 as AVCC with room for the driver's header in front, input back

export {
  type AfterglowPaths,
  appsText,
  confText,
  driverVer,
  linkPort,
  linkText,
  pathsOf,
  readLink,
  svcBinPath,
  tokenOf,
  unitText,
} from "./afterglow/config.ts";
export { asText, InputPacker, modOf, pack, vkOf } from "./afterglow/input.ts";
export {
  configOf,
  cursorOf,
  frameOf,
  In,
  jsonOf,
  Link,
  type LinkAddr,
  type LinkEvents,
  LinkReader,
  maxLen,
  message as linkMessage,
  Out,
  type Ready,
} from "./afterglow/link.ts";
export { AfterglowSource, type AfterglowTarget } from "./afterglow/source.ts";

export { FrameRoom, type Opened, type Sink, type Source, type Start, type Video, type VideoSink } from "./source.ts";
