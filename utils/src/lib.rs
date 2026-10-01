// Thin FFI marshalling wrappers around libwayland-client: every unsafe fn
// here shares one contract (valid wayland proxies, the protocol's threading
// rules) documented on the module — per-fn # Safety sections would repeat
// the same sentence 20 times.
#![allow(clippy::missing_safety_doc)]

pub mod vk;
pub mod wlproto;
