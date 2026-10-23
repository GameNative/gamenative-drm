//! Wrapper-swapping thunks (aarch64 only — the shim only ever runs on
//! Android/arm64; other targets stub out so host `cargo check` works).
//!
//! Device functions the proxy does not intercept still need to reach the
//! hooks when the outer loader calls them — but the loader calls them with
//! the shim's wrapper handles. A thunk swaps the wrapper for the real
//! handle (stored right after the scratch prefix) and tail-branches to the
//! hooks' function. Generated code, one 24-byte thunk per target; the RW→RX
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
    cache: ThunkCache,
}

/// Thunks keyed by the address they branch to: the emitted code depends on
/// nothing else. A name is not a key, because one name resolves to different
/// targets on different devices of the same process (a promoted alias reached
/// through the loader on one device and through the driver on another).
#[derive(Default)]
struct ThunkCache {
    by_target: HashMap<usize, PFN_vkVoidFunction>,
    // Diagnostic only: the last target each name was served.
    last_target: HashMap<Box<str>, usize>,
}

impl ThunkCache {
    fn get(&self, target: usize) -> Option<PFN_vkVoidFunction> {
        self.by_target.get(&target).copied()
    }

    fn insert(&mut self, target: usize, thunk: PFN_vkVoidFunction) {
        self.by_target.insert(target, thunk);
    }

    /// Record that `name` was served `target`.
    fn note_name(&mut self, name: &str, target: usize) -> Served {
        match self.last_target.get_mut(name) {
            Some(prev) if *prev == target => Served::Same,
            Some(prev) => Served::Retarget(std::mem::replace(prev, target)),
            None => {
                self.last_target.insert(name.into(), target);
                Served::First
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Served {
    First,
    Same,
    Retarget(usize),
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

/// One thunk per target (the thunk's code depends only on the target). `name`
/// is only for the log.
pub fn thunk_for(name: &str, target: PFN_vkVoidFunction) -> PFN_vkVoidFunction {
    let addr = target? as usize;
    let mut guard = STATE.lock().unwrap();
    let st = guard.get_or_insert_with(|| ThunkState {
        page: std::ptr::null_mut(),
        used: 0,
        cache: ThunkCache::default(),
    });
    let served = st.cache.note_name(name, addr);
    let cached = st.cache.get(addr);
    let fnptr = match cached {
        Some(fnptr) => fnptr,
        None => {
            let Some(f) = (unsafe { emit(target, st) }) else {
                drop(guard);
                log_retarget(name, served, addr);
                return None;
            };
            if !st.page.is_null() {
                let ret = unsafe {
                    libc::mprotect(st.page.cast(), PAGE, libc::PROT_READ | libc::PROT_EXEC)
                };
                if ret != 0 {
                    crate::logf!(gndrm_obf::obf!(
                        "proxy: thunk page mprotect failed: {}",
                        errno_str()
                    ));
                }
            }
            let fnptr = Some(f);
            st.cache.insert(addr, fnptr);
            fnptr
        }
    };
    drop(guard);
    log_retarget(name, served, addr);
    if cached.is_none() || served == Served::First {
        crate::logf!(gndrm_obf::obf!(
            "proxy: thunk -> {name} = {addr:#x} {}",
            describe(addr)
        ));
    }
    fnptr
}

fn log_retarget(name: &str, served: Served, addr: usize) {
    if let Served::Retarget(old) = served {
        crate::logf!(gndrm_obf::obf!(
            "proxy: thunk retarget {name}: {old:#x} -> {addr:#x}"
        ));
    }
}

/// Where `addr` lives, by `dladdr`: `symbol (library)`, `library+offset`, or
/// `anon` for code no loaded object claims.
fn describe(addr: usize) -> String {
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    let found = unsafe { libc::dladdr(addr as *const libc::c_void, &raw mut info) };
    if found == 0 || info.dli_fname.is_null() {
        return gndrm_obf::obf!("anon").to_string();
    }
    let lib = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy();
    if info.dli_sname.is_null() {
        let off = addr.wrapping_sub(info.dli_fbase as usize);
        return format!("{}", gndrm_obf::obf!("{lib}+{off:#x}"));
    }
    let sym = unsafe { std::ffi::CStr::from_ptr(info.dli_sname) }.to_string_lossy();
    format!("{}", gndrm_obf::obf!("{sym} ({lib})"))
}

pub fn errno_str() -> String {
    let err = std::io::Error::last_os_error();
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn fake_a() {}
    unsafe extern "C" fn fake_b() {}

    #[test]
    fn one_thunk_per_target_not_per_name() {
        let mut c = ThunkCache::default();
        let a: PFN_vkVoidFunction = Some(fake_a);
        let b: PFN_vkVoidFunction = Some(fake_b);
        c.insert(0x1000, a);
        assert_eq!(c.get(0x1000), Some(a));
        assert_eq!(c.get(0x2000), None);
        c.insert(0x2000, b);
        assert_eq!(c.get(0x1000), Some(a));
        assert_eq!(c.get(0x2000), Some(b));
    }

    #[test]
    fn retarget_is_reported_once_per_change() {
        let mut c = ThunkCache::default();
        assert_eq!(c.note_name("vkResetQueryPoolEXT", 0x1000), Served::First);
        assert_eq!(c.note_name("vkResetQueryPoolEXT", 0x1000), Served::Same);
        assert_eq!(
            c.note_name("vkResetQueryPoolEXT", 0x2000),
            Served::Retarget(0x1000)
        );
        assert_eq!(c.note_name("vkResetQueryPoolEXT", 0x2000), Served::Same);
        assert_eq!(c.note_name("vkResetQueryPool", 0x1000), Served::First);
    }
}
