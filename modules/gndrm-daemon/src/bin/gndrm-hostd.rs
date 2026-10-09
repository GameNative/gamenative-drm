//! `gndrm-hostd` — host-side test daemon (B9 harness, also used by phase-3
//! bring-up scripts): serves one `Device` over a Unix socket, exactly like
//! the in-app `gndrm-android` loop, minus anything Android.
//!
//! Usage: `gndrm-hostd <socket-name>`
//!   "/abs/path" → filesystem socket; "@name"/"name" → abstract namespace.
//!
//! Two listeners, matching the two node types the daemon serves:
//!   - the configured `name` is the **card** node: the first accepted
//!     connection is the RPC channel, the second is the event channel (matches
//!     `gndrm-shim`'s connect order).
//!   - `name + "-render"` is the **render** node, served by
//!     [`server::serve_render_listener`] — the same per-client loop
//!     `libgamenative_drm.so` runs on device (Xwayland's glamor/dri3 open).

#![deny(clippy::all)]

use std::sync::Arc;
use std::thread;

use gndrm_daemon::server::{self, SharedLog};
use gndrm_daemon::{Device, NullPresenter, PanelDesc};

fn main() {
    let name = std::env::args()
        .nth(1)
        .expect(&gndrm_obf::obf!("usage: gndrm-hostd <socket-name>"));
    if name.starts_with('/') {
        let _ = std::fs::remove_file(&name);
    }
    let listener = server::listen_seqpacket(&name).unwrap();
    let render_name = server::render_socket_name(&name);
    if render_name.starts_with('/') {
        let _ = std::fs::remove_file(&render_name);
    }
    let render_listener = server::listen_seqpacket(&render_name).unwrap();

    let log: SharedLog = Arc::new(|msg: &str| eprintln!("[gndrm-hostd] {msg}"));
    log(&format!(
        "listening on {name} (card) + {render_name} (render), SOCK_SEQPACKET"
    ));

    // Render-node clients on their own listener: the device-side path, one
    // Device per RPC/event pair, on the same code `libgamenative_drm.so` runs.
    let render_log = Arc::clone(&log);
    let stop_fd = server::new_stop_eventfd().expect(&gndrm_obf::obf!("stop eventfd"));
    thread::Builder::new()
        .name("gndrm-hostd-render".into())
        .spawn(move || {
            server::serve_render_listener(
                render_listener,
                stop_fd,
                Arc::new(|| Device::new(PanelDesc::default(), Box::<NullPresenter>::default())),
                render_log,
            );
        })
        .expect(&gndrm_obf::obf!("render listener thread"));

    // First connection = RPC, second = events (wire contract).
    let rpc = server::accept(listener).unwrap();
    let events = server::accept(listener).unwrap();
    log("client connected (rpc + events)");

    let mut dev = Device::new(PanelDesc::default(), Box::<NullPresenter>::default());
    dev.set_logger(Box::new(|msg| eprintln!("[gndrm-hostd] {msg}")));
    let serve_log: &dyn Fn(&str) = &*log;
    server::serve(&mut dev, rpc, events, serve_log);
}
