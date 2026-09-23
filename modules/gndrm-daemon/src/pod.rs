//! Plain-old-data (de)serialization for the `#[repr(C)]` UAPI structs.
//!
//! The kernel ABI structs contain padding, so `bytemuck::Pod` (which forbids
//! padding) cannot be derived for them. These helpers are the single audited
//! `unsafe` site of the crate:
//!
//! - every type passed here is a fieldless-enum-free, all-integer/array
//!   `#[repr(C)]` struct from `gndrm-uapi` (all bit patterns valid),
//! - reads are `ptr::read_unaligned` from length-checked slices,
//! - writes copy `size_of::<T>()` bytes of the value,
//! - padding bytes are never *read* from untrusted input: decoded structs
//!   are used field-by-field, and encoders zero-initialise the output.

#![allow(unsafe_code)]

/// Decode one struct from the *start* of `buf`.
///
/// Returns `None` if the slice is shorter than the struct.
pub fn pod_read<T: Copy>(buf: &[u8]) -> Option<T> {
    if buf.len() < core::mem::size_of::<T>() {
        return None;
    }
    // SAFETY: length checked above; read_unaligned handles alignment; T: Copy
    // with all-valid bit patterns (UAPI integer structs). Padding bytes in the
    // result are uninitialised-from-the-caller's-buffer, which is fine because
    // we never inspect padding (field access only) and never re-encode a
    // decoded struct without going through `pod_write_zeroed` semantics.
    let v = unsafe { core::ptr::read_unaligned(buf.as_ptr().cast::<T>()) };
    Some(v)
}

/// Append the byte representation of `v` to `out`, with padding zeroed.
pub fn pod_write<T: Copy>(out: &mut Vec<u8>, v: &T) {
    let mut tmp = vec![0u8; core::mem::size_of::<T>()];
    // SAFETY: tmp is exactly size_of::<T>() bytes; T: Copy.
    unsafe {
        core::ptr::write_unaligned(tmp.as_mut_ptr().cast::<T>(), *v);
    }
    out.extend_from_slice(&tmp);
}

/// Append `slice` of structs (padding zeroed) to `out`.
pub fn pod_write_array<T: Copy>(out: &mut Vec<u8>, slice: &[T]) {
    for v in slice {
        pod_write(out, v);
    }
}

/// Raw little-endian scalar helpers for the linearized extension arrays
/// (these arrays are defined by *our* wire format, not the kernel ABI).
pub fn take_u32(buf: &[u8], off: &mut usize) -> Option<u32> {
    let b = buf.get(*off..*off + 4)?;
    *off += 4;
    Some(u32::from_le_bytes(b.try_into().ok()?))
}

pub fn take_u64(buf: &[u8], off: &mut usize) -> Option<u64> {
    let b = buf.get(*off..*off + 8)?;
    *off += 8;
    Some(u64::from_le_bytes(b.try_into().ok()?))
}
