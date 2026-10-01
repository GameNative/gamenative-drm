//! Host execution of the `src/dump.rs` unit tests.
//!
//! The rest of `gndrm-android` is `cfg(target_os = "android")`, so the crate is
//! empty on the host and cargo would never run the panel-dump conversion tests
//! there. That module is deliberately Android-free, so this integration test
//! compiles the very same file — one home for the conversion, exercised by
//! `cargo test --workspace` on the host and by `--all-targets` on the Android
//! target.

#[path = "../src/dump.rs"]
mod dump;
