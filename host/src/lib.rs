//! afterglow: a host's apps captured, encoded as H.264 and driven over one loopback
//! link (docs/protocol.md). The link and its framing are portable; capture, encoding,
//! input and app launching are each platform's (platform/).

pub mod log;

pub mod conf;
pub mod host;
pub mod input;
pub mod link;
pub mod platform;
pub mod wire;
