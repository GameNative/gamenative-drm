//! Vendor-independent GPU busy time from per-submit timestamps.
//!
//! The app has no portable way to read GPU load: kgsl exposes a busy counter on
//! Adreno, but Mali and PowerVR either hide theirs or report something else. The
//! shim sees every queue submit of the session's Vulkan processes, so it can
//! bracket each one with two timestamps and sum the time the GPU spent between
//! them.
//!
//! Shape, per (device, queue family that can write timestamps):
//!
//!   - one `TIMESTAMP` query pool of `2 * SLOTS` queries and a ring of `SLOTS`
//!     slots; each slot owns two pre-recorded command buffers: *begin* resets
//!     the slot's pair and writes a `TOP_OF_PIPE` timestamp, *end* writes a
//!     `BOTTOM_OF_PIPE` one;
//!   - an intercepted submit gets *begin* prepended to its first batch and *end*
//!     appended to its last, in arrays the shim builds anyway; no extra
//!     `vkQueueSubmit` is ever made. When the ring is full the submit goes out
//!     untimed and is counted;
//!   - results are read on present (and from submit at most every
//!     `DRAIN_EVERY_NS`) with `vkGetQueryPoolResults(64_BIT | WITH_AVAILABILITY)`
//!     and never `WAIT`: the drain stops at the first slot that is not ready.
//!
//! Intervals are masked to the family's `timestampValidBits`, unwrapped, and
//! merged as a union per family (overlap between queues of one family counts
//! once). Families are not assumed to share a clock domain, so a device's busy
//! time over a drain window is the largest of its families' busy time in that
//! window; devices are summed per process.
//!
//! Export: at most once a second, `$GN_GPUTIME_DIR/<pid>` is replaced (write a
//! temporary file, then rename) with one line:
//!
//! ```text
//! v1 <role> <busy_ns> <wall_ns> <presents> <untimed> <updated_mono_ns> <devices> <timed>
//! ```
//!
//! All counters are cumulative since the first timer of the process was made;
//! `wall_ns` and `updated_mono_ns` are `CLOCK_MONOTONIC`. `role` is
//! `guest`, `gamescope` or `xwayland`. `timed`/`untimed` count submits. The file
//! is removed when the process's last timed device is destroyed.
//!
//! Without `GN_GPUTIME_DIR` every entry point returns after one load of a
//! cached flag: no query pools, no command buffers, no file.

use crate::vk::*;
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::sync::{Mutex, OnceLock};

/// Slots per family ring: a slot is in flight from its submit until its end
/// timestamp has been read back, so this bounds the submits outstanding on the
/// GPU (plus up to one drain period) before submits go out untimed.
pub const SLOTS: usize = 128;
/// Submit-side drain period, for processes that never present (gamescope
/// scans out through KMS, not `vkQueuePresentKHR`).
const DRAIN_EVERY_NS: u64 = 100_000_000;
/// File refresh period.
const EXPORT_EVERY_NS: u64 = 1_000_000_000;
/// A slot still unread this long after its submit is given up on (a lost
/// device, or a submit the driver accepted but never ran), so one bad slot
/// cannot stall the ring forever.
const ABANDON_AFTER_NS: u64 = 5_000_000_000;
/// One interval longer than this is not a frame: treat it as garbage.
const MAX_INTERVAL_NS: f64 = 5_000_000_000.0;

const VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO: VkStructureType = 11;
const VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO: VkStructureType = 1000314006;
const VK_STRUCTURE_TYPE_DEVICE_GROUP_SUBMIT_INFO: VkStructureType = 1000060005;
const VK_STRUCTURE_TYPE_PROTECTED_SUBMIT_INFO: VkStructureType = 1000145000;
const VK_QUERY_TYPE_TIMESTAMP: i32 = 2;
const VK_QUERY_RESULT_64_BIT: u32 = 0x1;
const VK_QUERY_RESULT_WITH_AVAILABILITY_BIT: u32 = 0x4;
const VK_COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE_BIT: u32 = 0x4;
const VK_QUEUE_GRAPHICS_BIT: u32 = 0x1;
const VK_QUEUE_COMPUTE_BIT: u32 = 0x2;
const VK_SUBMIT_PROTECTED_BIT: u32 = 0x1;
/// `VkPhysicalDeviceLimits::timestampPeriod` offset inside the limits block.
const LIMITS_TIMESTAMP_PERIOD_OFF: usize = 424;

type VkQueryPool = u64;

#[repr(C)]
struct VkQueryPoolCreateInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    flags: u32,
    query_type: i32,
    query_count: u32,
    pipeline_statistics: u32,
}

#[repr(C)]
struct BaseIn {
    s_type: VkStructureType,
    p_next: *const BaseIn,
}

type PfnCreateQueryPool = unsafe extern "C" fn(
    VkDevice,
    *const VkQueryPoolCreateInfo,
    *const c_void,
    *mut VkQueryPool,
) -> VkResult;
type PfnDestroyQueryPool = unsafe extern "C" fn(VkDevice, VkQueryPool, *const c_void);
type PfnCreateCommandPool = unsafe extern "C" fn(
    VkDevice,
    *const VkCommandPoolCreateInfo,
    *const c_void,
    *mut VkCommandPool,
) -> VkResult;
type PfnDestroyCommandPool = unsafe extern "C" fn(VkDevice, VkCommandPool, *const c_void);
type PfnAllocateCommandBuffers = unsafe extern "C" fn(
    VkDevice,
    *const VkCommandBufferAllocateInfo,
    *mut VkCommandBuffer,
) -> VkResult;
type PfnBeginCommandBuffer =
    unsafe extern "C" fn(VkCommandBuffer, *const VkCommandBufferBeginInfo) -> VkResult;
type PfnEndCommandBuffer = unsafe extern "C" fn(VkCommandBuffer) -> VkResult;
type PfnCmdResetQueryPool = unsafe extern "C" fn(VkCommandBuffer, VkQueryPool, u32, u32);
type PfnCmdWriteTimestamp = unsafe extern "C" fn(VkCommandBuffer, u32, VkQueryPool, u32);
type PfnGetQueryPoolResults =
    unsafe extern "C" fn(VkDevice, VkQueryPool, u32, u32, usize, *mut c_void, u64, u32) -> VkResult;
type PfnGetQueueFamilyProperties =
    unsafe extern "C" fn(VkPhysicalDevice, *mut u32, *mut VkQueueFamilyProperties);

/// The driver entry points a timer needs, resolved once per device.
#[derive(Clone, Copy)]
pub struct Fns {
    create_query_pool: PfnCreateQueryPool,
    destroy_query_pool: PfnDestroyQueryPool,
    create_cmd_pool: PfnCreateCommandPool,
    destroy_cmd_pool: PfnDestroyCommandPool,
    alloc_cmd: PfnAllocateCommandBuffers,
    begin: PfnBeginCommandBuffer,
    end: PfnEndCommandBuffer,
    reset_query_pool: PfnCmdResetQueryPool,
    write_timestamp: PfnCmdWriteTimestamp,
    get_results: PfnGetQueryPoolResults,
}

impl Fns {
    /// Resolve every entry point through `lookup`; `None` if any is missing.
    pub fn resolve(lookup: impl Fn(&CStr) -> PFN_vkVoidFunction) -> Option<Self> {
        macro_rules! get {
            ($name:literal) => {{
                let f = lookup($name)?;
                // SAFETY: a resolved Vulkan entry point punned to its own
                // signature; every target is fn-pointer-sized.
                unsafe { std::mem::transmute::<unsafe extern "C" fn(), _>(f) }
            }};
        }
        Some(Self {
            create_query_pool: get!(c"vkCreateQueryPool"),
            destroy_query_pool: get!(c"vkDestroyQueryPool"),
            create_cmd_pool: get!(c"vkCreateCommandPool"),
            destroy_cmd_pool: get!(c"vkDestroyCommandPool"),
            alloc_cmd: get!(c"vkAllocateCommandBuffers"),
            begin: get!(c"vkBeginCommandBuffer"),
            end: get!(c"vkEndCommandBuffer"),
            reset_query_pool: get!(c"vkCmdResetQueryPool"),
            write_timestamp: get!(c"vkCmdWriteTimestamp"),
            get_results: get!(c"vkGetQueryPoolResults"),
        })
    }
}

// ---------------------------------------------------------------------------
// Pure parts (unit-tested on the host).

/// The mask of a `timestampValidBits` value (0 means "no timestamps").
pub fn valid_mask(bits: u32) -> u64 {
    match bits {
        0 => 0,
        b if b >= 64 => u64::MAX,
        b => (1u64 << b) - 1,
    }
}

/// Extend a masked counter reading to 64 bits, choosing the wrap epoch that
/// puts it closest to `prev` (the last unwrapped value on this family). With
/// 64 valid bits the reading already is the value.
pub fn unwrap_ticks(raw: u64, prev: u64, mask: u64) -> u64 {
    if mask == u64::MAX {
        return raw;
    }
    let raw = raw & mask;
    let span = mask + 1;
    let half = span / 2;
    let mut c = (prev & !mask) | raw;
    if c + half < prev {
        c += span;
    } else if c > prev + half && c >= span {
        c -= span;
    }
    c
}

/// One begin/end pair as read back, masked to the valid bits, turned into an
/// absolute interval in ticks. `prev` is the family's unwrap reference and is
/// advanced. Returns `None` for an interval that cannot be a real submit.
pub fn interval(begin_raw: u64, end_raw: u64, mask: u64, prev: &mut u64) -> Option<(u64, u64)> {
    let b = unwrap_ticks(begin_raw, *prev, mask);
    let dur = end_raw.wrapping_sub(begin_raw) & mask;
    // A begin later than its end on a 64-bit counter is a reading the driver
    // did not produce for one submit (the masked subtraction would turn it into
    // an enormous duration, caught by the caller's length check).
    let e = b.checked_add(dur)?;
    *prev = e;
    Some((b, e))
}

/// Add `ivs` (sorted here by begin) to a running union whose covered prefix
/// ends at `*covered_until`; returns the newly covered ticks.
pub fn union_add(covered_until: &mut u64, ivs: &mut [(u64, u64)]) -> u64 {
    ivs.sort_unstable_by_key(|iv| iv.0);
    let mut added = 0u64;
    for &(b, e) in ivs.iter() {
        if e <= *covered_until {
            continue;
        }
        let start = b.max(*covered_until);
        added += e - start;
        *covered_until = e;
    }
    added
}

/// Ring bookkeeping: `head` is the next slot sequence to hand out, `tail` the
/// oldest not yet read back. Sequences grow without bound; slot = seq % SLOTS.
#[derive(Default)]
pub struct Ring {
    pub head: u64,
    pub tail: u64,
}

impl Ring {
    pub fn in_flight(&self) -> usize {
        (self.head - self.tail) as usize
    }

    /// The next free slot sequence, or `None` when all `SLOTS` are in flight.
    pub fn alloc(&mut self) -> Option<u64> {
        if self.in_flight() >= SLOTS {
            return None;
        }
        let s = self.head;
        self.head += 1;
        Some(s)
    }
}

/// Build the command-buffer list of a timed batch: `begin` first when the batch
/// is the submit's first, `end` last when it is the submit's last.
pub fn bracket<T: Copy>(orig: &[T], first: bool, last: bool, begin: T, end: T) -> Vec<T> {
    let mut v = Vec::with_capacity(orig.len() + 2);
    if first {
        v.push(begin);
    }
    v.extend_from_slice(orig);
    if last {
        v.push(end);
    }
    v
}

/// The exported line (see the module docs).
#[allow(clippy::too_many_arguments)]
pub fn format_line(
    role: &str,
    busy_ns: u64,
    wall_ns: u64,
    presents: u64,
    untimed: u64,
    updated_mono_ns: u64,
    devices: usize,
    timed: u64,
) -> String {
    format!(
        "v1 {role} {busy_ns} {wall_ns} {presents} {untimed} {updated_mono_ns} {devices} {timed}\n"
    )
}

// ---------------------------------------------------------------------------
// Driver-facing state.

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    Pending,
    Abandoned,
}

struct Slot {
    begin_cb: VkCommandBuffer,
    end_cb: VkCommandBuffer,
    state: SlotState,
    submitted_ns: u64,
    /// The raw pair read for this slot's previous use: until the slot's reset
    /// has run on the GPU, a read returns these (still flagged available).
    last_raw: (u64, u64),
}

struct FamilyTimer {
    pool: VkQueryPool,
    cmd_pool: VkCommandPool,
    mask: u64,
    slots: Vec<Slot>,
    ring: Ring,
    unwrap_prev: u64,
    covered_until: u64,
    /// Busy nanoseconds in the current drain window.
    window_busy_ns: f64,
}

struct DevTimer {
    real: VkDevice,
    fns: Fns,
    period_ns: f64,
    families: HashMap<u32, FamilyTimer>,
}

struct Proc {
    devices: HashMap<usize, DevTimer>,
    start_ns: u64,
    busy_ns: f64,
    presents: u64,
    timed: u64,
    untimed: u64,
    last_drain_ns: u64,
    last_export_ns: u64,
}

// SAFETY: the raw handles inside are driver objects only ever used under
// STATE's lock; Vulkan handles are not tied to the thread that made them.
unsafe impl Send for Proc {}

static STATE: Mutex<Option<Proc>> = Mutex::new(None);

/// `$GN_GPUTIME_DIR`, read once; `None` disables the whole module.
fn dir() -> Option<&'static std::path::Path> {
    static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var_os("GN_GPUTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    })
    .as_deref()
}

pub fn enabled() -> bool {
    dir().is_some()
}

fn lock() -> std::sync::MutexGuard<'static, Option<Proc>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Create the per-family timers of a new device. `families` are the queue
/// families the device was created with; `qfp` the physical device's family
/// properties; `limits` the raw `VkPhysicalDeviceLimits` block.
///
/// # Safety
/// `real` must be a live device created with those families; `fns` resolved
/// for it.
pub unsafe fn device_created(
    key: usize,
    real: VkDevice,
    fns: Fns,
    families: &[u32],
    qfp: &[VkQueueFamilyProperties],
    limits: &[u8],
) {
    if !enabled() {
        return;
    }
    let period_ns = limits
        .get(LIMITS_TIMESTAMP_PERIOD_OFF..LIMITS_TIMESTAMP_PERIOD_OFF + 4)
        .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as f64)
        .unwrap_or(0.0);
    if !(period_ns > 0.0 && period_ns.is_finite()) {
        crate::logi!(gndrm_obf::obf!(
            "gputime: device has no usable timestampPeriod ({period_ns}) — untimed"
        ));
        return;
    }
    let mut dt = DevTimer {
        real,
        fns,
        period_ns,
        families: HashMap::new(),
    };
    for &fam in families {
        if dt.families.contains_key(&fam) {
            continue;
        }
        let Some(p) = qfp.get(fam as usize) else {
            continue;
        };
        let mask = valid_mask(p.timestampValidBits);
        // vkCmdResetQueryPool needs a graphics or compute queue.
        if mask == 0 || p.queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT) == 0 {
            continue;
        }
        if let Some(ft) = unsafe { family_timer(real, &fns, fam, mask) } {
            dt.families.insert(fam, ft);
        }
    }
    let n = dt.families.len();
    crate::logi!(gndrm_obf::obf!(
        "gputime: device {key:#x} timed on {n} famil(ies), period {period_ns} ns"
    ));
    if n == 0 {
        return;
    }
    let now = crate::proxy::monotonic_ns();
    let mut g = lock();
    let p = g.get_or_insert_with(|| Proc {
        devices: HashMap::new(),
        start_ns: now,
        busy_ns: 0.0,
        presents: 0,
        timed: 0,
        untimed: 0,
        last_drain_ns: now,
        last_export_ns: 0,
    });
    p.devices.insert(key, dt);
}

unsafe fn family_timer(real: VkDevice, f: &Fns, fam: u32, mask: u64) -> Option<FamilyTimer> {
    let qci = VkQueryPoolCreateInfo {
        s_type: VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: 0,
        query_type: VK_QUERY_TYPE_TIMESTAMP,
        query_count: (2 * SLOTS) as u32,
        pipeline_statistics: 0,
    };
    let mut pool: VkQueryPool = 0;
    if unsafe { (f.create_query_pool)(real, &qci, std::ptr::null(), &mut pool) } != VK_SUCCESS {
        return None;
    }
    let mut pci: VkCommandPoolCreateInfo = unsafe { std::mem::zeroed() };
    pci.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO;
    pci.queueFamilyIndex = fam;
    let mut cmd_pool: VkCommandPool = 0;
    if unsafe { (f.create_cmd_pool)(real, &pci, std::ptr::null(), &mut cmd_pool) } != VK_SUCCESS {
        unsafe { (f.destroy_query_pool)(real, pool, std::ptr::null()) };
        return None;
    }
    let mut cbs = vec![std::ptr::null_mut::<c_void>(); 2 * SLOTS];
    let mut ai: VkCommandBufferAllocateInfo = unsafe { std::mem::zeroed() };
    ai.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO;
    ai.commandPool = cmd_pool;
    ai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    ai.commandBufferCount = cbs.len() as u32;
    let fail = |real: VkDevice| unsafe {
        (f.destroy_cmd_pool)(real, cmd_pool, std::ptr::null());
        (f.destroy_query_pool)(real, pool, std::ptr::null());
    };
    if unsafe { (f.alloc_cmd)(real, &ai, cbs.as_mut_ptr()) } != VK_SUCCESS {
        fail(real);
        return None;
    }
    let mut bi: VkCommandBufferBeginInfo = unsafe { std::mem::zeroed() };
    bi.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
    // Re-submitted every time the ring comes round; the read-back proves the
    // previous use finished on the GPU, but not that the driver retired the
    // batch, so the buffers are recorded for simultaneous use.
    bi.flags = VK_COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE_BIT;
    let mut slots = Vec::with_capacity(SLOTS);
    for i in 0..SLOTS {
        let (b, e) = (cbs[2 * i], cbs[2 * i + 1]);
        let q = (2 * i) as u32;
        let ok = unsafe {
            (f.begin)(b, &bi) == VK_SUCCESS
                && {
                    (f.reset_query_pool)(b, pool, q, 2);
                    (f.write_timestamp)(b, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, pool, q);
                    (f.end)(b) == VK_SUCCESS
                }
                && (f.begin)(e, &bi) == VK_SUCCESS
                && {
                    (f.write_timestamp)(e, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, pool, q + 1);
                    (f.end)(e) == VK_SUCCESS
                }
        };
        if !ok {
            fail(real);
            return None;
        }
        slots.push(Slot {
            begin_cb: b,
            end_cb: e,
            state: SlotState::Free,
            submitted_ns: 0,
            last_raw: (0, 0),
        });
    }
    Some(FamilyTimer {
        pool,
        cmd_pool,
        mask,
        slots,
        ring: Ring::default(),
        unwrap_prev: 0,
        covered_until: 0,
        window_busy_ns: 0.0,
    })
}

/// What a timed submit holds until the driver has the batch: the rebuilt
/// arrays it points at, and the slot to roll back if the submit fails.
pub struct Timed<S> {
    pub submits: Vec<S>,
    _cbs: Vec<Vec<VkCommandBuffer>>,
    _cbis: Vec<Vec<VkCommandBufferSubmitInfo>>,
    key: usize,
    family: u32,
    seq: u64,
}

/// Why a submit goes out untimed, if it does: chained structs whose arrays are
/// indexed by command buffer, or protected work our buffers cannot join.
unsafe fn chain_blocks(mut p: *const BaseIn) -> bool {
    let mut guard = 0;
    while !p.is_null() && guard < 64 {
        let t = unsafe { (*p).s_type };
        if t == VK_STRUCTURE_TYPE_DEVICE_GROUP_SUBMIT_INFO
            || t == VK_STRUCTURE_TYPE_PROTECTED_SUBMIT_INFO
        {
            return true;
        }
        p = unsafe { (*p).p_next };
        guard += 1;
    }
    false
}

/// Reserve a slot for a submit on `family` of device `key`. `None` = untimed
/// (counted unless the module is off or the device is not timed).
fn reserve(
    key: usize,
    family: u32,
    blocked: bool,
) -> Option<(VkCommandBuffer, VkCommandBuffer, u64)> {
    let mut g = lock();
    let p = g.as_mut()?;
    let now = crate::proxy::monotonic_ns();
    let dt = p.devices.get_mut(&key)?;
    let Some(ft) = dt.families.get_mut(&family).filter(|_| !blocked) else {
        p.untimed += 1;
        return None;
    };
    let Some(seq) = ft.ring.alloc() else {
        p.untimed += 1;
        return None;
    };
    let s = &mut ft.slots[(seq % SLOTS as u64) as usize];
    s.state = SlotState::Pending;
    s.submitted_ns = now;
    p.timed += 1;
    Some((s.begin_cb, s.end_cb, seq))
}

/// The batch array of a timed `vkQueueSubmit`: `begin` prepended to the first
/// batch's command buffers, `end` appended to the last one's. The returned
/// lists own the arrays the returned batches point at.
///
/// # Safety
/// Each batch's `pCommandBuffers` must hold `commandBufferCount` handles.
pub unsafe fn rebuild1(
    src: &[VkSubmitInfo],
    b: VkCommandBuffer,
    e: VkCommandBuffer,
) -> (Vec<VkSubmitInfo>, Vec<Vec<VkCommandBuffer>>) {
    let n = src.len();
    let mut out: Vec<VkSubmitInfo> = src.to_vec();
    let mut cbs = Vec::new();
    for (i, s) in src.iter().enumerate() {
        let (first, last) = (i == 0, i + 1 == n);
        if !first && !last {
            continue;
        }
        let orig = if s.pCommandBuffers.is_null() || s.commandBufferCount == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(s.pCommandBuffers, s.commandBufferCount as usize) }
        };
        let v = bracket(orig, first, last, b, e);
        out[i].commandBufferCount = v.len() as u32;
        out[i].pCommandBuffers = v.as_ptr();
        cbs.push(v);
    }
    (out, cbs)
}

/// Timed copy of a `vkQueueSubmit` batch array (already handle-translated).
///
/// # Safety
/// `submits` must point at `count` valid `VkSubmitInfo`s.
pub unsafe fn wrap_submit(
    key: usize,
    family: u32,
    count: u32,
    submits: *const VkSubmitInfo,
) -> Option<Timed<VkSubmitInfo>> {
    if !enabled() || count == 0 || submits.is_null() {
        return None;
    }
    let src = unsafe { std::slice::from_raw_parts(submits, count as usize) };
    let blocked = src
        .iter()
        .any(|s| unsafe { chain_blocks(s.pNext as *const BaseIn) });
    let (b, e, seq) = reserve(key, family, blocked)?;
    let (out, cbs) = unsafe { rebuild1(src, b, e) };
    Some(Timed {
        submits: out,
        _cbs: cbs,
        _cbis: Vec::new(),
        key,
        family,
        seq,
    })
}

/// Timed copy of a `vkQueueSubmit2` batch array (already handle-translated).
///
/// # Safety
/// `submits` must point at `count` valid `VkSubmitInfo2`s.
pub unsafe fn wrap_submit2(
    key: usize,
    family: u32,
    count: u32,
    submits: *const VkSubmitInfo2,
) -> Option<Timed<VkSubmitInfo2>> {
    if !enabled() || count == 0 || submits.is_null() {
        return None;
    }
    let src = unsafe { std::slice::from_raw_parts(submits, count as usize) };
    let blocked = src.iter().any(|s| s.flags & VK_SUBMIT_PROTECTED_BIT != 0);
    let (b, e, seq) = reserve(key, family, blocked)?;
    let info = |cb: VkCommandBuffer| VkCommandBufferSubmitInfo {
        sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO,
        pNext: std::ptr::null(),
        commandBuffer: cb,
        deviceMask: 0,
    };
    let n = src.len();
    let mut out: Vec<VkSubmitInfo2> = src.to_vec();
    let mut cbis = Vec::new();
    for (i, s) in src.iter().enumerate() {
        let (first, last) = (i == 0, i + 1 == n);
        if !first && !last {
            continue;
        }
        let orig = if s.pCommandBufferInfos.is_null() || s.commandBufferInfoCount == 0 {
            &[][..]
        } else {
            unsafe {
                std::slice::from_raw_parts(s.pCommandBufferInfos, s.commandBufferInfoCount as usize)
            }
        };
        let v = bracket(orig, first, last, info(b), info(e));
        out[i].commandBufferInfoCount = v.len() as u32;
        out[i].pCommandBufferInfos = v.as_ptr();
        cbis.push(v);
    }
    Some(Timed {
        submits: out,
        _cbs: Vec::new(),
        _cbis: cbis,
        key,
        family,
        seq,
    })
}

/// After the driver returned for a timed submit: a failed submit never writes
/// its timestamps, so its slot is released (or skipped by the drain). Then
/// drain if the submit-side period has passed.
pub fn submitted<S>(t: Option<Timed<S>>, res: VkResult) {
    if !enabled() {
        return;
    }
    if let Some(t) = t {
        if res != VK_SUCCESS {
            let mut g = lock();
            if let Some(ft) = g
                .as_mut()
                .and_then(|p| p.devices.get_mut(&t.key))
                .and_then(|d| d.families.get_mut(&t.family))
            {
                let idx = (t.seq % SLOTS as u64) as usize;
                if ft.ring.head == t.seq + 1 {
                    ft.ring.head -= 1;
                    ft.slots[idx].state = SlotState::Free;
                } else {
                    ft.slots[idx].state = SlotState::Abandoned;
                }
            }
        }
    }
    poll(false);
}

/// A present: count it and drain.
pub fn presented() {
    if !enabled() {
        return;
    }
    {
        let mut g = lock();
        match g.as_mut() {
            Some(p) => p.presents += 1,
            None => return,
        }
    }
    poll(true);
}

/// Drain (always on present, else at most every `DRAIN_EVERY_NS`) and export
/// at most every `EXPORT_EVERY_NS`.
fn poll(present: bool) {
    let now = crate::proxy::monotonic_ns();
    let line = {
        let mut g = lock();
        let Some(p) = g.as_mut() else {
            return;
        };
        if !present && now.saturating_sub(p.last_drain_ns) < DRAIN_EVERY_NS {
            return;
        }
        p.last_drain_ns = now;
        unsafe { drain_all(p, now) };
        if now.saturating_sub(p.last_export_ns) < EXPORT_EVERY_NS {
            return;
        }
        p.last_export_ns = now;
        line_of(p, now)
    };
    write_file(&line);
}

fn line_of(p: &Proc, now: u64) -> String {
    format_line(
        crate::process_role(),
        p.busy_ns as u64,
        now.saturating_sub(p.start_ns),
        p.presents,
        p.untimed,
        now,
        p.devices.len(),
        p.timed,
    )
}

unsafe fn drain_all(p: &mut Proc, now: u64) {
    let mut total = 0.0f64;
    for dt in p.devices.values_mut() {
        let mut dev_max = 0.0f64;
        for ft in dt.families.values_mut() {
            unsafe { drain_family(dt.real, &dt.fns, dt.period_ns, ft, now) };
            dev_max = dev_max.max(ft.window_busy_ns);
            ft.window_busy_ns = 0.0;
        }
        total += dev_max;
    }
    p.busy_ns += total;
}

/// Read back every ready slot in submit order, stopping at the first that is
/// not; never waits.
unsafe fn drain_family(real: VkDevice, f: &Fns, period_ns: f64, ft: &mut FamilyTimer, now: u64) {
    let mut ivs: Vec<(u64, u64)> = Vec::new();
    while ft.ring.tail < ft.ring.head {
        let idx = (ft.ring.tail % SLOTS as u64) as usize;
        let slot = &mut ft.slots[idx];
        if slot.state == SlotState::Abandoned {
            slot.state = SlotState::Free;
            ft.ring.tail += 1;
            continue;
        }
        // [value, availability] per query.
        let mut r = [0u64; 4];
        let res = unsafe {
            (f.get_results)(
                real,
                ft.pool,
                (2 * idx) as u32,
                2,
                std::mem::size_of_val(&r),
                r.as_mut_ptr().cast(),
                16,
                VK_QUERY_RESULT_64_BIT | VK_QUERY_RESULT_WITH_AVAILABILITY_BIT,
            )
        };
        let ready = (res == VK_SUCCESS || res == VK_NOT_READY)
            && r[1] != 0
            && r[3] != 0
            && (r[0], r[2]) != slot.last_raw;
        if !ready {
            if now.saturating_sub(slot.submitted_ns) > ABANDON_AFTER_NS {
                slot.state = SlotState::Free;
                ft.ring.tail += 1;
                continue;
            }
            break;
        }
        slot.last_raw = (r[0], r[2]);
        slot.state = SlotState::Free;
        ft.ring.tail += 1;
        if let Some(iv) = interval(r[0], r[2], ft.mask, &mut ft.unwrap_prev) {
            if ((iv.1 - iv.0) as f64) * period_ns <= MAX_INTERVAL_NS {
                ivs.push(iv);
            }
        }
    }
    if !ivs.is_empty() {
        let ticks = union_add(&mut ft.covered_until, &mut ivs);
        ft.window_busy_ns += ticks as f64 * period_ns;
    }
}

fn file_path() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let d = dir()?;
    let pid = std::process::id();
    Some((d.join(pid.to_string()), d.join(format!(".{pid}.tmp"))))
}

fn write_file(line: &str) {
    let Some((path, tmp)) = file_path() else {
        return;
    };
    let write = || -> std::io::Result<()> {
        std::fs::write(&tmp, line.as_bytes())?;
        std::fs::rename(&tmp, &path)
    };
    if write().is_err() {
        // The app clears and recreates the directory at session start; make it
        // once if it is missing, then give up quietly.
        if let Some(d) = dir() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = write();
    }
}

/// Final drain and teardown for a device about to be destroyed (the caller has
/// waited it idle). The file is refreshed, or removed with the last device.
///
/// # Safety
/// The device must still be live and idle.
pub unsafe fn device_destroyed(key: usize) {
    if !enabled() {
        return;
    }
    let now = crate::proxy::monotonic_ns();
    let (line, last) = {
        let mut g = lock();
        let Some(p) = g.as_mut() else {
            return;
        };
        if !p.devices.contains_key(&key) {
            return;
        }
        unsafe { drain_all(p, now) };
        if let Some(dt) = p.devices.remove(&key) {
            for ft in dt.families.values() {
                unsafe {
                    (dt.fns.destroy_cmd_pool)(dt.real, ft.cmd_pool, std::ptr::null());
                    (dt.fns.destroy_query_pool)(dt.real, ft.pool, std::ptr::null());
                }
            }
        }
        (line_of(p, now), p.devices.is_empty())
    };
    if last {
        if let Some((path, _)) = file_path() {
            let _ = std::fs::remove_file(path);
        }
    } else {
        write_file(&line);
    }
}

/// Queue-family properties of `pd` through the instance-level entry point.
///
/// # Safety
/// `f` must be the instance's `vkGetPhysicalDeviceQueueFamilyProperties`.
pub unsafe fn queue_families(
    f: PFN_vkVoidFunction,
    pd: VkPhysicalDevice,
) -> Vec<VkQueueFamilyProperties> {
    let Some(f) = f else {
        return Vec::new();
    };
    let f: PfnGetQueueFamilyProperties = unsafe { std::mem::transmute(f) };
    let mut n = 0u32;
    unsafe { f(pd, &mut n, std::ptr::null_mut()) };
    let mut v: Vec<VkQueueFamilyProperties> = vec![unsafe { std::mem::zeroed() }; n as usize];
    unsafe { f(pd, &mut n, v.as_mut_ptr()) };
    v.truncate(n as usize);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_from_valid_bits() {
        assert_eq!(valid_mask(0), 0);
        assert_eq!(valid_mask(36), (1u64 << 36) - 1);
        assert_eq!(valid_mask(64), u64::MAX);
    }

    #[test]
    fn interval_masks_high_garbage_and_wraps() {
        let mask = valid_mask(32);
        let mut prev = 0;
        // Bits above the valid ones are ignored.
        let iv = interval(
            0xdead_0000_0000_1000,
            0xbeef_0000_0000_1800,
            mask,
            &mut prev,
        )
        .unwrap();
        assert_eq!(iv, (0x1000, 0x1800));
        // A pair straddling the wrap keeps its true length.
        let mut prev = 0xffff_f000;
        let iv = interval(0xffff_ff00, 0x0000_0100, mask, &mut prev).unwrap();
        assert_eq!(iv.1 - iv.0, 0x200);
        assert_eq!(iv.0, 0xffff_ff00);
        // The next reading after the wrap lands in the next epoch.
        let iv2 = interval(0x200, 0x300, mask, &mut prev).unwrap();
        assert_eq!(iv2.0, 0x1_0000_0200);
        assert!(iv2.0 > iv.1);
    }

    #[test]
    fn interval_full_width_counter() {
        let mut prev = 0;
        assert_eq!(interval(100, 250, u64::MAX, &mut prev), Some((100, 250)));
    }

    #[test]
    fn union_counts_overlap_once() {
        let mut cu = 0;
        let mut v = vec![(10, 20), (15, 30), (40, 50)];
        assert_eq!(union_add(&mut cu, &mut v), 30);
        assert_eq!(cu, 50);
        // Out of order and partially inside the covered prefix.
        let mut v = vec![(60, 70), (45, 55)];
        assert_eq!(union_add(&mut cu, &mut v), 5 + 10);
        // Fully covered adds nothing.
        let mut v = vec![(20, 30)];
        assert_eq!(union_add(&mut cu, &mut v), 0);
        // Nested intervals.
        let mut cu = 0;
        let mut v = vec![(0, 100), (10, 20), (30, 40)];
        assert_eq!(union_add(&mut cu, &mut v), 100);
    }

    #[test]
    fn ring_fills_and_drains() {
        let mut r = Ring::default();
        for i in 0..SLOTS as u64 {
            assert_eq!(r.alloc(), Some(i));
        }
        assert_eq!(r.alloc(), None);
        r.tail += 3;
        assert_eq!(r.in_flight(), SLOTS - 3);
        assert_eq!(r.alloc(), Some(SLOTS as u64));
        assert_eq!(SLOTS as u64 % SLOTS as u64, 0);
    }

    #[test]
    fn bracket_first_last_single() {
        assert_eq!(bracket(&[1, 2], true, true, 0, 9), vec![0, 1, 2, 9]);
        assert_eq!(bracket(&[1, 2], true, false, 0, 9), vec![0, 1, 2]);
        assert_eq!(bracket(&[1, 2], false, true, 0, 9), vec![1, 2, 9]);
        assert_eq!(bracket::<i32>(&[], true, true, 0, 9), vec![0, 9]);
    }

    #[test]
    fn line_format() {
        let l = format_line("guest", 123, 456, 7, 1, 999, 2, 40);
        assert_eq!(l, "v1 guest 123 456 7 1 999 2 40\n");
        let f: Vec<&str> = l.split_whitespace().collect();
        assert_eq!(f.len(), 9);
    }

    #[test]
    fn submit_wrap_builds_bracketed_arrays() {
        let a = [1usize as VkCommandBuffer, 2usize as VkCommandBuffer];
        let c = [3usize as VkCommandBuffer];
        let mk = |p: *const VkCommandBuffer, n: u32| VkSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            pNext: std::ptr::null(),
            waitSemaphoreCount: 0,
            pWaitSemaphores: std::ptr::null(),
            pWaitDstStageMask: std::ptr::null(),
            commandBufferCount: n,
            pCommandBuffers: p,
            signalSemaphoreCount: 0,
            pSignalSemaphores: std::ptr::null(),
        };
        let src = [
            mk(a.as_ptr(), 2),
            mk(std::ptr::null(), 0),
            mk(c.as_ptr(), 1),
        ];
        let (out, _keep) = unsafe {
            rebuild1(
                &src,
                100usize as VkCommandBuffer,
                200usize as VkCommandBuffer,
            )
        };
        let got = |i: usize| unsafe {
            std::slice::from_raw_parts(out[i].pCommandBuffers, out[i].commandBufferCount as usize)
                .iter()
                .map(|&p| p as usize)
                .collect::<Vec<_>>()
        };
        assert_eq!(got(0), vec![100, 1, 2]);
        assert_eq!(out[1].commandBufferCount, 0);
        assert_eq!(got(2), vec![3, 200]);
    }
}
