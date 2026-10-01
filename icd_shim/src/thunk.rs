//! Wrapper-swapping thunks (aarch64 only — the shim only ever runs on
//! Android/arm64; other targets stub out so host `cargo check` works).
//!
//! Device functions the proxy does not intercept still need to reach the
//! hooks when the outer loader calls them — but the loader calls them with
//! the shim's wrapper handles. A thunk swaps the wrapper for the real
//! handle (stored right after the scratch prefix) and tail-branches to the
//! hooks' function. Generated code, one 24-byte thunk per name; the RW→RX
//! transition matches what box64's dynarec does on-device.

use crate::vk::PFN_vkVoidFunction;
use std::collections::HashMap;
use std::sync::Mutex;

#[cfg(target_arch = "aarch64")]
use crate::proxy::GN_HANDLE_SCRATCH;

pub const GN_THUNK_SIZE: usize = 24;

struct ThunkState {
    page: *mut u8,
    // Only read by the aarch64 emitter; the host stub never advances it.
    #[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
    used: usize,
    thunks: HashMap<Box<str>, PFN_vkVoidFunction>,
}

// Raw page pointer is shared only under this mutex.
unsafe impl Send for ThunkState {}

static STATE: Mutex<Option<ThunkState>> = Mutex::new(None);

const PAGE: usize = 4096;

/// compiler-rt `__clear_cache` equivalent, inlined: bionic does not export
/// that symbol (the NDK static-links it into C binaries), so declaring it
/// extern would leave an unresolvable dynamic reference and the shim's
/// dlopen would fail outright on device. Data cache clean to PoU +
/// instruction cache invalidate to PoU over [begin, end), line sizes from
/// CTR_EL0, then the architectural barriers.
#[cfg(target_arch = "aarch64")]
unsafe fn clear_cache(begin: *mut u8, end: *mut u8) {
    let ctr: u64;
    unsafe { std::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    // DminLine/IminLine are log2(line words); line bytes = 4 << field.
    let dline = 4usize << (ctr & 0xF);
    let iline = 4usize << ((ctr >> 16) & 0xF);
    let mut p = (begin as usize) & !(dline - 1);
    while p < end as usize {
        unsafe { std::arch::asm!("dc cvau, {}", in(reg) p, options(nostack)) };
        p += dline;
    }
    unsafe { std::arch::asm!("dsb ish", options(nostack)) };
    let mut p = (begin as usize) & !(iline - 1);
    while p < end as usize {
        unsafe { std::arch::asm!("ic ivau, {}", in(reg) p, options(nostack)) };
        p += iline;
    }
    unsafe { std::arch::asm!("dsb ish", options(nostack)) };
    unsafe { std::arch::asm!("isb", options(nostack)) };
}

#[cfg(target_arch = "aarch64")]
unsafe fn emit(target: PFN_vkVoidFunction, st: &mut ThunkState) -> PFN_vkVoidFunction {
    if st.page.is_null() || st.used + GN_THUNK_SIZE > PAGE {
        let page = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if page == libc::MAP_FAILED {
            st.page = std::ptr::null_mut();
            return None;
        }
        st.page = page.cast();
        st.used = 0;
    } else {
        unsafe {
            libc::mprotect(st.page.cast(), PAGE, libc::PROT_READ | libc::PROT_WRITE);
        }
    }
    let p = unsafe { st.page.add(st.used) };
    let ins = p.cast::<u32>();
    let scratch = (GN_HANDLE_SCRATCH as u32) & 0xFFF;
    unsafe {
        // ldr x0, [x0, #scratch*8]  (wrapper -> real handle)
        ins.write(0xF9400000u32 | (scratch << 10));
        // ldr x16, [pc, #12]
        ins.add(1).write(0x58000070u32);
        // br x16
        ins.add(2).write(0xD61F0200u32);
        // nop
        ins.add(3).write(0xD503201Fu32);
        p.add(16).cast::<usize>().write(target? as usize);
        clear_cache(p, p.add(GN_THUNK_SIZE));
    }
    st.used += GN_THUNK_SIZE;
    Some(unsafe { std::mem::transmute::<*mut u8, unsafe extern "C" fn()>(p) })
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn emit(_target: PFN_vkVoidFunction, _st: &mut ThunkState) -> PFN_vkVoidFunction {
    None
}

static THUNKED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// One thunk per name (the thunk's target is fixed at emission).
pub fn thunk_for(name: &str, target: PFN_vkVoidFunction) -> PFN_vkVoidFunction {
    // Every entry point the guest resolves through the shim lands here: what
    // is absent from this list either came from the shim's own table or was
    // answered by someone else (the loader's stub).
    if let Ok(mut seen) = THUNKED.lock() {
        if seen.len() < 128 && !seen.iter().any(|s| s == name) {
            crate::logf!("proxy: thunk -> {name}");
            seen.push(name.to_string());
        }
    }
    let mut guard = STATE.lock().unwrap();
    let st = guard.get_or_insert_with(|| ThunkState {
        page: std::ptr::null_mut(),
        used: 0,
        thunks: HashMap::new(),
    });
    if let Some(&fnptr) = st.thunks.get(name) {
        return fnptr;
    }
    let f = unsafe { emit(target, st) }?;
    if !st.page.is_null() {
        let ret =
            unsafe { libc::mprotect(st.page.cast(), PAGE, libc::PROT_READ | libc::PROT_EXEC) };
        if ret != 0 {
            crate::logf!("proxy: thunk page mprotect failed: {}", errno_str());
        }
    }
    let fnptr = Some(f);
    st.thunks.insert(name.into(), fnptr);
    fnptr
}

pub fn errno_str() -> String {
    let err = std::io::Error::last_os_error();
    err.to_string()
}
