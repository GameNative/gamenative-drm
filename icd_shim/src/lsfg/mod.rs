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

use std::ffi::{c_char, c_void, CString, OsStr};
use std::os::unix::ffi::OsStrExt;

use crate::vk::{VkCommandBuffer, VkDevice, VkFormat, VkImage, VkImageView, VkPhysicalDevice};

/// ABI revision this shim speaks; `lsfg_engine_abi` must return it.
const EXPECTED_ABI: u32 = 1;

/// Environment override for the library path.
const PATH_ENV: &str = "GNDRM_LSFG_ENGINE";
const LIBRARY_NAME: &str = "liblsfg_engine.so";

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
type VkdLoadFn =
    unsafe extern "C" fn(crate::vk::VkInstance, VkDevice, Option<crate::vk::GipaFn>) -> bool;
type TranslateFn = gndrm_lsfg::cache::TranslateFn;

/// The engine's API, resolved once per process.
pub struct Api {
    /// Fills the engine's `vk*` dispatch from the driver's own table. Called once
    /// per device, before the first create.
    pub vkd_load: VkdLoadFn,
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
            vkd_load: take!("vkd_load", VkdLoadFn),
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
    let library = std::env::var(PATH_ENV).unwrap_or_else(|_| library_beside_icd());
    let Ok(name) = CString::new(library.clone()) else {
        return None;
    };
    // RTLD_NOW: a missing symbol is reported here rather than at the first
    // generated frame.
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        crate::logf!(gndrm_obf::obf!(
            "lsfg: cannot load {}: {}",
            library,
            dlerror_text()
        ));
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

    // The DXBC translator lives in the engine too; the cache builder needs it
    // only for a DLL that carries DXBC instead of pre-built SPIR-V.
    if let Some(translate) = unsafe { resolve_one::<TranslateFn>(handle, "lsfg_translate_dxbc") } {
        gndrm_lsfg::cache::set_translator(translate);
    }
    // Before any create: the engine loads its shader modules through here.
    unsafe { set_host(&gndrm_lsfg::HOST_OPS) };
    let api = unsafe { Api::resolve(handle) }?;
    crate::logf!(gndrm_obf::obf!("lsfg: engine ready ({})", library));
    Some(Engine { api, library })
}

/// `dlerror()`, cleared first so a stale message is never reported.
fn dlerror_text() -> String {
    // One call: `dlerror` returns the pending message and clears it, so a
    // "clear first, then read" pair reads nothing.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        return "no message".to_owned();
    }
    unsafe { std::ffi::CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned()
}

/// `liblsfg_engine.so` next to this ICD, by its own loaded path.
///
/// A bare name would depend on `LD_LIBRARY_PATH`, and the payload's library
/// directory is only on gamescope's path — not on the wine guest's, which is the
/// process this shim usually serves. The loader did open *this* library from a
/// known path, so ask for that path rather than guess.
fn library_beside_icd() -> String {
    let symbol = crate::vk_icdNegotiateLoaderICDInterfaceVersion as *const c_void;
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    let found = unsafe { libc::dladdr(symbol, &raw mut info) };
    if found == 0 || info.dli_fname.is_null() {
        return LIBRARY_NAME.to_owned();
    }
    let path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_bytes();
    let path = std::path::Path::new(OsStr::from_bytes(path));
    match path.parent() {
        Some(dir) => dir.join(LIBRARY_NAME).to_string_lossy().into_owned(),
        None => LIBRARY_NAME.to_owned(),
    }
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

/// Environment names this backend reads. Container env, like `GNDRM_HDR`: they
/// reach the guest process the ICD runs in, not the app.
const MULTIPLIER_ENV: &str = "GNDRM_LSFG";
const DLL_ENV: &str = "GNDRM_LSFG_DLL";
const CACHE_ENV: &str = "GNDRM_LSFG_CACHE";
const FLOW_ENV: &str = "GNDRM_LSFG_FLOW";
/// The user's source cap, carried in the environment for the launch path: at
/// start the app has not pushed anything through the daemon yet, and without this
/// the session would run unpaced until the first Quick Menu change.
const CAP_ENV: &str = "GNDRM_LSFG_FPS_LIMIT";

/// The settings in force, kept live.
///
/// The app can change the multiplier or the source cap mid-session; the daemon
/// pushes the new values on the scan-out channel and the shim's release path
/// picks them up ([`crate::scanout::poll_notes`]), so the presenter follows within
/// a frame instead of on the next launch. Atomics because the present path reads
/// them per commit and must not take a lock.
struct Live {
    enabled: std::sync::atomic::AtomicU32,
    multiplier: std::sync::atomic::AtomicU32,
    flow_per_mille: std::sync::atomic::AtomicU32,
    fps_limit: std::sync::atomic::AtomicU32,
}

static LIVE: Live = Live {
    enabled: std::sync::atomic::AtomicU32::new(0),
    multiplier: std::sync::atomic::AtomicU32::new(0),
    flow_per_mille: std::sync::atomic::AtomicU32::new(0),
    fps_limit: std::sync::atomic::AtomicU32::new(0),
};

/// Adopt a settings frame. `enabled = 0` is the user turning generation off and
/// is honoured as such — it does not fall back to the launch-time environment.
pub fn apply(config: gndrm_uapi::wire::LsfgConfig) {
    use std::sync::atomic::Ordering::Relaxed;
    let previous = live();
    LIVE.enabled.store(config.enabled, Relaxed);
    LIVE.multiplier.store(config.multiplier, Relaxed);
    LIVE.flow_per_mille.store(config.flow_per_mille, Relaxed);
    LIVE.fps_limit.store(config.fps_limit, Relaxed);
    let now = (config.enabled != 0, config.multiplier, config.fps_limit);
    if previous != now {
        crate::logf!(gndrm_obf::obf!(
            "lsfg: settings enabled={} multiplier={} source_cap={}",
            now.0,
            now.1,
            now.2
        ));
    }
}

/// `(enabled, multiplier, source cap)` as last adopted; all zeros until the first
/// settings frame arrives.
#[must_use]
pub fn live() -> (bool, u32, u32) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        LIVE.enabled.load(Relaxed) != 0,
        LIVE.multiplier.load(Relaxed),
        LIVE.fps_limit.load(Relaxed),
    )
}

/// The rate the guest should be paced to while frame generation runs.
///
/// `None` when generation is off or the user set no cap, in which case the
/// daemon's own frame cap (if any) still applies. This is the first consumer of
/// [`live`]: with the app's limiter disarmed for gamescope, the cap on the source
/// comes from here, not from a `FrameCapNote`.
#[must_use]
pub fn source_cap() -> Option<u32> {
    let (enabled, _, cap) = live();
    (enabled && cap > 0).then_some(cap)
}

/// `(source, output)` rates in whole hertz while generation is on.
///
/// The user's cap is the **source** rate: the guest renders that many frames, and
/// the presenter presents `cap × multiplier` by inserting generated frames
/// between them. `(0, 0)` when generation is off or uncapped.
#[must_use]
pub fn grid() -> (u32, u32) {
    let (enabled, multiplier, cap) = live();
    if !enabled || cap == 0 {
        return (0, 0);
    }
    (cap, cap.saturating_mul(multiplier.max(1)))
}

/// One device's frame-generation state.
///
/// Created with the device and destroyed with it. Nothing is generated until the
/// swapchain and present paths learn about it; at this stage the engine is
/// initialised, its shaders are loaded, and the session reports what it found.
pub struct Session {
    engine: Engine,
    /// `VkrLsfg*` — owned by this session, freed in `Drop`.
    root: *mut c_void,
    multiplier: u32,
}

// The engine handle is only touched under the shim's own serialisation (device
// create/destroy, swapchain create, present worker), which is what the `unsafe`
// blocks at each call site assert.
unsafe impl Send for Session {}

impl Session {
    /// Open the engine for a device, load the shader cache, and create the
    /// engine context.
    ///
    /// `fp16` must reflect what the **guest's device** actually enabled: a module
    /// using float16 arithmetic fails pipeline creation otherwise, so the fp32
    /// variant is the safe default and fp16 is used only when the guest asked for
    /// it.
    ///
    /// # Safety
    /// `instance`, `device` and `physical` must be the real driver handles
    /// belonging to the device being created, and `gipa` the driver's own
    /// `vkGetInstanceProcAddr`.
    #[must_use]
    pub unsafe fn open(
        instance: crate::vk::VkInstance,
        physical: VkPhysicalDevice,
        device: VkDevice,
        gipa: Option<crate::vk::GipaFn>,
        fp16: bool,
    ) -> Option<Self> {
        // The app's settings arrive through the daemon, over the same channel
        // scan-out allocation uses: `GameNativeDrm.nativeSetLsfgConfig` stores
        // them in this process's daemon, and the shim asks for them here.
        let from_daemon = crate::scanout::lsfg_config();
        // A configured session answers for both directions: `enabled = 0` is the
        // user turning it off, and must not fall through to the environment, which
        // was captured at launch.
        if let Some(config) = from_daemon {
            if config.enabled == 0 || config.multiplier < 2 {
                crate::logf!(gndrm_obf::obf!("lsfg: disabled by the app"));
                return None;
            }
        }
        let multiplier = from_daemon
            .map(|s| s.multiplier)
            .or_else(|| std::env::var(MULTIPLIER_ENV).ok()?.parse().ok())?;
        if multiplier < 2 {
            return None;
        }
        let dll = std::env::var(DLL_ENV).ok().map(std::path::PathBuf::from)?;
        if !dll.is_file() {
            crate::logf!(gndrm_obf::obf!(
                "lsfg: no Lossless.dll at {}; frame generation off",
                dll.display()
            ));
            return None;
        }

        let engine = open()?;
        if !unsafe { (engine.api.vkd_load)(instance, device, gipa) } {
            crate::logf!(gndrm_obf::obf!(
                "lsfg: dispatch load failed; frame generation off"
            ));
            return None;
        }

        // Cache beside the DLL unless the caller named one, and keyed by the
        // variant so a device that cannot run fp16 never invalidates the fp32
        // build (and vice versa).
        let cache = std::env::var(CACHE_ENV)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let name = if fp16 {
                    "lossless_fp16.lsfgc"
                } else {
                    "lossless_fp32.lsfgc"
                };
                dll.with_file_name(name)
            });
        let status = gndrm_lsfg::ensure_cache(&dll, &cache, fp16);
        if status != gndrm_lsfg::LsfgStatus::Ok {
            crate::logf!(gndrm_obf::obf!(
                "lsfg: shader cache unusable ({:?}); frame generation off",
                status
            ));
            return None;
        }

        let Ok(cache_c) = std::ffi::CString::new(cache.as_os_str().as_encoded_bytes()) else {
            return None;
        };
        let root = unsafe { (engine.api.create)(device, physical, cache_c.as_ptr()) };
        if root.is_null() {
            crate::logf!(gndrm_obf::obf!(
                "lsfg: engine create failed; frame generation off"
            ));
            return None;
        }

        let flow = from_daemon
            .filter(|s| s.flow_per_mille != 0)
            .map(|s| s.flow_per_mille as f32 / 1000.0)
            .or_else(|| std::env::var(FLOW_ENV).ok().and_then(|v| v.parse().ok()))
            .unwrap_or(0.8)
            .clamp(0.25, 1.0);
        let source_cap = from_daemon
            .map(|s| s.fps_limit)
            .or_else(|| std::env::var(CAP_ENV).ok().and_then(|v| v.parse().ok()))
            .unwrap_or(0);
        unsafe { (engine.api.configure)(root, multiplier, 0, flow, 0.0) };
        let (source, output) = grid();
        crate::logf!(gndrm_obf::obf!(
            "lsfg: ready multiplier={} fp16={} source={} output={} cache={}",
            multiplier,
            fp16,
            source,
            output,
            cache.display()
        ));

        // Seed the live state with this session's decision: a push only arrives
        // when the user changes something, so `live()` must already be right.
        apply(gndrm_uapi::wire::LsfgConfig {
            enabled: 1,
            multiplier,
            flow_per_mille: (flow * 1000.0) as u32,
            performance: from_daemon.map_or(0, |s| s.performance),
            fps_limit: source_cap,
        });

        Some(Self {
            engine,
            root,
            multiplier,
        })
    }

    /// The multiplier this session was created for.
    #[must_use]
    pub fn multiplier(&self) -> u32 {
        self.multiplier
    }

    /// The engine's entry points, for the swapchain and present paths.
    #[must_use]
    pub fn api(&self) -> &Api {
        &self.engine.api
    }

    /// The `VkrLsfg*`.
    #[must_use]
    pub fn root(&self) -> *mut c_void {
        self.root
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.root.is_null() {
            unsafe { (self.engine.api.destroy)(self.root) };
            self.root = std::ptr::null_mut();
        }
    }
}
