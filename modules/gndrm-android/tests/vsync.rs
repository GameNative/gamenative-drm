//! Host execution of the `src/vsync.rs` unit tests.
//!
//! The rest of `gndrm-android` is `cfg(target_os = "android")`, so the crate is
//! empty on the host and cargo would never run the latch-grid tracker's tests
//! there. The tracker is deliberately Android-free (its glue is cfg-gated), so
//! this integration test compiles the very same file — one home for the
//! arithmetic, exercised by `cargo test --workspace` on the host and by
//! `--all-targets` on the Android target, where the module's own `#[cfg(test)]`
//! block runs inside the library and this harness is empty: the glue the file
//! gates on Android reaches into the crate root, which a test crate does not
//! have.

#![cfg(not(target_os = "android"))]

#[path = "../src/vsync.rs"]
mod vsync;
