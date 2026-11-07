// @phosphorus/afterglow/glow: the viewer's end of glow (docs/glow.md), afterglow's
// frames and input straight between the host and a page over WebTransport. Safe to
// bundle for a page or its worker: no Bun, no Node

export { codecOf } from "../afterglow/link.ts";
export { Assembly, cut, Flag, HeadLen, Kind, parityCount, parseHead } from "./fec.ts";
export { Receiver, type ReceiverOut, type ReceiverStats, Up } from "./receiver.ts";
export { type GlowEvents, type GlowOptions, type GlowStats, type GlowTicket, GlowViewer, Welcome } from "./viewer.ts";
