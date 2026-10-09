//! Compile-time string obfuscation for the shipped libraries.
//!
//! Message and diagnostic string literals are stored XOR-encoded in `.rodata`
//! and decoded into a stack buffer at the use site, so `strings` over a shipped
//! artifact no longer recovers log/error text. Encoding happens at macro
//! expansion (see `gndrm-obf-macros`); decoding is a per-byte XOR into a fixed
//! local array — no allocation, no heap, no global state.
//!
//! Functionally load-bearing strings (env var names, Vulkan layer/extension
//! names, socket/protocol names, device paths compared at runtime) are left
//! readable by design.

use std::ffi::CStr;
use std::fmt;
use std::ops::Deref;

// The `obf!`/`obfc!` macros expand to `::gndrm_obf::…` paths; this makes that
// path resolve inside this crate too (its own unit tests).
extern crate self as gndrm_obf;

pub use gndrm_obf_macros::obf;
pub use gndrm_obf_macros::obfc;

/// One byte of the position-varying XOR key stream.
#[inline]
#[must_use]
pub const fn key_byte(key: u8, i: usize) -> u8 {
    key.wrapping_add(i as u8)
}

/// A string literal decoded into a stack-resident byte array.
///
/// The encoded bytes come from [`obf`]; [`Obf::new`] applies the XOR key at
/// runtime. The buffer is valid UTF-8 by construction (the encoder only ever
/// XORs the original literal's bytes).
pub struct Obf<const N: usize>([u8; N]);

impl<const N: usize> Obf<N> {
    /// Decode `enc` in place under `key`.
    ///
    /// Not `const`, and the key is passed through [`std::hint::black_box`], so
    /// the optimizer cannot constant-fold the XOR back into a decoded `.rodata`
    /// string — which would defeat the obfuscation entirely.
    #[inline]
    #[must_use]
    pub fn new(enc: [u8; N], key: u8) -> Self {
        let key = std::hint::black_box(key);
        let mut b = enc;
        let mut i = 0;
        while i < N {
            b[i] ^= key_byte(key, i);
            i += 1;
        }
        Obf(b)
    }
}

impl<const N: usize> Deref for Obf<N> {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        // The encoder XORs the original literal's bytes, which are valid
        // UTF-8; the decode is the exact inverse, so the result is valid.
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }
}

impl<const N: usize> fmt::Display for Obf<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self)
    }
}

impl<const N: usize> AsRef<str> for Obf<N> {
    fn as_ref(&self) -> &str {
        self
    }
}

/// A NUL-terminated literal decoded into a stack-resident byte array.
///
/// Same encoding as [`Obf`], but the encoded buffer carries the terminating
/// NUL so the decoded value is a valid [`CStr`] — used for logcat tags handed
/// to `__android_log_write`.
pub struct ObfC<const N: usize>([u8; N]);

impl<const N: usize> ObfC<N> {
    /// Decode `enc` (a literal plus its NUL) under `key`. See [`Obf::new`] for
    /// why this is not `const` and black-boxes the key.
    #[inline]
    #[must_use]
    pub fn new(enc: [u8; N], key: u8) -> Self {
        let key = std::hint::black_box(key);
        let mut b = enc;
        let mut i = 0;
        while i < N {
            b[i] ^= key_byte(key, i);
            i += 1;
        }
        ObfC(b)
    }

    /// The decoded value as a C string.
    ///
    /// # Panics
    /// Only if `N == 0` (the encoder always appends the NUL).
    #[inline]
    #[must_use]
    pub fn as_cstr(&self) -> &CStr {
        CStr::from_bytes_with_nul(&self.0).expect("obfc encoding includes the NUL")
    }
}

/// A [`fmt::Display`] adaptor over a closure that writes with `write!`.
///
/// `obf!` expands to this so the decoded pieces (whose buffers must not
/// outlive the formatting statement) are written through the formatter the
/// caller already holds, never stored.
pub struct FmtWith<F>(F);

impl<F: Fn(&mut fmt::Formatter<'_>) -> fmt::Result> fmt::Display for FmtWith<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (self.0)(f)
    }
}

/// Wrap a formatting closure as a [`fmt::Display`] value.
#[inline]
#[must_use]
pub fn fmt_with<F: Fn(&mut fmt::Formatter<'_>) -> fmt::Result>(f: F) -> FmtWith<F> {
    FmtWith(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obf_roundtrips() {
        let s = obf!("scan-out acceptor poll: {}", 7u32);
        assert_eq!(s.to_string(), "scan-out acceptor poll: 7");
    }

    #[test]
    fn obfc_matches_plain() {
        let t = obfc!("gndrm-daemon");
        assert_eq!(t.as_cstr().to_str().unwrap(), "gndrm-daemon");
    }

    #[test]
    fn obf_escapes() {
        assert_eq!(
            obf!("a\tb\n\"q\"\u{1f600}").to_string(),
            "a\tb\n\"q\"\u{1f600}"
        );
    }
}
