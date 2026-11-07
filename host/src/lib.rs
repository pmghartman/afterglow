//! afterglow: a host's apps captured, encoded as H.264 and driven over one loopback
//! link (docs/protocol.md). The link and its framing are portable; capture, encoding,
//! input and app launching are each platform's (platform/).

pub mod log;

pub mod avc;
pub mod conf;
pub mod convert;
pub mod desk;
pub mod glow;
pub mod h264;
pub mod host;
pub mod icons;
pub mod input;
pub mod link;
pub mod platform;
pub mod png;
pub mod wire;
