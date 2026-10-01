//! Host execution of the `src/log_policy.rs` unit tests.
//!
//! The rest of `gndrm-android` is `cfg(target_os = "android")`, so the crate is
//! empty on the host and cargo would never run the policy tests there. That
//! module is deliberately Android-free, so this integration test compiles the
//! very same file — one home for the table, exercised by `cargo test
//! --workspace` on the host and by `--all-targets` on the Android target.

#[path = "../src/log_policy.rs"]
mod log_policy;
