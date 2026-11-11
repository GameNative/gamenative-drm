//! The engine's host side: the shader cache it loads through, in Rust.
//!
//! `liblsfg_engine.so` needs exactly two things from its embedder — load the
//! module set for a cache path, and free it again — because everything else about
//! the DLL (PE parsing, variant selection, SPIR-V renumbering, the cache format)
//! lives here rather than in `lsfg_dll.c`.
//!
//! The functions are handed over as a [`HostOps`] table, not exported as symbols:
//! the engine is a separate library, so a table is the only way to reach this
//! code without the ICD having to export anything. That is also why nothing in
//! this crate is `#[no_mangle]` — there is no link-time edge in either direction.
//!
//! The layouts are `lsfg_dll.h`'s: `LsfgModule` is
//! `{ u32 id; u32* words; u32 word_count; }` and the set is a fixed 25-entry
//! array. Module words are `malloc`ed here and freed by [`release_modules`], so
//! the C++ side may hold them for as long as it keeps the set.

use std::ffi::{c_char, CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Diagnostics, on the same gate and with the same prefix the ICD uses.
/// Resolved once: this is called at cache build and load, never per frame.
macro_rules! log {
    ($e:expr) => {
        if $crate::logging() {
            eprintln!("{} {}", "gn-vkshim:", $e);
        }
    };
}

fn logging() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    // 0 = unresolved, 1 = off, 2 = on.
    static GATE: AtomicU8 = AtomicU8::new(0);
    match GATE.load(Ordering::Relaxed) {
        2 => true,
        1 => false,
        _ => {
            let on = std::env::var_os("GNDRM_LOG_VKSHIM")
                .is_some_and(|v| !v.is_empty() && v != OsStr::new("0"));
            GATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

pub mod cache;
pub mod pacing;
pub mod pe;

/// `LSFG_SHADER_COUNT` in the header.
pub const SHADER_COUNT: usize = 25;
const _: () = assert!(cache::SHADER_IDS.len() == SHADER_COUNT);

/// `LsfgStatus`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum LsfgStatus {
    Ok = 0,
    NotInstalled = 1,
    UnreadableFile = 2,
    NotPortableExecutable = 3,
    MissingShaders = 4,
    TranslationFailed = 5,
    CacheUnusable = 6,
}

/// `LsfgModule`.
#[repr(C)]
pub struct LsfgModule {
    pub id: u32,
    pub words: *mut u32,
    pub word_count: u32,
}

/// `LsfgModuleSet`.
#[repr(C)]
pub struct LsfgModuleSet {
    pub modules: [LsfgModule; SHADER_COUNT],
    pub count: u32,
    pub variant: cache::Variant,
}

/// The table `liblsfg_engine.so` installs with `lsfg_engine_set_host`.
#[repr(C)]
pub struct HostOps {
    pub load_modules: unsafe extern "C" fn(*const c_char, *mut LsfgModuleSet) -> LsfgStatus,
    pub release_modules: unsafe extern "C" fn(*mut LsfgModuleSet),
}

/// Installed once, before the engine's first create.
pub static HOST_OPS: HostOps = HostOps {
    load_modules,
    release_modules,
};

unsafe fn cpath(ptr: *const c_char) -> Option<PathBuf> {
    if ptr.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
    Some(PathBuf::from(OsStr::from_bytes(bytes)))
}

fn malloc_words(words: &[u32]) -> Option<*mut u32> {
    let bytes = words.len().checked_mul(std::mem::size_of::<u32>())?;
    let ptr = unsafe { libc::malloc(bytes) }.cast::<u32>();
    if ptr.is_null() {
        return None;
    }
    unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), ptr, words.len()) };
    Some(ptr)
}

/// Fill `out_set` with the cache's modules. The engine owns the set and the
/// words until it calls [`release_modules`]; a failure leaves the set empty.
///
/// # Safety
/// `out_set` must be null or point to a writable `LsfgModuleSet`.
pub unsafe extern "C" fn load_modules(
    cache_path: *const c_char,
    out_set: *mut LsfgModuleSet,
) -> LsfgStatus {
    if out_set.is_null() {
        return LsfgStatus::CacheUnusable;
    }
    unsafe { std::ptr::write_bytes(out_set, 0, 1) };
    let Some(path) = (unsafe { cpath(cache_path) }) else {
        return LsfgStatus::CacheUnusable;
    };

    let (variant, modules) = match cache::load(&path) {
        Ok(loaded) => loaded,
        Err(status) => return status,
    };
    let set = unsafe { &mut *out_set };
    set.variant = variant;
    for module in modules {
        let Some(words) = malloc_words(&module.words) else {
            unsafe { release_modules(out_set) };
            return LsfgStatus::CacheUnusable;
        };
        let Ok(word_count) = u32::try_from(module.words.len()) else {
            unsafe { libc::free(words.cast()) };
            unsafe { release_modules(out_set) };
            return LsfgStatus::CacheUnusable;
        };
        set.modules[set.count as usize] = LsfgModule {
            id: module.id,
            words,
            word_count,
        };
        set.count += 1;
    }
    LsfgStatus::Ok
}

/// Free the words a loaded set owns and empty it.
///
/// # Safety
/// `set` must be null or the set handed to [`load_modules`].
pub unsafe extern "C" fn release_modules(set: *mut LsfgModuleSet) {
    if set.is_null() {
        return;
    }
    let set = unsafe { &mut *set };
    for index in 0..set.count as usize {
        let words = set.modules[index].words;
        if !words.is_null() {
            unsafe { libc::free(words.cast()) };
        }
        set.modules[index].words = std::ptr::null_mut();
    }
    set.count = 0;
    set.variant = cache::Variant::None;
}

/// Rebuild `cache_path` only when it does not already match `dll_path`.
///
/// The DLL is the input, the cache is derived, and the consumer validates before
/// it trusts: a cache from an older DLL or a different fp16 choice is rebuilt,
/// one that matches is left alone.
pub fn ensure_cache(dll: &Path, cache: &Path, prefer_fp16: bool) -> LsfgStatus {
    match cache::matches_source(cache, dll) {
        Ok(true) => LsfgStatus::Ok,
        Ok(false) => cache::build(dll, cache, prefer_fp16),
        Err(LsfgStatus::NotInstalled) if !dll.exists() => LsfgStatus::NotInstalled,
        Err(_) => cache::build(dll, cache, prefer_fp16),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_set() -> LsfgModuleSet {
        LsfgModuleSet {
            modules: std::array::from_fn(|_| LsfgModule {
                id: 0,
                words: std::ptr::null_mut(),
                word_count: 0,
            }),
            count: 0,
            variant: cache::Variant::None,
        }
    }

    #[test]
    fn the_abi_layout_matches_the_header() {
        // u32 id, 8-byte pointer, u32 count: 24 bytes with the pointer's alignment.
        assert_eq!(std::mem::size_of::<LsfgModule>(), 24);
        assert_eq!(std::mem::align_of::<LsfgModule>(), 8);
        assert_eq!(std::mem::size_of::<LsfgModuleSet>(), 25 * 24 + 8);
    }

    #[test]
    fn the_host_table_carries_both_entries() {
        let ops = &HOST_OPS;
        assert_eq!(
            ops.load_modules as usize,
            load_modules as unsafe extern "C" fn(*const c_char, *mut LsfgModuleSet) -> LsfgStatus
                as usize
        );
        assert_eq!(
            ops.release_modules as usize,
            release_modules as unsafe extern "C" fn(*mut LsfgModuleSet) as usize
        );
    }

    #[test]
    fn releasing_an_empty_set_is_a_no_op() {
        let mut set = empty_set();
        set.variant = cache::Variant::Fp16;
        unsafe { release_modules(&raw mut set) };
        assert_eq!(set.count, 0);
        assert!(matches!(set.variant, cache::Variant::None));
    }

    #[test]
    fn a_missing_cache_is_reported_as_not_installed() {
        let mut set = empty_set();
        let missing = b"/nonexistent/lsfg.cache\0";
        let status = unsafe { load_modules(missing.as_ptr().cast(), &raw mut set) };
        assert_eq!(status, LsfgStatus::NotInstalled);
        assert_eq!(set.count, 0);
    }
}
