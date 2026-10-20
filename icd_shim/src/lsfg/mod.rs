//! Bridge to `liblsfg_engine.so`.
//!
//! The engine is its own shared library (built by `tools/build-lsfg.sh`) rather
//! than objects inside this ICD, for two reasons: the ICD's dynamic table is
//! pinned to the three Khronos loader entry points, and libc++ links cleanly
//! inside a leaf library instead of colliding with rustc's linker stubs here.
//!
//! The dependency runs both ways and is therefore explicit:
//!
//!   shim ──dlopen/dlsym──▶ `vkr_lsfg_*`   (frame generation)
//!   shim ──host table────▶ `lsfg_load_modules`/`release_modules`
//!
//! The second edge is why the engine never needs a symbol from this library:
//! it calls back through the table installed by [`open`], so nothing has to be
//! exported from the ICD.
//!
//! A missing, unloadable or ABI-mismatched library is not an error the session
//! dies of — [`open`] returns `None` and frame generation stays off.

use std::ffi::{c_char, c_void, CString};

use crate::vk::{VkCommandBuffer, VkDevice, VkFormat, VkImage, VkImageView, VkPhysicalDevice};

/// ABI revision this shim speaks; `lsfg_engine_abi` must return it.
const EXPECTED_ABI: u32 = 1;

/// Environment override for the library path.
const PATH_ENV: &str = "GNDRM_LSFG_ENGINE";
/// Default: resolved by the dynamic loader, which already searches the payload's
/// library directory.
const DEFAULT_LIBRARY: &str = "liblsfg_engine.so";

type CreateFn = unsafe extern "C" fn(VkDevice, VkPhysicalDevice, *const c_char) -> *mut c_void;
type DestroyFn = unsafe extern "C" fn(*mut c_void);
type ConfigureFn = unsafe extern "C" fn(*mut c_void, u32, u32, f32, f32);
type RefreshFn = unsafe extern "C" fn(*mut c_void, f32);
type ExtentFn = unsafe extern "C" fn(*mut c_void, u32, u32);
type RebuildFn = unsafe extern "C" fn(*const c_void, u32, u32, VkFormat) -> bool;
type PrepareFn = unsafe extern "C" fn(*mut c_void, u32, u32, VkFormat) -> bool;
type PlanFn = unsafe extern "C" fn(*mut c_void, u32, u64) -> u32;
type ProcessFn = unsafe extern "C" fn(*mut c_void, VkCommandBuffer, VkImage, u32, u32, u32);
type GenerateFn =
    unsafe extern "C" fn(*mut c_void, VkCommandBuffer, u32, u32, VkImage, VkImageView, u32, u32);
type ForgetFn = unsafe extern "C" fn(*mut c_void);
type ResetFn = unsafe extern "C" fn(*mut c_void);
type AbiFn = unsafe extern "C" fn() -> u32;
type SetHostFn = unsafe extern "C" fn(*const gndrm_lsfg::HostOps);

/// The engine's API, resolved once per process.
pub struct Api {
    pub create: CreateFn,
    pub destroy: DestroyFn,
    pub configure: ConfigureFn,
    pub set_refresh_rate: RefreshFn,
    pub set_guest_extent: ExtentFn,
    pub needs_rebuild: RebuildFn,
    pub prepare: PrepareFn,
    pub plan: PlanFn,
    pub process: ProcessFn,
    pub generate_into: GenerateFn,
    pub forget_targets: ForgetFn,
    pub reset: ResetFn,
}

impl Api {
    /// Resolve every entry point, refusing a partial set.
    unsafe fn resolve(handle: *mut c_void) -> Option<Self> {
        unsafe fn symbol(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
            let name = CString::new(name).ok()?;
            let ptr = libc::dlsym(handle, name.as_ptr());
            (!ptr.is_null()).then_some(ptr)
        }
        // One macro keeps the list and the struct in step: a missing symbol is a
        // build/ABI mismatch, never a partially usable engine.
        macro_rules! take {
            ($name:literal, $ty:ty) => {{
                let ptr = unsafe { symbol(handle, $name) }?;
                // SAFETY: the symbol is the same C function the engine exports.
                unsafe { std::mem::transmute::<*mut c_void, $ty>(ptr) }
            }};
        }
        Some(Self {
            create: take!("vkr_lsfg_create", CreateFn),
            destroy: take!("vkr_lsfg_destroy", DestroyFn),
            configure: take!("vkr_lsfg_configure", ConfigureFn),
            set_refresh_rate: take!("vkr_lsfg_set_refresh_rate", RefreshFn),
            set_guest_extent: take!("vkr_lsfg_set_guest_extent", ExtentFn),
            needs_rebuild: take!("vkr_lsfg_needs_rebuild", RebuildFn),
            prepare: take!("vkr_lsfg_prepare", PrepareFn),
            plan: take!("vkr_lsfg_plan", PlanFn),
            process: take!("vkr_lsfg_process", ProcessFn),
            generate_into: take!("vkr_lsfg_generate_into", GenerateFn),
            forget_targets: take!("vkr_lsfg_forget_targets", ForgetFn),
            reset: take!("vkr_lsfg_reset", ResetFn),
        })
    }
}

/// An open engine library. Dropping it does not `dlclose`: the engine may outlive
/// a session in the guest process, and a second `dlopen` of the same path returns
/// the same handle anyway.
pub struct Engine {
    pub api: Api,
    library: String,
}

impl Engine {
    /// The path this engine was loaded from, for logs.
    #[must_use]
    pub fn library(&self) -> &str {
        &self.library
    }
}

/// `dlopen` the engine, check its ABI, and install this shim as its host.
///
/// Returns `None` on any failure, including a symbol set that does not match —
/// frame generation is optional and must never take the session with it.
#[must_use]
pub fn open() -> Option<Engine> {
    let library = std::env::var(PATH_ENV).unwrap_or_else(|_| DEFAULT_LIBRARY.to_owned());
    let Ok(name) = CString::new(library.clone()) else {
        return None;
    };
    // RTLD_NOW: a missing symbol is reported here rather than at the first
    // generated frame.
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        crate::logf!(gndrm_obf::obf!("lsfg: cannot load {}", library));
        return None;
    }

    let abi = unsafe { resolve_one::<AbiFn>(handle, "lsfg_engine_abi") };
    let set_host = unsafe { resolve_one::<SetHostFn>(handle, "lsfg_engine_set_host") };
    let (Some(abi), Some(set_host)) = (abi, set_host) else {
        crate::logf!(gndrm_obf::obf!("lsfg: {} is not an engine", library));
        return None;
    };
    let revision = unsafe { abi() };
    if revision != EXPECTED_ABI {
        crate::logf!(gndrm_obf::obf!(
            "lsfg: {} speaks ABI {}, this build speaks {}",
            library,
            revision,
            EXPECTED_ABI
        ));
        return None;
    }

    // Before any create: the engine loads its shader modules through here.
    unsafe { set_host(&gndrm_lsfg::HOST_OPS) };
    let api = unsafe { Api::resolve(handle) }?;
    crate::logf!(gndrm_obf::obf!("lsfg: engine ready ({})", library));
    Some(Engine { api, library })
}

unsafe fn resolve_one<T>(handle: *mut c_void, name: &str) -> Option<T> {
    let name = CString::new(name).ok()?;
    let ptr = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller asserts the symbol has this signature.
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&ptr) })
}
