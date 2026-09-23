//! `gndrm-hostd` — host-side test daemon (B9 harness, also used by phase-3
//! bring-up scripts): serves one `Device` over a Unix socket, exactly like
//! the in-app `gndrm-android` loop, minus anything Android.
//!
//! Usage: `gndrm-hostd <socket-name>`
//!   "/abs/path" → filesystem socket; "@name"/"name" → abstract namespace.
//!
//! Connection contract: the first accepted connection is the RPC channel,
//! the second is the event channel (matches `gndrm-shim`'s connect order).

#![deny(clippy::all)]

use gndrm_daemon::server;
use gndrm_daemon::{Device, NullPresenter, PanelDesc};

fn main() {
    let name = std::env::args()
        .nth(1)
        .expect("usage: gndrm-hostd <socket-name>");
    if name.starts_with('/') {
        let _ = std::fs::remove_file(&name);
    }
    let listener = server::listen_seqpacket(&name).unwrap();
    let log = |msg: &str| eprintln!("[gndrm-hostd] {msg}");
    log(&format!("listening on {name} (SOCK_SEQPACKET)"));

    // First connection = RPC, second = events (wire contract).
    let rpc = server::accept(listener).unwrap();
    let events = server::accept(listener).unwrap();
    log("client connected (rpc + events)");

    let mut dev = Device::new(PanelDesc::default(), Box::<NullPresenter>::default());
    server::serve(&mut dev, rpc, events, &log);
}
