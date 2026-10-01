//! Tests for the scan-out path diagnostics (`gndrm_daemon::diag`): the
//! snapshot must name the request step, the lock holder and the role states,
//! and the watchdog must dump a stalled request exactly once.
//!
//! Each test builds its own leaked registry: `cargo test` runs these in
//! parallel and the real daemon's global registry is not shared state any test
//! should observe.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gndrm_daemon::diag::{
    Diag, Reply, Role, Stage, State, TrackedMutex, LOCK_SCANOUT_POOL, STALL_WARN_MS,
};

/// A registry that lives for the test, at `'static` so `TrackedMutex` can hold
/// it (the daemon passes its own process-wide instance).
fn local() -> &'static Diag {
    Box::leak(Box::new(Diag::new()))
}

#[test]
fn snapshot_names_role_lock_holder_and_request_step() {
    let d = local();
    let pool = TrackedMutex::new(d, LOCK_SCANOUT_POOL, 0u32);
    let guard = pool.lock(Role::Commit);
    // The commit worker publishes its own state (the lock tracker restores it
    // after acquisition).
    d.set_role(Role::Commit, State::CommitWaitingLatch);
    d.set_role(Role::Serve, State::ServeWaitingRpc);
    d.begin_request(1280, 960, 5);
    d.step(State::ScanoutAllocating);
    d.note_stage(Stage::Read, 2);
    d.note_stage(Stage::Lock, 3);

    let s = d.snapshot("test");
    assert!(s.contains("scan-out diag[test]"), "{s}");
    assert!(s.contains("req=1280x960 fmt=0x5"), "{s}");
    assert!(s.contains("active elapsed="), "{s}");
    assert!(s.contains("step=allocating"), "{s}");
    assert!(s.contains("recv=2ms lock=3ms"), "{s}");
    assert!(s.contains("scanout-pool held_by=commit"), "{s}");
    assert!(s.contains("commit=wait-latch"), "{s}");
    assert!(s.contains("serve=wait-rpc"), "{s}");
    assert!(s.contains("reply=none"), "{s}");
    assert!(s.contains("seen=1 replies=0"), "{s}");

    drop(guard);
    let after = d.snapshot("after");
    assert!(after.contains("scanout-pool held=free"), "{after}");
    assert!(after.contains("acq=1"), "{after}");
}

#[test]
fn stall_dump_fires_once_per_request_and_rearms() {
    let d = local();
    d.begin_request(1280, 960, 5);
    // Threshold 0: this request has already been in flight "long enough", so
    // the watchdog must dump it — and only once.
    let first = d.maybe_stall_dump(0).expect("first dump");
    assert!(first.contains("diag[stalled]"), "{first}");
    assert!(
        d.maybe_stall_dump(0).is_none(),
        "a stalled request is dumped once, not per tick"
    );

    d.end_request(Reply::Written);
    assert!(
        d.maybe_stall_dump(0).is_none(),
        "no request in flight → no dump"
    );

    // The next request re-arms the watchdog: the once-per-epoch guard must not
    // silence the rest of the session.
    d.begin_request(1280, 960, 5);
    assert!(
        d.maybe_stall_dump(0).is_some(),
        "a new request must be dumpable again"
    );
}

#[test]
fn stall_dump_respects_the_threshold() {
    let d = local();
    d.begin_request(1280, 960, 5);
    assert!(
        d.maybe_stall_dump(STALL_WARN_MS).is_none(),
        "a request just started is not a stall"
    );
    // And the skipped request can still be dumped later.
    assert!(d.maybe_stall_dump(0).is_some());
}

#[test]
fn end_request_records_total_and_reply_outcome() {
    let d = local();
    d.begin_request(1280, 960, 5);
    assert!(d.request_active());
    thread::sleep(Duration::from_millis(5));
    let total = d.end_request(Reply::Written);
    assert!(total >= 5, "elapsed {total} ms");
    assert!(!d.request_active());
    assert_eq!(d.requests_seen(), 1);
    assert_eq!(d.replies_sent(), 1);

    let s = d.snapshot("t");
    assert!(s.contains("idle last="), "{s}");
    assert!(s.contains("reply=written"), "{s}");

    // A reply that failed to reach the socket is not a reply sent.
    d.begin_request(64, 64, 5);
    d.end_request(Reply::Failed);
    assert_eq!(d.requests_seen(), 2);
    assert_eq!(d.replies_sent(), 1, "failed write must not count as sent");
    assert!(d.snapshot("t2").contains("reply=failed"));
}

#[test]
fn tracked_lock_publishes_holder_and_waiter() {
    let d = local();
    let pool = Arc::new(TrackedMutex::new(d, LOCK_SCANOUT_POOL, ()));
    let guard = pool.lock(Role::Commit);
    assert_eq!(guard.waited_ms(), 0, "first acquisition is uncontended");
    assert!(d.snapshot("held").contains("held_by=commit"));

    let done = Arc::new(AtomicBool::new(false));
    let p2 = Arc::clone(&pool);
    let dn = Arc::clone(&done);
    let waiter = thread::spawn(move || {
        // Blocks on the pool lock the main thread holds.
        let g = p2.lock(Role::Scanout);
        dn.store(true, Ordering::Relaxed);
        drop(g);
    });

    // While blocked, the snapshot must show the waiter AND the waiting role
    // itself as blocked on the lock.
    let mut seen = None;
    for _ in 0..2000 {
        let s = d.snapshot("waiting");
        if s.contains("waiters=1") {
            seen = Some(s);
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let waiting = seen.expect("the waiter never registered on the lock");
    assert!(waiting.contains("scanout=wait-pool-lock"), "{waiting}");
    assert!(waiting.contains("held_by=commit"), "{waiting}");

    drop(guard);
    waiter.join().expect("waiter thread");
    assert!(done.load(Ordering::Relaxed));

    let after = d.snapshot("after");
    assert!(after.contains("scanout-pool held=free"), "{after}");
    assert!(after.contains("acq=2"), "{after}");
    assert!(after.contains("scanout="), "{after}");
}

#[test]
fn snapshot_reports_no_request_before_the_first_one() {
    let d = local();
    let s = d.snapshot("idle");
    assert!(s.contains("req=0x0"), "{s}");
    assert!(s.contains("idle last=0ms"), "{s}");
    assert!(s.contains("scanout-pool held=free waiters=0 acq=0"), "{s}");
    assert!(s.contains("serve=idle"), "{s}");
    assert_eq!(d.requests_seen(), 0);
    assert_eq!(d.replies_sent(), 0);
}

#[test]
fn slow_report_is_rate_limited_but_never_fully_silent() {
    let d = local();
    for i in 0..24 {
        assert!(d.slow_report_allowed(), "slow report {i} must be logged");
    }
    let allowed = (0..128).filter(|_| d.slow_report_allowed()).count();
    assert!(allowed >= 1, "a persistent fault must still be reported");
    assert!(allowed < 128, "a burst must not flood the log");
}

#[test]
fn request_receipt_is_logged_at_first_then_sparsely() {
    let d = local();
    // The first 32 requests all get a receipt: the initial allocation burst is
    // exactly the window the display failure happens in.
    for i in 0..32 {
        d.begin_request(1280, 960, 5);
        assert!(d.receipt_allowed(), "receipt {i} must be logged");
    }
    // Later requests are logged sparsely, but never go fully silent.
    let allowed = (0..128)
        .filter(|_| {
            d.begin_request(1280, 960, 5);
            d.receipt_allowed()
        })
        .count();
    assert!(allowed >= 1, "a late request must still leave a receipt");
    assert!(allowed < 128, "a flood of receipts must be rate-limited");
}
