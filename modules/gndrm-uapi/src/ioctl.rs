//! ioctl request-number computation, mirroring `<asm-generic/ioctl.h>`.
//!
//! Linux encodes an ioctl request as
//! `(dir << 30) | (size << 16) | (type << 8) | nr` on all `asm-generic`
//! architectures (`x86_64`, `aarch64`, ...). The kernel's DRM dispatch switches on
//! the full encoded number, so reproducing these values exactly is what makes
//! the shim indistinguishable from a real device node.

/// `asm-generic` direction bits (`_IOC_NONE` is unused by DRM).
pub const IOC_WRITE: u32 = 1;
pub const IOC_READ: u32 = 2;

const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + 8;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + 8;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + 14;

#[must_use]
pub const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    (dir << IOC_DIRSHIFT) | (ty << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT)
}

#[must_use]
pub const fn iow<T>(ty: u32, nr: u32) -> u32 {
    ioc(IOC_WRITE, ty, nr, core::mem::size_of::<T>() as u32)
}

#[must_use]
pub const fn iowr<T>(ty: u32, nr: u32) -> u32 {
    ioc(
        IOC_READ | IOC_WRITE,
        ty,
        nr,
        core::mem::size_of::<T>() as u32,
    )
}

/// Extract the payload size encoded in a request number (`_IOC_SIZE`).
#[must_use]
pub const fn ioc_size(request: u32) -> u32 {
    (request >> IOC_SIZESHIFT) & 0x3FFF
}

/// Extract the command number (`_IOC_NR`).
#[must_use]
pub const fn ioc_nr(request: u32) -> u32 {
    (request >> IOC_NRSHIFT) & 0xFF
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ground truth from a real kernel header decode (x86_64):
    // DRM_IOCTL_VERSION = _IOWR('d', 0x00, struct drm_version[64])
    #[test]
    fn known_values() {
        assert_eq!(iow::<u64>(u32::from(b'd'), 0x00), 0x4008_6400);
        assert_eq!(ioc_size(0xC028_64A7), 0x28); // GETCONNECTOR size field
        assert_eq!(ioc_nr(0xC028_64A7), 0xA7);
    }
}
