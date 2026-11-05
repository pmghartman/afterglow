// @phosphorus/afterglow: streaming a Windows host's apps to a viewer. Two sources, one
// contract (source.ts):
//   AfterglowSource drives afterglow (host/), the fork of Apollo, over one loopback link
//   GlowSource drives a host's own Sunshine or Apollo over GameStream (glow/)
// Each hands back H.264 as AVCC with room for the driver's header in front

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
} from "./afterglow/config.ts";
export {
  configOf,
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

export { type Identity, makeIdentity, signatureOf } from "./glow/cert.ts";
export {
  addClient,
  addLauncher,
  filesOf,
  GlowHost,
  launcherName,
  type Platform,
  parseConf,
  unixLike,
  windows,
} from "./glow/host.ts";
export { InputPacker } from "./glow/input.ts";
export { confOf, type Instance, startInstance } from "./glow/instance.ts";
export { type App, apps as hostApps, cancel, type Host, pair, serverInfo } from "./glow/lumen.ts";
export { avccOf, Depacketizer, nalsOf } from "./glow/photon.ts";
export { GlowSource, type GlowTarget } from "./glow/source.ts";
export { pack, Sealer } from "./glow/spark.ts";
export { asText, utf8Chunks, vkOf } from "./glow/vk.ts";

export { FrameRoom, type Opened, type Sink, type Source, type Start, type Video, type VideoSink } from "./source.ts";
