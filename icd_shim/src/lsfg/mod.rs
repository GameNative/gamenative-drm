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
        crate::logf!("lsfg: cannot load {}: {}",
            library,
            dlerror_text());
        return None;
    }

    let abi = unsafe { resolve_one::<AbiFn>(handle, "lsfg_engine_abi") };
    let set_host = unsafe { resolve_one::<SetHostFn>(handle, "lsfg_engine_set_host") };
    let (Some(abi), Some(set_host)) = (abi, set_host) else {
        crate::logf!("lsfg: {} is not an engine", library);
        return None;
    };
    let revision = unsafe { abi() };
    if revision != EXPECTED_ABI {
        crate::logf!("lsfg: {} speaks ABI {}, this build speaks {}",
            library,
            revision,
            EXPECTED_ABI);
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
    crate::logf!("lsfg: engine ready ({})", library);
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
    /// The QuickMenu's present mode for generation: 0 mailbox, 1 fifo.
    present_mode: std::sync::atomic::AtomicU32,
    refresh_mhz: std::sync::atomic::AtomicU32,
    /// The panel's latch grid as the daemon last reported it; a period of 0
    /// means no report yet.
    vsync_anchor_ns: std::sync::atomic::AtomicU64,
    vsync_period_ns: std::sync::atomic::AtomicU32,
    vsync_lead_ns: std::sync::atomic::AtomicU32,
}

static LIVE: Live = Live {
    enabled: std::sync::atomic::AtomicU32::new(0),
    multiplier: std::sync::atomic::AtomicU32::new(0),
    flow_per_mille: std::sync::atomic::AtomicU32::new(0),
    fps_limit: std::sync::atomic::AtomicU32::new(0),
    present_mode: std::sync::atomic::AtomicU32::new(0),
    refresh_mhz: std::sync::atomic::AtomicU32::new(0),
    vsync_anchor_ns: std::sync::atomic::AtomicU64::new(0),
    vsync_period_ns: std::sync::atomic::AtomicU32::new(0),
    vsync_lead_ns: std::sync::atomic::AtomicU32::new(0),
};

/// The last settings applied to this process, kept because a session opened
/// later than the push must not have to ask the daemon again — the request can
/// fail while the push already succeeded, and the settings carry the DLL path
/// the engine cannot be opened without.
static APPLIED: std::sync::Mutex<Option<gndrm_uapi::wire::LsfgConfig>> =
    std::sync::Mutex::new(None);

/// The last settings applied here, if any.
#[must_use]
pub fn applied() -> Option<gndrm_uapi::wire::LsfgConfig> {
    *APPLIED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bumped whenever the multiplier or the on/off state changes. The session is
/// opened lazily on the present path — the app's settings arrive after the
/// device exists — so a change has to be visible as "something new to act on"
/// rather than as a retry on every frame.
static EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The generation factor to run at, or `None` when the user has it off.
#[must_use]
pub fn wanted() -> Option<u32> {
    let (enabled, multiplier, _) = live();
    (enabled && multiplier >= 2).then_some(multiplier)
}

/// The settings epoch: equal to `wanted`'s generation changes.
#[must_use]
pub fn epoch() -> u32 {
    EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Adopt a settings frame. `enabled = 0` is the user turning generation off and
/// is honoured as such — it does not fall back to the launch-time environment.
pub fn apply(config: gndrm_uapi::wire::LsfgConfig) {
    use std::sync::atomic::Ordering::Relaxed;
    *APPLIED.lock().unwrap_or_else(|e| e.into_inner()) = Some(config);
    let previous = live();
    if previous.0 != (config.enabled != 0) || previous.1 != config.multiplier {
        EPOCH.fetch_add(1, Relaxed);
    }
    LIVE.enabled.store(config.enabled, Relaxed);
    LIVE.multiplier.store(config.multiplier, Relaxed);
    LIVE.flow_per_mille.store(config.flow_per_mille, Relaxed);
    LIVE.fps_limit.store(config.fps_limit, Relaxed);
    LIVE.present_mode.store(config.present_mode, Relaxed);
    LIVE.refresh_mhz.store(config.refresh_mhz, Relaxed);
    let now = (config.enabled != 0, config.multiplier, config.fps_limit);
    if previous != now {
        crate::logf!("lsfg: settings enabled={} multiplier={} source_cap={} refresh_hz={}",
            now.0,
            now.1,
            now.2,
            refresh_hz());
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

/// Diagnostic bisect switches for the generation path.
///
/// `GNDRM_LSFG_SKIP=grow` keeps the destination ring at its first size;
/// `GNDRM_LSFG_SKIP=commit` runs the whole chain but never publishes a
/// generated frame; `grow,commit` does both. Empty/unset is the shipping
/// behaviour. Read once per process.
#[must_use]
pub fn skip(what: &str) -> bool {
    static SWITCHES: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let switches = SWITCHES.get_or_init(|| {
        std::env::var("GNDRM_LSFG_SKIP")
            .unwrap_or_default()
            .to_lowercase()
    });
    switches.split(',').any(|s| s.trim() == what)
}

/// The rate the guest should be paced to while frame generation runs.
/// The panel's refresh in whole hertz, or 0 when the daemon has not reported it.
#[must_use]
pub fn refresh_hz() -> u32 {
    LIVE.refresh_mhz
        .load(std::sync::atomic::Ordering::Relaxed)
        .div_euclid(1000)
}

/// Adopt the panel's latch grid from the daemon.
pub fn apply_vsync(note: gndrm_uapi::wire::VsyncNote) {
    use std::sync::atomic::Ordering::Relaxed;
    let had = LIVE.vsync_period_ns.load(Relaxed);
    LIVE.vsync_anchor_ns.store(note.anchor_ns, Relaxed);
    LIVE.vsync_period_ns.store(note.period_ns, Relaxed);
    LIVE.vsync_lead_ns.store(note.lead_ns, Relaxed);
    if had != note.period_ns {
        crate::logf!("lsfg: panel grid period={}us lead={}us",
            note.period_ns / 1000,
            note.lead_ns / 1000);
    }
}

/// The panel's latch grid, or `None` until the daemon has reported one.
#[must_use]
pub fn latch_grid() -> Option<gndrm_lsfg::pacing::Grid> {
    use std::sync::atomic::Ordering::Relaxed;
    let period = LIVE.vsync_period_ns.load(Relaxed);
    (period != 0).then(|| gndrm_lsfg::pacing::Grid {
        anchor_ns: LIVE.vsync_anchor_ns.load(Relaxed),
        period_ns: u64::from(period),
        lead_ns: u64::from(LIVE.vsync_lead_ns.load(Relaxed)),
    })
}

/// The panel's rate in whole hertz: from its latch grid when the daemon has
/// reported one, else the rate the app reported, else 0.
#[must_use]
pub fn panel_hz() -> u32 {
    let period = LIVE
        .vsync_period_ns
        .load(std::sync::atomic::Ordering::Relaxed);
    (1_000_000_000 + period / 2)
        .checked_div(period)
        .unwrap_or_else(refresh_hz)
}

/// One generation's render target: the engine writes here, and the shim blits the
/// result into a destination slot.
///
/// Optimal tiling, because `generate_into` writes through a storage image and this
/// driver does not support storage on a LINEAR image — the destination slots are
/// LINEAR dma-bufs, which is why the copy exists.
pub struct Target {
    pub image: crate::vk::VkImage,
    pub view: crate::vk::VkImageView,
    mem: crate::vk::VkDeviceMemory,
    pub width: u32,
    pub height: u32,
    pub format: crate::vk::VkFormat,
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
    targets: Vec<Target>,
    /// The panel rate last handed to the engine, in hertz.
    refresh_rate: f32,
    /// Source frames handed to the engine; the pacer uses it to count warm-up.
    source_frames: u64,
    /// Geometry the engine was last prepared for, including the guest area:
    /// that area decides the chain's working scale, so a change to it rebuilds.
    prepared: Option<(u32, u32, crate::vk::VkFormat, u32, u32)>,
    /// The largest area the guest draws, as last reported.
    guest_extent: (u32, u32),
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
        // The daemon first, then whatever a push already delivered: the request
        // can fail while the push succeeded, and both carry the DLL path.
        let from_daemon = crate::scanout::lsfg_config().or_else(applied);
        // A configured session answers for both directions: `enabled = 0` is the
        // user turning it off, and must not fall through to the environment, which
        // was captured at launch.
        if let Some(config) = from_daemon {
            if config.enabled == 0 || config.multiplier < 2 {
                crate::logf!("lsfg: disabled by the app");
                return None;
            }
        }
        let Some(multiplier) = from_daemon.map(|s| s.multiplier) else {
            crate::logf!("lsfg: the app has sent no settings");
            return None;
        };
        if multiplier < 2 {
            return None;
        }
        // The settings carry the container's DLL path: this process is the
        // app's, where the container's environment never arrives. The variable
        // stays as the guest-side answer for a launch that predates them.
        let dll = from_daemon.as_ref().and_then(|c| {
            c.dll_path()
                .map(|p| std::path::PathBuf::from(p.to_string_lossy().into_owned()))
        });
        let Some(dll) = dll else {
            crate::logf!("lsfg: no Lossless.dll path from the app");
            return None;
        };
        if !dll.is_file() {
            crate::logf!("lsfg: no Lossless.dll at {}; frame generation off",
                dll.display());
            return None;
        }

        let engine = open()?;
        if !unsafe { (engine.api.vkd_load)(instance, device, gipa) } {
            crate::logf!("lsfg: dispatch load failed; frame generation off");
            return None;
        }

        // Cache beside the DLL unless the caller named one, and keyed by the
        // variant so a device that cannot run fp16 never invalidates the fp32
        // build (and vice versa).
        // Beside the DLL, keyed by the variant so a device that cannot run fp16
        // never invalidates the fp32 build (and vice versa).
        let cache = dll.with_file_name(if fp16 {
            "lossless_fp16.lsfgc"
        } else {
            "lossless_fp32.lsfgc"
        });
        let status = gndrm_lsfg::ensure_cache(&dll, &cache, fp16);
        if status != gndrm_lsfg::LsfgStatus::Ok {
            crate::logf!("lsfg: shader cache unusable ({:?}); frame generation off",
                status);
            return None;
        }

        let Ok(cache_c) = std::ffi::CString::new(cache.as_os_str().as_encoded_bytes()) else {
            return None;
        };
        let root = unsafe { (engine.api.create)(device, physical, cache_c.as_ptr()) };
        if root.is_null() {
            crate::logf!("lsfg: engine create failed; frame generation off");
            return None;
        }

        let flow = from_daemon
            .filter(|s| s.flow_per_mille != 0)
            .map(|s| s.flow_per_mille as f32 / 1000.0)
            .unwrap_or(0.8)
            .clamp(0.25, 1.0);
        let source_cap = from_daemon.map(|s| s.fps_limit).unwrap_or(0);
        // The panel rate is what lets the pacer clamp: without it the engine
        // plans `multiplier` generations whatever the display can show.
        let refresh = from_daemon
            .map(|s| s.refresh_mhz as f32 / 1000.0)
            .unwrap_or(0.0);
        unsafe { (engine.api.configure)(root, multiplier, 0, flow, refresh) };
        // What the session allows is `crate::limits`'s answer, printed by the
        // shim's census; here the engine reports only its own configuration.
        crate::logf!("lsfg: ready multiplier={} fp16={} cap={} refresh_hz={} cache={}",
            multiplier,
            fp16,
            source_cap,
            refresh,
            cache.display());

        // Seed the live state with this session's decision: a push only arrives
        // when the user changes something, so `live()` must already be right.
        let mut current = gndrm_uapi::wire::LsfgConfig {
            enabled: 1,
            multiplier,
            flow_per_mille: (flow * 1000.0) as u32,
            performance: from_daemon.map_or(0, |s| s.performance),
            fps_limit: source_cap,
            dll: [0; gndrm_uapi::wire::LSFG_DLL_PATH_MAX],
            // Mailbox unless the settings ask for fifo; the QuickMenu's choice
            // travels on the note, so it reaches here through `from_daemon`.
            present_mode: from_daemon.map_or(0, |s| s.present_mode),
            refresh_mhz: (refresh * 1000.0) as u32,
        };
        current.set_dll_path(&dll.to_string_lossy());
        apply(current);

        Some(Self {
            engine,
            root,
            multiplier,
            targets: Vec::new(),
            refresh_rate: refresh,
            source_frames: 0,
            prepared: None,
            guest_extent: (0, 0),
        })
    }

    /// The multiplier this session was created for.
    #[must_use]
    pub fn multiplier(&self) -> u32 {
        self.multiplier
    }

    /// Follow a change in the panel rate.
    ///
    /// The app reports it before the session starts, but a display mode change can
    /// arrive later; the engine's pacer is the only consumer and it reads the
    /// value on every plan.
    pub fn sync_refresh_rate(&mut self) {
        let hz = refresh_hz() as f32;
        if (hz - self.refresh_rate).abs() < 0.5 {
            return;
        }
        unsafe { (self.engine.api.set_refresh_rate)(self.root, hz) };
        self.refresh_rate = hz;
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

impl Session {
    /// Make sure the engine is prepared for this geometry and that one render
    /// target per generation exists.
    ///
    /// # Safety
    /// `device` and `f` must belong to the real device this session was opened on.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn prepare(
        &mut self,
        device: crate::vk::VkDevice,
        f: &crate::proxy::RealDevFns,
        mem_props: &crate::vk::VkPhysicalDeviceMemoryProperties,
        width: u32,
        height: u32,
        format: crate::vk::VkFormat,
    ) -> bool {
        let count = self.multiplier.saturating_sub(1);
        if count == 0 {
            return false;
        }
        // The engine writes into its target through an `rgba8` storage image, so
        // the images it is given must be R,G,B,A in byte order, whatever order
        // the scan-out slots are in. See `engine_storage_format`.
        let format = crate::vk::engine_storage_format(format);
        let key = (width, height, format, self.guest_extent.0, self.guest_extent.1);
        if self.prepared != Some(key) {
            if unsafe { (self.engine.api.prepare)(self.root, width, height, format) } {
                self.prepared = Some(key);
            } else {
                return false;
            }
        }
        if self.targets.len() == count as usize
            && self
                .targets
                .first()
                .is_some_and(|t| t.width == width && t.height == height && t.format == format)
        {
            return true;
        }
        unsafe { self.drop_targets(device, f) };

        let (
            Some(create_image),
            Some(get_reqs),
            Some(alloc),
            Some(bind),
            Some(view_fn),
            Some(free),
        ) = (
            f.create_image,
            f.get_mem_reqs,
            f.alloc_mem,
            f.bind_mem,
            f.create_image_view,
            f.free_mem,
        )
        else {
            return false;
        };
        let Some(destroy_image) = f.destroy_image else {
            return false;
        };

        for _ in 0..count {
            let mut ici: crate::vk::VkImageCreateInfo = unsafe { std::mem::zeroed() };
            ici.sType = crate::vk::VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO;
            ici.imageType = crate::vk::VK_IMAGE_TYPE_2D;
            ici.format = format;
            ici.extent = crate::vk::VkExtent3D {
                width,
                height,
                depth: 1,
            };
            ici.mipLevels = 1;
            ici.arrayLayers = 1;
            ici.samples = crate::vk::VK_SAMPLE_COUNT_1_BIT;
            ici.tiling = crate::vk::VK_IMAGE_TILING_OPTIMAL;
            // The engine no longer writes this image: it generates into its own
            // image (a compute store into an image the engine did not create
            // never lands on this driver) and copies the result here. So this is
            // a transfer image only: filled by that copy, blitted out to a slot.
            ici.usage = crate::vk::VK_IMAGE_USAGE_TRANSFER_SRC_BIT
                | crate::vk::VK_IMAGE_USAGE_TRANSFER_DST_BIT;
            ici.sharingMode = crate::vk::VK_SHARING_MODE_EXCLUSIVE;
            ici.initialLayout = crate::vk::VK_IMAGE_LAYOUT_UNDEFINED;

            let mut image: crate::vk::VkImage = 0;
            if unsafe { create_image(device, &ici, std::ptr::null(), &mut image) }
                != crate::vk::VK_SUCCESS
            {
                crate::logf!("lsfg: target image create failed");
                return false;
            }

            let mut reqs: crate::vk::VkMemoryRequirements = unsafe { std::mem::zeroed() };
            unsafe { get_reqs(device, image, &mut reqs) };
            let mut ai: crate::vk::VkMemoryAllocateInfo = unsafe { std::mem::zeroed() };
            ai.sType = crate::vk::VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
            ai.allocationSize = reqs.size;
            // A render target belongs in device-local memory. The lowest matching
            // type is not always one: on this driver a host-visible system heap can
            // come first, and a compute store into such an image never lands (the
            // engine's generate pass writes nothing while a clear into the same
            // image does). Prefer DEVICE_LOCAL and fall back to the first match.
            let mut fallback = u32::MAX;
            for i in 0..32 {
                if reqs.memoryTypeBits & (1 << i) == 0 {
                    continue;
                }
                if fallback == u32::MAX {
                    fallback = i;
                }
                if mem_props.memoryTypes[i as usize].propertyFlags
                    & crate::vk::VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT
                    != 0
                {
                    ai.memoryTypeIndex = i;
                    break;
                }
            }
            if ai.memoryTypeIndex == 0 && fallback != u32::MAX {
                ai.memoryTypeIndex = fallback;
            }
            let mut mem: crate::vk::VkDeviceMemory = 0;
            if unsafe { alloc(device, &ai, std::ptr::null(), &mut mem) } != crate::vk::VK_SUCCESS {
                unsafe { destroy_image(device, image, std::ptr::null()) };
                crate::logf!("lsfg: target memory alloc failed");
                return false;
            }
            if unsafe { bind(device, image, mem, 0) } != crate::vk::VK_SUCCESS {
                unsafe { free(device, mem, std::ptr::null()) };
                unsafe { destroy_image(device, image, std::ptr::null()) };
                crate::logf!("lsfg: target bind failed");
                return false;
            }

            let mut vci: crate::vk::VkImageViewCreateInfo = unsafe { std::mem::zeroed() };
            vci.sType = crate::vk::VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO;
            vci.image = image;
            vci.viewType = crate::vk::VK_IMAGE_VIEW_TYPE_2D;
            vci.format = format;
            vci.subresourceRange.aspectMask = crate::vk::VK_IMAGE_ASPECT_COLOR_BIT;
            vci.subresourceRange.levelCount = 1;
            vci.subresourceRange.layerCount = 1;
            let mut view: crate::vk::VkImageView = 0;
            if unsafe { view_fn(device, &vci, std::ptr::null(), &mut view) }
                != crate::vk::VK_SUCCESS
            {
                unsafe { free(device, mem, std::ptr::null()) };
                unsafe { destroy_image(device, image, std::ptr::null()) };
                crate::logf!("lsfg: target view create failed");
                return false;
            }

            self.targets.push(Target {
                image,
                view,
                mem,
                width,
                height,
                format,
            });
        }
        crate::logf!("lsfg: {} target(s) {}x{} fmt {}",
            self.targets.len(),
            width,
            height,
            format);
        true
    }

    /// The largest area the guest draws, so the engine sizes its working
    /// resolution to the content rather than to the window around it.
    pub fn set_guest_extent(&mut self, width: u32, height: u32) {
        self.guest_extent = (width, height);
        unsafe { (self.engine.api.set_guest_extent)(self.root, width, height) }
    }

    /// Generations this source frame yields, and advance the frame counter.
    pub fn plan(&mut self, capacity: u32) -> u32 {
        let frames = self.source_frames;
        self.source_frames = self.source_frames.wrapping_add(1);
        unsafe { (self.engine.api.plan)(self.root, capacity, frames) }
    }

    #[must_use]
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// Free the device resources this session holds, before it is dropped: the
    /// engine context goes in `Drop`, but the targets need the device's table.
    ///
    /// # Safety
    /// `device` and `f` must belong to the real device this session was opened on.
    pub unsafe fn close(&mut self, device: crate::vk::VkDevice, f: &crate::proxy::RealDevFns) {
        unsafe { self.drop_targets(device, f) };
    }

    unsafe fn drop_targets(&mut self, device: crate::vk::VkDevice, f: &crate::proxy::RealDevFns) {
        for target in self.targets.drain(..) {
            // No `vkDestroyImageView` in the device table; the view dies with the
            // image, which the device destroys on teardown.
            if let Some(destroy_image) = f.destroy_image {
                unsafe { destroy_image(device, target.image, std::ptr::null()) };
            }
            if let Some(free) = f.free_mem {
                unsafe { free(device, target.mem, std::ptr::null()) };
            }
        }
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


/// The present mode the QuickMenu chose for generation: 0 = mailbox, 1 = fifo.
/// Zero until a config arrives, which is mailbox — the default the app ships.
pub(crate) fn generating_present_mode() -> u32 {
    LIVE.present_mode.load(std::sync::atomic::Ordering::Relaxed)
}
