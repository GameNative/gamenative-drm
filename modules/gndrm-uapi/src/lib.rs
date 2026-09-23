//! # gndrm-uapi
//!
//! Linux DRM/KMS UAPI mirror for the gamenative-drm userspace KMS emulator,
//! plus the wire protocol spoken between the `gndrm-shim` (inside the
//! gamescope process) and `gndrm-daemon` (inside the `GameNative` app).
//!
//! - [`drm`] — device-level `drm.h` subset
//! - [`mode`] — KMS `drm_mode.h` subset
//! - [`ioctl`] — ioctl request-number encoding
//! - [`wire`] — daemon RPC framing (header codec, linearization helpers)

#![forbid(unsafe_code)]
// UAPI mirrors intentionally use C-style names (DrmMode*, DRM_*).
#![allow(clippy::module_name_repetitions)]
// ioctl numbers encode `sizeof(T)` into a 14-bit field; truncation is
// impossible for every struct in this crate (all < 16 KiB), and the
// ABI ground-truth test cross-checks the results against the C headers.
#![allow(clippy::cast_possible_truncation)]

pub mod drm;
pub mod ioctl;
pub mod mode;
pub mod wire;
