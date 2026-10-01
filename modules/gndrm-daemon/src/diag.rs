//! Cross-thread diagnostics for the scan-out request/response path.
//!
//! WHY this module exists: the shim answers a scan-out request on a 250 ms
//! budget, and when the daemon misses it the panel stops updating with nothing
//! on the daemon side that says which thread or lock was responsible — the
//! failure was diagnosed twice by host-side reasoning and was wrong. Every
//! field here exists so that one log line, emitted *while the request is still
//! stuck*, names the blocker instead of a hypothesis:
//!
//! - the request's own step timing (channel read / pool-lock wait / gralloc
//!   work / reply write), so "slow" points at one stage;
//! - the holder of every mutex on the path ([`TrackedMutex`]), so "blocked on
//!   the pool lock" names the thread that holds it (serve loop, commit worker,
//!   warm worker, acceptor, presenter callback);
//! - what each of those threads is doing at that instant;
//! - whether the reply reached the socket at all.
//!
//! The registry is atomics only: no mutex is taken to update it and none is
//! taken to read it, so sampling it from a watchdog thread can never be the
//! thing a request waits on — the diagnostic must not perturb the timing it
//! measures. Rate limiting is a per-request epoch counter, so a stalled request
//! is dumped once, not once per watchdog tick.

use core::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Instant;

/// The shim's reply budget (`SO_RCVTIMEO`), the deadline a request must be
/// answered inside.
pub const SHIM_BUDGET_MS: u64 = 250;

/// Watchdog threshold: below the shim's budget so the snapshot is taken while
/// the request is still in flight, with the blocker still present, rather than
/// after the shim has given up and dropped the channel.
pub const STALL_WARN_MS: u64 = 120;

/// Watchdog sampling period. Small enough to catch a stall with the holder
/// still there, large enough that the `sleep` cannot meaningfully perturb a
/// request.
pub const STALL_TICK_MS: u64 = 15;

/// Number of threads whose state the snapshot reports.
pub const N_ROLES: usize = 5;

/// Number of tracked mutexes.
pub const N_LOCKS: usize = 1;

/// The scan-out buffer pool mutex (see `asc::ScanoutPool`): the only mutex on
/// the request/response path a request can block on, and therefore the one
/// whose holder the snapshot has to name.
pub const LOCK_SCANOUT_POOL: usize = 0;

/// A thread that participates in the scan-out path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Role {
    /// The RPC serve loop (also owns presenter teardown).
    Serve = 0,
    /// The commit worker that publishes and waits for the presenter latch.
    Commit = 1,
    /// The warm-spare worker that keeps a pre-allocated scan-out buffer ready.
    Warm = 2,
    /// The scan-out channel thread (accept loop + request/response).
    Scanout = 3,
    /// The SurfaceFlinger `setOnComplete` callback, which fires the latch.
    PresenterCallback = 4,
}

impl Role {
    /// Every role, in snapshot order.
    pub const ALL: [Role; N_ROLES] = [
        Role::Serve,
        Role::Commit,
        Role::Warm,
        Role::Scanout,
        Role::PresenterCallback,
    ];

    /// Index into the role table.
    #[must_use]
    pub fn index(self) -> usize {
        self as usize
    }

    /// Short label used in the snapshot.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Role::Serve => "serve",
            Role::Commit => "commit",
            Role::Warm => "warm",
            Role::Scanout => "scanout",
            Role::PresenterCallback => "presenter-cb",
        }
    }

    fn from_index(i: usize) -> Role {
        match i {
            1 => Role::Commit,
            2 => Role::Warm,
            3 => Role::Scanout,
            4 => Role::PresenterCallback,
            _ => Role::Serve,
        }
    }
}

/// What a role is doing at the instant the snapshot is taken, or the step a
/// request has reached.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum State {
    Idle = 0,
    /// Any role blocked acquiring a [`TrackedMutex`] (the pool lock).
    WaitPoolLock,
    /// Serve loop parked in `poll` for an RPC frame or flip event.
    ServeWaitingRpc,
    /// Serve loop dispatching one ioctl frame.
    ServeDispatch,
    /// Commit worker parked on its job channel.
    CommitWaitingJob,
    /// Commit worker inside `presenter.publish` (the SurfaceFlinger path).
    CommitPublishing,
    /// Commit worker waiting (bounded) for the presenter's latch.
    CommitWaitingLatch,
    /// Commit worker queueing the flip event / releasing in-flight state.
    CommitFlushing,
    /// Warm worker parked on its doorbell.
    WarmWaiting,
    /// Warm worker in `AHardwareBuffer_allocate` for the spare.
    WarmAllocating,
    /// Scan-out acceptor parked in `poll` for a connection.
    ScanoutAccepting,
    /// Scan-out channel parked in `recvmsg` for the next request.
    ScanoutWaitingRequest,
    /// Scan-out request holding the pool lock (map work only).
    ScanoutLocked,
    /// Scan-out request in `AHardwareBuffer_allocate` / native-handle work.
    ScanoutAllocating,
    /// Scan-out request in `sendmsg` writing the reply.
    ScanoutSending,
    /// Presenter completion callback running.
    PresenterCallback,
}

impl State {
    /// Short label used in the snapshot.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::WaitPoolLock => "wait-pool-lock",
            State::ServeWaitingRpc => "wait-rpc",
            State::ServeDispatch => "dispatch",
            State::CommitWaitingJob => "wait-job",
            State::CommitPublishing => "publishing",
            State::CommitWaitingLatch => "wait-latch",
            State::CommitFlushing => "flush-event",
            State::WarmWaiting => "wait-doorbell",
            State::WarmAllocating => "allocating-spare",
            State::ScanoutAccepting => "accepting",
            State::ScanoutWaitingRequest => "wait-request",
            State::ScanoutLocked => "pool-locked",
            State::ScanoutAllocating => "allocating",
            State::ScanoutSending => "sending",
            State::PresenterCallback => "callback",
        }
    }

    fn from_u32(v: u32) -> State {
        match v {
            1 => State::WaitPoolLock,
            2 => State::ServeWaitingRpc,
            3 => State::ServeDispatch,
            4 => State::CommitWaitingJob,
            5 => State::CommitPublishing,
            6 => State::CommitWaitingLatch,
            7 => State::CommitFlushing,
            8 => State::WarmWaiting,
            9 => State::WarmAllocating,
            10 => State::ScanoutAccepting,
            11 => State::ScanoutWaitingRequest,
            12 => State::ScanoutLocked,
            13 => State::ScanoutAllocating,
            14 => State::ScanoutSending,
            15 => State::PresenterCallback,
            _ => State::Idle,
        }
    }
}

/// A stage of the request/response path, whose elapsed time is recorded so the
/// snapshot says *which* step consumed the budget.
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Stage {
    /// Time parked in `recvmsg` before the request arrived (includes idle time
    /// when the channel was simply quiet — read it with the step, not alone).
    Read = 0,
    /// Time waiting to acquire the pool lock.
    Lock = 1,
    /// Time in the gralloc allocation / native-handle work.
    Alloc = 2,
    /// Time in `sendmsg` writing the reply.
    Reply = 3,
}

const N_STAGES: usize = 4;

/// Outcome recorded for the request epoch that just ended — the "was a reply
/// written at all" the investigation needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Reply {
    /// No reply attempt completed for this epoch.
    None = 0,
    /// The reply frame was handed to the socket.
    Written = 1,
    /// The reply could not be written (peer gone, `EPIPE`).
    Failed = 2,
}

impl Reply {
    #[must_use]
    fn label(self) -> &'static str {
        match self {
            Reply::None => "none",
            Reply::Written => "written",
            Reply::Failed => "failed",
        }
    }

    fn from_u32(v: u32) -> Reply {
        match v {
            1 => Reply::Written,
            2 => Reply::Failed,
            _ => Reply::None,
        }
    }
}

/// One role's published state.
struct RoleSlot {
    state: AtomicU32,
    since_ms: AtomicU64,
}

impl RoleSlot {
    const fn new() -> Self {
        Self {
            state: AtomicU32::new(State::Idle as u32),
            since_ms: AtomicU64::new(0),
        }
    }
}

/// One tracked mutex: who holds it, since when, how many are waiting, and how
/// many times it has been taken (so "never contended" is distinguishable from
/// "contended and slow").
struct LockSlot {
    name: &'static str,
    /// [`Role`] index of the last/current holder, `-1` when free.
    owner: AtomicI32,
    since_ms: AtomicU64,
    waiters: AtomicU32,
    acquisitions: AtomicU64,
}

impl LockSlot {
    const fn new(name: &'static str) -> Self {
        Self {
            name,
            owner: AtomicI32::new(-1),
            since_ms: AtomicU64::new(0),
            waiters: AtomicU32::new(0),
            acquisitions: AtomicU64::new(0),
        }
    }
}

/// The scan-out request epoch plus request/response counters.
struct ReqSlot {
    active: AtomicBool,
    /// Bumped on every request; the watchdog dumps once per epoch.
    epoch: AtomicU64,
    /// Epoch last dumped by the watchdog (0 = none yet).
    dumped: AtomicU64,
    seen: AtomicU64,
    replies: AtomicU64,
    slow: AtomicU64,
    w: AtomicU32,
    h: AtomicU32,
    fmt: AtomicU32,
    start_ms: AtomicU64,
    step: AtomicU32,
    step_since_ms: AtomicU64,
    prev_ms: AtomicU64,
    last_reply: AtomicU32,
    stages: [AtomicU64; N_STAGES],
}

impl ReqSlot {
    const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            dumped: AtomicU64::new(0),
            seen: AtomicU64::new(0),
            replies: AtomicU64::new(0),
            slow: AtomicU64::new(0),
            w: AtomicU32::new(0),
            h: AtomicU32::new(0),
            fmt: AtomicU32::new(0),
            start_ms: AtomicU64::new(0),
            step: AtomicU32::new(State::Idle as u32),
            step_since_ms: AtomicU64::new(0),
            prev_ms: AtomicU64::new(0),
            last_reply: AtomicU32::new(Reply::None as u32),
            stages: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }
}

/// The shared diagnostic registry. One instance is used process-wide by the
/// daemon ([`diag`]); tests construct their own so assertions are not shared
/// with a parallel test's device threads.
pub struct Diag {
    origin: OnceLock<Instant>,
    roles: [RoleSlot; N_ROLES],
    locks: [LockSlot; N_LOCKS],
    req: ReqSlot,
    stop: AtomicBool,
}

impl Diag {
    /// A fresh registry with every role idle and every lock free.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            origin: OnceLock::new(),
            roles: [
                RoleSlot::new(),
                RoleSlot::new(),
                RoleSlot::new(),
                RoleSlot::new(),
                RoleSlot::new(),
            ],
            locks: [LockSlot::new("scanout-pool")],
            req: ReqSlot::new(),
            stop: AtomicBool::new(false),
        }
    }

    fn now_ms(&self) -> u64 {
        let origin = self.origin.get_or_init(Instant::now);
        origin.elapsed().as_millis() as u64
    }

    /// Publish what `role` is doing right now.
    pub fn set_role(&self, role: Role, state: State) {
        let slot = &self.roles[role.index()];
        slot.state.store(state as u32, Ordering::Relaxed);
        slot.since_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    /// What `role` is doing and how long it has been doing it (`ms`).
    #[must_use]
    pub fn role_state(&self, role: Role) -> (State, u64) {
        let slot = &self.roles[role.index()];
        let state = State::from_u32(slot.state.load(Ordering::Relaxed));
        let since = slot.since_ms.load(Ordering::Relaxed);
        (state, self.now_ms().saturating_sub(since))
    }

    /// Open a request epoch: records the geometry, starts the clock, and bumps
    /// the epoch so the watchdog can dump this request exactly once.
    pub fn begin_request(&self, width: u32, height: u32, format: u32) {
        let now = self.now_ms();
        let r = &self.req;
        r.epoch.fetch_add(1, Ordering::Relaxed);
        r.seen.fetch_add(1, Ordering::Relaxed);
        r.w.store(width, Ordering::Relaxed);
        r.h.store(height, Ordering::Relaxed);
        r.fmt.store(format, Ordering::Relaxed);
        r.start_ms.store(now, Ordering::Relaxed);
        r.step.store(State::Idle as u32, Ordering::Relaxed);
        r.step_since_ms.store(now, Ordering::Relaxed);
        r.last_reply.store(Reply::None as u32, Ordering::Relaxed);
        for stage in &r.stages {
            stage.store(0, Ordering::Relaxed);
        }
        r.active.store(true, Ordering::Relaxed);
    }

    /// Record the step the request has reached (channel read, lock wait,
    /// gralloc work, reply write).
    pub fn step(&self, step: State) {
        self.req.step.store(step as u32, Ordering::Relaxed);
        self.req
            .step_since_ms
            .store(self.now_ms(), Ordering::Relaxed);
    }

    /// Record how long one stage of the path took.
    pub fn note_stage(&self, stage: Stage, ms: u64) {
        self.req.stages[stage as usize].store(ms, Ordering::Relaxed);
    }

    /// Close the epoch: stores the total time and the reply outcome, and
    /// returns the total. The geometry and last step are kept so a snapshot
    /// taken immediately afterwards still describes the request.
    pub fn end_request(&self, reply: Reply) -> u64 {
        let total = self.request_elapsed_ms();
        self.req.prev_ms.store(total, Ordering::Relaxed);
        self.req.last_reply.store(reply as u32, Ordering::Relaxed);
        if reply == Reply::Written {
            self.req.replies.fetch_add(1, Ordering::Relaxed);
        }
        self.req.active.store(false, Ordering::Relaxed);
        total
    }

    /// Is a request being handled right now?
    #[must_use]
    pub fn request_active(&self) -> bool {
        self.req.active.load(Ordering::Relaxed)
    }

    /// Time the in-flight request has taken so far (0 when no request is open).
    #[must_use]
    pub fn request_elapsed_ms(&self) -> u64 {
        if !self.req.active.load(Ordering::Relaxed) {
            return self.req.prev_ms.load(Ordering::Relaxed);
        }
        self.now_ms()
            .saturating_sub(self.req.start_ms.load(Ordering::Relaxed))
    }

    /// Requests received on the scan-out channel so far.
    #[must_use]
    pub fn requests_seen(&self) -> u64 {
        self.req.seen.load(Ordering::Relaxed)
    }

    /// Replies successfully written to the scan-out channel so far.
    #[must_use]
    pub fn replies_sent(&self) -> u64 {
        self.req.replies.load(Ordering::Relaxed)
    }

    /// The watchdog's stall hook: `Some(snapshot)` once per request epoch, as
    /// soon as that request has been in flight at least `threshold_ms`. Cheap
    /// when nothing is wrong (two atomic loads).
    pub fn maybe_stall_dump(&self, threshold_ms: u64) -> Option<String> {
        if !self.req.active.load(Ordering::Relaxed) {
            return None;
        }
        let epoch = self.req.epoch.load(Ordering::Relaxed);
        if self.req.dumped.load(Ordering::Relaxed) == epoch {
            return None;
        }
        if self.request_elapsed_ms() < threshold_ms {
            return None;
        }
        self.req.dumped.store(epoch, Ordering::Relaxed);
        Some(self.snapshot("stalled"))
    }

    /// Rate limit for the "reply was slow" line: the first few are always
    /// logged, then sparsely, so a pathologically slow path cannot flood the
    /// log (and the counters remain in every snapshot).
    #[must_use]
    pub fn slow_report_allowed(&self) -> bool {
        let n = self.req.slow.fetch_add(1, Ordering::Relaxed);
        n < 24 || n.is_multiple_of(32)
    }

    /// Rate limit for the per-request receipt line: the first 32 requests are
    /// all logged, then every 64th, so the daemon log holds a receipt for every
    /// request the shim sent without flooding a busily allocating client. A
    /// shim timeout with no matching receipt is the proof the request never
    /// reached the scan-out thread (a channel/accept fault, not a slow path).
    #[must_use]
    pub fn receipt_allowed(&self) -> bool {
        let n = self.req.seen.load(Ordering::Relaxed);
        n <= 32 || n.is_multiple_of(64)
    }

    /// Ask a running watchdog to exit after its current tick.
    pub fn watchdog_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Has [`Self::watchdog_stop`] been called?
    #[must_use]
    pub fn watchdog_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// One-line, greppable description of the whole path at this instant.
    #[must_use]
    pub fn snapshot(&self, trigger: &str) -> String {
        let now = self.now_ms();
        let r = &self.req;
        let mut s = String::with_capacity(320);
        let stage = |st: Stage| r.stages[st as usize].load(Ordering::Relaxed);

        let _ = write!(s, "scan-out diag[{trigger}] req=");
        let w = r.w.load(Ordering::Relaxed);
        let h = r.h.load(Ordering::Relaxed);
        let fmt = r.fmt.load(Ordering::Relaxed);
        let step = State::from_u32(r.step.load(Ordering::Relaxed));
        if r.active.load(Ordering::Relaxed) {
            let elapsed = now.saturating_sub(r.start_ms.load(Ordering::Relaxed));
            let step_ms = now.saturating_sub(r.step_since_ms.load(Ordering::Relaxed));
            let _ = write!(
                s,
                "{w}x{h} fmt={fmt:#x} active elapsed={elapsed}ms step={} for={step_ms}ms",
                step.label()
            );
        } else {
            let prev = r.prev_ms.load(Ordering::Relaxed);
            let _ = write!(
                s,
                "{w}x{h} fmt={fmt:#x} idle last={prev}ms last_step={}",
                step.label()
            );
        }
        let _ = write!(
            s,
            " seen={} replies={} reply={} stages=[recv={}ms lock={}ms alloc={}ms write={}ms]",
            r.seen.load(Ordering::Relaxed),
            r.replies.load(Ordering::Relaxed),
            Reply::from_u32(r.last_reply.load(Ordering::Relaxed)).label(),
            stage(Stage::Read),
            stage(Stage::Lock),
            stage(Stage::Alloc),
            stage(Stage::Reply),
        );

        s.push_str(" locks=[");
        for (i, lock) in self.locks.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            let owner = lock.owner.load(Ordering::Relaxed);
            let waiters = lock.waiters.load(Ordering::Relaxed);
            let acq = lock.acquisitions.load(Ordering::Relaxed);
            if owner < 0 {
                let _ = write!(s, "{} held=free waiters={waiters} acq={acq}", lock.name);
            } else {
                let held = now.saturating_sub(lock.since_ms.load(Ordering::Relaxed));
                let _ = write!(
                    s,
                    "{} held_by={} for={held}ms waiters={waiters} acq={acq}",
                    lock.name,
                    Role::from_index(owner as usize).label()
                );
            }
        }
        s.push_str("] roles=[");
        for (i, role) in Role::ALL.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            let slot = &self.roles[role.index()];
            let state = State::from_u32(slot.state.load(Ordering::Relaxed));
            let ms = now.saturating_sub(slot.since_ms.load(Ordering::Relaxed));
            let _ = write!(s, "{}={}({ms}ms)", role.label(), state.label());
        }
        s.push(']');
        s
    }

    /// Called by [`TrackedMutex::lock`] before blocking: advertise the pending
    /// wait and mark `role` as blocked on the lock, saving the state to restore.
    fn lock_enter(&self, id: usize, role: Role) -> State {
        self.locks[id].waiters.fetch_add(1, Ordering::Relaxed);
        let prev = State::from_u32(self.roles[role.index()].state.load(Ordering::Relaxed));
        self.set_role(role, State::WaitPoolLock);
        prev
    }

    /// Called by [`TrackedMutex::lock`] after acquiring: publish the new holder,
    /// restore the role's real state, and return the time spent waiting.
    fn lock_acquired(&self, id: usize, role: Role, t0: u64, prev: State) -> u64 {
        let now = self.now_ms();
        let lock = &self.locks[id];
        // Publish the new owner before dropping the waiter count so a snapshot
        // never sees "free" with a waiter present (a handoff in progress).
        lock.owner.store(role.index() as i32, Ordering::Relaxed);
        lock.since_ms.store(now, Ordering::Relaxed);
        lock.acquisitions.fetch_add(1, Ordering::Relaxed);
        lock.waiters.fetch_sub(1, Ordering::Relaxed);
        self.set_role(role, prev);
        now.saturating_sub(t0)
    }

    /// Called when a [`TrackedGuard`] drops. If a waiter is already present the
    /// lock is being handed straight over, so the holder is left in place for
    /// the next [`Self::lock_acquired`] to overwrite.
    fn lock_released(&self, id: usize) {
        let lock = &self.locks[id];
        if lock.waiters.load(Ordering::Relaxed) == 0 {
            lock.owner.store(-1, Ordering::Relaxed);
            lock.since_ms.store(0, Ordering::Relaxed);
        }
    }
}

impl Default for Diag {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide registry the daemon threads publish into.
static GLOBAL: Diag = Diag::new();

/// The process-wide diagnostic registry.
#[must_use]
pub fn diag() -> &'static Diag {
    &GLOBAL
}

/// A `Mutex` that publishes its holder (by [`Role`]) and its waiters into a
/// [`Diag`], so a snapshot taken while a request is stalled can name the thread
/// that is blocking it.
///
/// Used for the scan-out buffer pool, the one mutex on the request/response
/// path a request can block on.
pub struct TrackedMutex<T> {
    diag: &'static Diag,
    id: usize,
    inner: Mutex<T>,
}

impl<T> TrackedMutex<T> {
    /// Wrap `value`, publishing under lock id `id` of `diag`.
    #[must_use]
    pub fn new(diag: &'static Diag, id: usize, value: T) -> Self {
        Self {
            diag,
            id,
            inner: Mutex::new(value),
        }
    }

    /// Acquire the lock as `role`. The guard records how long the acquisition
    /// took (see [`TrackedGuard::waited_ms`]).
    ///
    /// A poisoned lock is not treated as fatal: the presenter runs under
    /// `panic = "abort"`-style bring-up paths where a poisoned table must still
    /// not wedge the frame loop (matches the previous `PoisonError::into_inner`
    /// use at every call site).
    pub fn lock(&self, role: Role) -> TrackedGuard<'_, T> {
        let t0 = self.diag.now_ms();
        let prev = self.diag.lock_enter(self.id, role);
        let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let waited_ms = self.diag.lock_acquired(self.id, role, t0, prev);
        TrackedGuard {
            guard,
            diag: self.diag,
            id: self.id,
            waited_ms,
        }
    }
}

/// Guard returned by [`TrackedMutex::lock`]; releases the tracker on drop.
pub struct TrackedGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    diag: &'static Diag,
    id: usize,
    waited_ms: u64,
}

impl<T> TrackedGuard<'_, T> {
    /// How long this acquisition waited for the lock (`0` when uncontended).
    #[must_use]
    pub fn waited_ms(&self) -> u64 {
        self.waited_ms
    }
}

impl<T> core::ops::Deref for TrackedGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> core::ops::DerefMut for TrackedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for TrackedGuard<'_, T> {
    fn drop(&mut self) {
        self.diag.lock_released(self.id);
    }
}
