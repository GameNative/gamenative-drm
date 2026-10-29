//! Host execution of the `src/layout.rs` unit tests.
//!
//! The rest of `gndrm-android` is `cfg(target_os = "android")`, so the crate is
//! empty on the host and cargo would never run the layout rule's tests there.
//! The rule and the aconfig decoder are Android-free (the property and file
//! reads are cfg-gated), so this integration test compiles the very same file.

#![cfg(not(target_os = "android"))]

#[path = "../src/layout.rs"]
#[allow(dead_code)]
mod layout;
