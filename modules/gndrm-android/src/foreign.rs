//! CPU copy of a committed dma-buf the scan-out pool does not own into a
//! daemon-owned `AHardwareBuffer` the presenter can publish.
//!
//! The source is mapped once per inode and the target allocated once per
//! inode and geometry; each frame is one `DMA_BUF_IOCTL_SYNC`-bracketed row
//! copy under an `AHardwareBuffer` write lock. The copy rules themselves live
//! in [`gndrm_daemon::foreign`].

use core::ffi::c_void;
use std::os::fd::RawFd;
use std::ptr;

use gndrm_daemon::foreign::{fourcc_name, plan_copy, CopyPlan};
use gndrm_daemon::Framebuffer;
use gndrm_uapi::wire::scanout_ahb_format;

use crate::ffi;

/// `DMA_BUF_IOCTL_SYNC` (`_IOW('b', 0, struct dma_buf_sync)`).
const DMA_BUF_IOCTL_SYNC: u64 = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_START: u64 = 0;
const DMA_BUF_SYNC_END: u64 = 4;

/// The framebuffer fields a copy target is built for. A re-registration of
/// the same dma-buf with a different layout rebuilds the target.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    width: u32,
    height: u32,
    fourcc: u32,
    pitch: u32,
    offset: u32,
}

impl Layout {
    pub(crate) fn of(fb: &Framebuffer) -> Self {
        Self {
            width: fb.width,
            height: fb.height,
            fourcc: fb.pixel_format,
            pitch: fb.pitches[0],
            offset: fb.offsets[0],
        }
    }
}

impl core::fmt::Display for Layout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}x{} fourcc={} pitch={} offset={}",
            self.width,
            self.height,
            fourcc_name(self.fourcc),
            self.pitch,
            self.offset
        )
    }
}

/// A mapped foreign dma-buf and the buffer its frames are copied into.
pub(crate) struct ForeignCopy {
    plan: CopyPlan,
    src_fd: RawFd,
    map: *mut u8,
    map_len: usize,
    pub(crate) ahb: *mut ffi::AHardwareBuffer,
    /// Target row pitch in bytes.
    pub(crate) stride: u32,
    /// Target gralloc format.
    pub(crate) format: u32,
    /// Frames copied through this target.
    pub(crate) copies: u64,
}

// SAFETY: owned by the presenter state, which is reached through a mutex; the
// mapping and the buffer reference are only used by the thread holding it.
unsafe impl Send for ForeignCopy {}

impl ForeignCopy {
    /// Map `src_fd` (borrowed) and allocate a target for `fb`.
    ///
    /// # Errors
    /// Why the buffer cannot be copied, for the failure log line.
    pub(crate) fn new(fb: &Framebuffer, src_fd: RawFd) -> Result<Self, String> {
        let len = source_len(src_fd);
        if len == 0 {
            return Err(gndrm_obf::obf!("dma-buf length unknown").to_string());
        }
        let layout = gndrm_daemon::scanout_layout();
        let plan = plan_copy(fb, len, layout).map_err(|r| r.to_string())?;
        // SAFETY: fcntl on a descriptor the caller keeps open for this call.
        let fd = unsafe { libc::fcntl(src_fd, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(gndrm_obf::obf!("dup failed (errno {})", errno()).to_string());
        }
        // SAFETY: a fresh shared read-only mapping of the whole dma-buf.
        let map = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            let e = errno();
            // SAFETY: `fd` is owned here.
            unsafe { libc::close(fd) };
            return Err(gndrm_obf::obf!("mmap of {len} bytes failed (errno {e})").to_string());
        }
        let format = if plan.ten_bit {
            ffi::AHARDWAREBUFFER_FORMAT_R10G10B10A2_UNORM
        } else {
            scanout_ahb_format(layout)
        };
        let desc = ffi::AHardwareBufferDesc {
            width: fb.width,
            height: fb.height,
            layers: 1,
            format,
            usage: ffi::SCANOUT_POOL_USAGE | ffi::AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN,
            ..Default::default()
        };
        let mut ahb: *mut ffi::AHardwareBuffer = ptr::null_mut();
        // SAFETY: `desc` is a live stack value; `ahb` receives one reference.
        let rc = unsafe { ffi::AHardwareBuffer_allocate(&desc, &mut ahb) };
        if rc != 0 || ahb.is_null() {
            // SAFETY: the mapping and descriptor are owned here.
            unsafe {
                libc::munmap(map, len as usize);
                libc::close(fd);
            }
            return Err(gndrm_obf::obf!("AHardwareBuffer_allocate failed (rc={rc})").to_string());
        }
        let mut real = ffi::AHardwareBufferDesc::default();
        // SAFETY: `ahb` is a live buffer owned here.
        unsafe { ffi::AHardwareBuffer_describe(ahb, &mut real) };
        Ok(Self {
            plan,
            src_fd: fd,
            map: map.cast(),
            map_len: len as usize,
            ahb,
            stride: real.stride.saturating_mul(4),
            format,
            copies: 0,
        })
    }

    /// Copy the source's current contents into the target. The caller has
    /// waited for the frame's acquire fence and for the target's previous
    /// release fence.
    ///
    /// # Errors
    /// Why the copy did not happen, for the failure log line.
    pub(crate) fn copy(&mut self) -> Result<(), String> {
        let dst_pitch = self.stride as usize;
        if dst_pitch < self.plan.row_bytes {
            return Err(
                gndrm_obf::obf!("target stride {dst_pitch} shorter than a row").to_string(),
            );
        }
        let mut addr: *mut c_void = ptr::null_mut();
        // SAFETY: `self.ahb` is a live buffer allocated with CPU write usage.
        let rc = unsafe {
            ffi::AHardwareBuffer_lock(
                self.ahb,
                ffi::AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN,
                -1,
                ptr::null(),
                &mut addr,
            )
        };
        if rc != 0 || addr.is_null() {
            return Err(gndrm_obf::obf!("AHardwareBuffer_lock failed (rc={rc})").to_string());
        }
        sync(self.src_fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ);
        // SAFETY: the mapping covers `map_len` bytes for the life of `self`;
        // the locked target covers `dst_len` bytes (its stride times height).
        // `plan_copy` checked the source rows against `map_len`.
        unsafe {
            let src = core::slice::from_raw_parts(self.map, self.map_len);
            let dst =
                core::slice::from_raw_parts_mut(addr.cast::<u8>(), self.plan.dst_len(dst_pitch));
            self.plan.copy_rows(src, dst, dst_pitch);
        }
        sync(self.src_fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
        // SAFETY: unlocks the lock taken above; a null fence blocks until the
        // CPU writes are visible to the next reader.
        unsafe { ffi::AHardwareBuffer_unlock(self.ahb, ptr::null_mut()) };
        self.copies += 1;
        Ok(())
    }
}

impl ForeignCopy {
    /// The source's current pixels in the target's channel order, tightly
    /// packed (`(pixels, width, height)`, row pitch `width * 4`). `None` for a
    /// 10-bit source.
    pub(crate) fn stage_rows(&self) -> Option<(Vec<u8>, u32, u32)> {
        if self.plan.ten_bit {
            return None;
        }
        let pitch = self.plan.row_bytes;
        let mut v = vec![0u8; self.plan.dst_len(pitch)];
        sync(self.src_fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ);
        // SAFETY: as in `copy`; the destination is a live Vec of `dst_len`.
        unsafe {
            let src = core::slice::from_raw_parts(self.map, self.map_len);
            self.plan.copy_rows(src, &mut v, pitch);
        }
        sync(self.src_fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
        v.resize(pitch * self.plan.height as usize, 0);
        Some((v, self.plan.width, self.plan.height))
    }
}

/// A small daemon-owned buffer holding a prepared (cropped, scaled, rotated)
/// cursor image. One per prepared image: a new image gets a new buffer, so a
/// buffer SurfaceFlinger may still show is never rewritten.
pub(crate) struct CursorStage {
    pub(crate) ahb: *mut ffi::AHardwareBuffer,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

// SAFETY: as for `ForeignCopy`.
unsafe impl Send for CursorStage {}

impl CursorStage {
    /// Allocate a `width` x `height` buffer of `format` with the scan-out
    /// usage and fill it from tightly packed `pixels`.
    pub(crate) fn new(pixels: &[u8], width: u32, height: u32, format: u32) -> Result<Self, String> {
        let desc = ffi::AHardwareBufferDesc {
            width,
            height,
            layers: 1,
            format,
            usage: ffi::SCANOUT_POOL_USAGE | ffi::AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN,
            ..Default::default()
        };
        let mut ahb: *mut ffi::AHardwareBuffer = ptr::null_mut();
        // SAFETY: `desc` is a live stack value; `ahb` receives one reference.
        let rc = unsafe { ffi::AHardwareBuffer_allocate(&desc, &mut ahb) };
        if rc != 0 || ahb.is_null() {
            return Err(gndrm_obf::obf!("AHardwareBuffer_allocate failed (rc={rc})").to_string());
        }
        let stage = Self { ahb, width, height };
        let mut real = ffi::AHardwareBufferDesc::default();
        // SAFETY: `ahb` is live.
        unsafe { ffi::AHardwareBuffer_describe(ahb, &mut real) };
        let dst_pitch = real.stride as usize * 4;
        let row = width as usize * 4;
        let mut addr: *mut c_void = ptr::null_mut();
        // SAFETY: allocated with CPU write usage above.
        let rc = unsafe {
            ffi::AHardwareBuffer_lock(
                ahb,
                ffi::AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN,
                -1,
                ptr::null(),
                &mut addr,
            )
        };
        if rc != 0 || addr.is_null() {
            return Err(gndrm_obf::obf!("AHardwareBuffer_lock failed (rc={rc})").to_string());
        }
        // SAFETY: the locked buffer covers `dst_pitch * height` bytes.
        unsafe {
            let dst = core::slice::from_raw_parts_mut(
                addr.cast::<u8>(),
                dst_pitch * (height as usize - 1) + row,
            );
            for y in 0..height as usize {
                dst[y * dst_pitch..y * dst_pitch + row]
                    .copy_from_slice(&pixels[y * row..(y + 1) * row]);
            }
            ffi::AHardwareBuffer_unlock(ahb, ptr::null_mut());
        }
        Ok(stage)
    }
}

impl Drop for CursorStage {
    fn drop(&mut self) {
        // SAFETY: one reference owned by `self`; SurfaceFlinger keeps its own.
        unsafe { ffi::AHardwareBuffer_release(self.ahb) };
    }
}

impl Drop for ForeignCopy {
    fn drop(&mut self) {
        // SAFETY: the mapping, descriptor and buffer reference are owned by
        // `self` and released exactly once. SurfaceFlinger keeps its own
        // reference on a buffer it may still be showing.
        unsafe {
            libc::munmap(self.map.cast(), self.map_len);
            libc::close(self.src_fd);
            ffi::AHardwareBuffer_release(self.ahb);
        }
    }
}

/// Bracket CPU access for the exporter's cache maintenance. A descriptor that
/// is not a dma-buf (a memfd) rejects the ioctl, which is harmless.
fn sync(fd: RawFd, flags: u64) {
    let arg = flags;
    // SAFETY: `arg` is a live `struct dma_buf_sync` (one u64) for the call.
    unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &raw const arg) };
}

/// The dma-buf's length: `st_size`, or the end offset where the exporter
/// reports a zero size.
fn source_len(fd: RawFd) -> u64 {
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: fstat on a live descriptor; `st` is written on success.
    if unsafe { libc::fstat(fd, &mut st) } == 0 && st.st_size > 0 {
        return st.st_size as u64;
    }
    // SAFETY: lseek on a live descriptor; the offset is restored.
    let end = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    if end > 0 {
        // SAFETY: as above.
        unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
        end as u64
    } else {
        0
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
