//! REMOVABLE DIAGNOSTIC (round v): a chaining `SIGSEGV`/`SIGBUS` dump of the
//! fault register file — **shipped DISABLED** (see below).
//!
//! STATUS / WHY IT IS OFF. It was implemented, validated and exercised on
//! device, and it does capture the fault register file, `ucontext` and the
//! `/proc/self/maps` row exactly as designed. But it is **not safe here**: with
//! the handler installed, 4 of 5 measured runs never reached the swapchain —
//! the guest (proton/wine under FEX) fell into an `ntdll.dll` `virtual_unwind`
//! exception loop (`trace:seh:dispatch_exception … EXCEPTION_INVALID_DISPOSITION`)
//! and `x11 present` stayed 0. With the identical build minus the
//! `segv::install()` call, 2 of 2 runs reached the swapchain and reproduced the
//! storm. So intercepting wine's arm64 SEH signal path — even chaining by
//! calling the saved `sa_sigaction` verbatim — perturbs wine's SEH and changes
//! the app's behaviour, which this pass must not do. The register file it would
//! have provided is already available without touching the process: the guest
//! env carries `WINEDEBUG=+err,+fixme,+seh,+loaddll,+warn`, and wine's own
//! `handle_syscall_fault` prints `x0..x28, fp, lr, sp` and the faulting address
//! on every fault. `install()` is therefore gated behind the `segv_dump` cargo
//! feature, default OFF, so a normal build installs no signal handler at all.
//!
//! (Kept in-tree, feature-gated, as the documented, removable instrument and as
//! the evidence for the A/B above.)
//!
//! ORIGINAL DESIGN — a shim-side `SIGSEGV`/`SIGBUS` handler recording the
//! `ucontext` register file (x0–x30, sp, pc, pstate), the faulting address, the
//! faulting instruction word, and a `/proc/self/maps` cross-check of the fault
//! PC; async-signal-safe, rate-limited, and chaining to the previous handler.
//! Signal handlers run LIFO: this ICD is loaded after wine/ntdll installed its
//! arm64 SEH handler, so this would run first and then invoke the saved handler.
//!
//! Constraints honoured here:
//!   - async-signal-safe only: `write(2)`, `open`/`read`/`close`, no
//!     allocation, no locks, no Rust formatting that can allocate;
//!   - bounded output, *per fault site*: at most [`PER_PC`] dumps for each
//!     distinct fault PC (up to [`NPCS`] distinct PCs) and at most
//!     [`MAX_TOTAL`] lines overall — a global-only cap is not enough, because
//!     an unrelated early fault burst (here: FEX SIGBUS at storm onset) can
//!     exhaust it before the site of interest is reached;
//!   - reentrancy-guarded: a fault inside this handler chains immediately
//!     instead of recursing;
//!   - only logging, no behaviour change: the previous disposition is always
//!     invoked (or faithfully replicated for `SIG_DFL`).
//!
//! Register extraction is aarch64-only (the bionic `mcontext_t` layout); on a
//! non-aarch64 build this module installs nothing, so host clippy/tests stay
//! clean and the aarch64 payload carries the diagnostic.
//!
//! To remove: delete this file, the `pub mod segv;` line and the single
//! `segv::install()` call in `lib.rs::init_once`.

/// Install the chaining dumps. Idempotent; safe to call from `init_once`.
pub fn install() {
    #[cfg(target_arch = "aarch64")]
    imp::install();
    #[cfg(not(target_arch = "aarch64"))]
    crate::logf!("segv: register dump is aarch64-only; inert on this build");
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::ffi::c_void;

    /// Dumps per distinct fault PC before that site goes quiet.
    const PER_PC: u32 = 8;
    /// Distinct fault PCs tracked (each gets up to [`PER_PC`] dumps).
    const NPCS: usize = 48;
    /// Hard ceiling on SIGSEGV dump lines per process, whatever the mix of sites.
    const MAX_TOTAL: u32 = 400;
    /// SIGBUS is deliberately kept almost silent: the guest (FEX) uses SIGBUS as
    /// a routine mechanism and emits a large, varied burst at startup — logging
    /// it in volume would add `write(2)` work inside that signal path on every
    /// occurrence and risks perturbing the guest. A handful of samples is enough
    /// to show the handler sees and chains SIGBUS too.
    const MAX_BUS: u32 = 4;

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static IN_HANDLER: AtomicBool = AtomicBool::new(false);
    static TOTAL: AtomicU32 = AtomicU32::new(0);
    static NSLOT: AtomicU32 = AtomicU32::new(0);
    static BUS_DUMPS: AtomicU32 = AtomicU32::new(0);
    static PCS: [AtomicU64; NPCS] = [const { AtomicU64::new(0) }; NPCS];
    static CNT: [AtomicU32; NPCS] = [const { AtomicU32::new(0) }; NPCS];

    // Verbatim copies of the dispositions we replaced (filled by `install`).
    // `MaybeUninit` so they can be zero-initialised in const context; cast to
    // `libc::sigaction` at use (same layout).
    static mut PREV_SEGV: core::mem::MaybeUninit<libc::sigaction> =
        core::mem::MaybeUninit::uninit();
    static mut PREV_BUS: core::mem::MaybeUninit<libc::sigaction> = core::mem::MaybeUninit::uninit();

    const HEX: &[u8; 16] = b"0123456789abcdef";

    pub fn install() {
        // Escape hatch: GN_VKSHIM_SEGV=0 keeps the handler out entirely.
        if let Some(v) = std::env::var_os("GN_VKSHIM_SEGV") {
            if v == "0" || v.is_empty() {
                crate::logf!("segv: dump disabled by GN_VKSHIM_SEGV");
                return;
            }
        }
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        unsafe {
            let mut act: libc::sigaction = core::mem::zeroed();
            act.sa_sigaction = dump_handler as *const () as libc::sighandler_t;
            act.sa_flags = libc::SA_SIGINFO;
            libc::sigemptyset(&mut act.sa_mask);
            libc::sigaction(
                libc::SIGSEGV,
                &act,
                core::ptr::addr_of_mut!(PREV_SEGV).cast::<libc::sigaction>(),
            );
            libc::sigaction(
                libc::SIGBUS,
                &act,
                core::ptr::addr_of_mut!(PREV_BUS).cast::<libc::sigaction>(),
            );
        }
        crate::logf!("segv: chaining dump installed (segv per-pc {PER_PC}, {NPCS} pcs, max {MAX_TOTAL}; sigbus sampled {MAX_BUS})");
    }

    /// Bounded, per-PC rate limit. Async-signal-safe (atomics only).
    fn should_log(pc: u64) -> bool {
        if TOTAL.fetch_add(1, Ordering::Relaxed) >= MAX_TOTAL {
            return false;
        }
        let n = (NSLOT.load(Ordering::Relaxed) as usize).min(NPCS);
        for i in 0..n {
            if PCS[i].load(Ordering::Relaxed) == pc {
                return CNT[i].fetch_add(1, Ordering::Relaxed) < PER_PC;
            }
        }
        // New PC: claim a slot. Concurrent faults may duplicate a slot; that is
        // harmless and bounded by NPCS.
        let idx = NSLOT.fetch_add(1, Ordering::AcqRel) as usize;
        if idx < NPCS {
            PCS[idx].store(pc, Ordering::Relaxed);
            CNT[idx].store(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    extern "C" fn dump_handler(sig: libc::c_int, info: *mut libc::siginfo_t, uctx: *mut c_void) {
        // Reentrancy guard: if we faulted while already dumping, skip straight
        // to the chain so we can never recurse.
        if !IN_HANDLER.swap(true, Ordering::Relaxed) {
            if !uctx.is_null() {
                let pc = unsafe { (*(uctx as *const libc::ucontext_t)).uc_mcontext.pc };
                let log = if sig == libc::SIGBUS {
                    BUS_DUMPS.fetch_add(1, Ordering::Relaxed) < MAX_BUS
                } else {
                    should_log(pc)
                };
                if log {
                    unsafe { dump(sig, uctx) };
                }
            }
            IN_HANDLER.store(false, Ordering::Relaxed);
        }
        unsafe { chain(sig, info, uctx) };
    }

    /// Format and emit one fault. Bounded buffer, single `write(2)`.
    unsafe fn dump(sig: libc::c_int, uctx: *mut c_void) {
        let mut buf = [0u8; 2048];
        let mut p = 0usize;
        let uc = uctx as *const libc::ucontext_t;
        let mc = &(*uc).uc_mcontext;
        let pc = mc.pc;
        put(&mut buf, &mut p, b"gn-vkshim: SIG ");
        put_dec(&mut buf, &mut p, sig as u32);
        put(&mut buf, &mut p, b" pc=");
        put_hex16(&mut buf, &mut p, pc);
        put(&mut buf, &mut p, b" addr=");
        put_hex16(&mut buf, &mut p, mc.fault_address);
        put(&mut buf, &mut p, b" sp=");
        put_hex16(&mut buf, &mut p, mc.sp);
        put(&mut buf, &mut p, b" pstate=");
        put_hex16(&mut buf, &mut p, mc.pstate);
        // The faulting instruction word (executable pages are readable); this
        // is the datum that names which register+offset the driver dereferenced.
        let insn = core::ptr::read_volatile(pc as *const u32);
        put(&mut buf, &mut p, b" insn=");
        put_hex32(&mut buf, &mut p, insn);
        put(&mut buf, &mut p, b"\n");
        // x0..x30, four per line.
        for i in 0..31usize {
            if i % 4 == 0 {
                put(&mut buf, &mut p, b"gn-vkshim:  ");
            }
            put(&mut buf, &mut p, b"x");
            put_dec(&mut buf, &mut p, i as u32);
            put(&mut buf, &mut p, b"=");
            put_hex16(&mut buf, &mut p, mc.regs[i]);
            if i % 4 == 3 || i == 30 {
                put(&mut buf, &mut p, b"\n");
            } else {
                put(&mut buf, &mut p, b" ");
            }
        }
        // /proc/self/maps cross-check: which mapping contains pc.
        put(&mut buf, &mut p, b"gn-vkshim:  map ");
        emit_map(&mut buf, &mut p, pc);
        put(&mut buf, &mut p, b"\n");
        libc::write(2, buf.as_ptr().cast::<c_void>(), p);
    }

    /// Faithfully hand control to the disposition we replaced.
    unsafe fn chain(sig: libc::c_int, info: *mut libc::siginfo_t, uctx: *mut c_void) {
        let prev: *const libc::sigaction = if sig == libc::SIGSEGV {
            core::ptr::addr_of!(PREV_SEGV).cast::<libc::sigaction>()
        } else if sig == libc::SIGBUS {
            core::ptr::addr_of!(PREV_BUS).cast::<libc::sigaction>()
        } else {
            return;
        };
        let flags = (*prev).sa_flags;
        let h = (*prev).sa_sigaction;
        if h == libc::SIG_DFL {
            // Replicate default: reset and re-raise (the kernel then terminates
            // the process exactly as it would have with no shim).
            let mut d: libc::sigaction = core::mem::zeroed();
            d.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(sig, &d, core::ptr::null_mut());
            libc::raise(sig);
            return;
        }
        if h == libc::SIG_IGN {
            return;
        }
        if flags & libc::SA_SIGINFO != 0 {
            let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut c_void) =
                core::mem::transmute(h);
            f(sig, info, uctx);
        } else {
            let f: extern "C" fn(libc::c_int) = core::mem::transmute(h);
            f(sig);
        }
    }

    // ---- async-signal-safe formatting + maps scan --------------------------

    fn put(buf: &mut [u8], p: &mut usize, s: &[u8]) {
        let n = s.len().min(buf.len().saturating_sub(*p));
        buf[*p..*p + n].copy_from_slice(&s[..n]);
        *p += n;
    }

    fn put_hex16(buf: &mut [u8], p: &mut usize, v: u64) {
        let mut tmp = [0u8; 16];
        for (i, t) in tmp.iter_mut().enumerate() {
            *t = HEX[((v >> ((15 - i) * 4)) & 0xf) as usize];
        }
        put(buf, p, &tmp);
    }

    fn put_hex32(buf: &mut [u8], p: &mut usize, v: u32) {
        let mut tmp = [0u8; 8];
        for (i, t) in tmp.iter_mut().enumerate() {
            *t = HEX[((v >> ((7 - i) * 4)) & 0xf) as usize];
        }
        put(buf, p, &tmp);
    }

    fn put_dec(buf: &mut [u8], p: &mut usize, mut v: u32) {
        if v == 0 {
            put(buf, p, b"0");
            return;
        }
        let mut tmp = [0u8; 10];
        let mut i = tmp.len();
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        put(buf, p, &tmp[i..]);
    }

    fn hexval(c: u8) -> Option<u64> {
        match c {
            b'0'..=b'9' => Some((c - b'0') as u64),
            b'a'..=b'f' => Some((c - b'a' + 10) as u64),
            b'A'..=b'F' => Some((c - b'A' + 10) as u64),
            _ => None,
        }
    }

    /// Parse "START-END" at the head of a maps line into (start, end).
    fn parse_range(line: &[u8]) -> Option<(u64, u64)> {
        let dash = line.iter().position(|&c| c == b'-')?;
        let mut start = 0u64;
        for &c in &line[..dash] {
            start = start.checked_mul(16)?.checked_add(hexval(c)?)?;
        }
        let mut end = 0u64;
        let mut i = dash + 1;
        while i < line.len() {
            match hexval(line[i]) {
                Some(v) => end = end.checked_mul(16)?.checked_add(v)?,
                None => break,
            }
            i += 1;
        }
        Some((start, end))
    }

    /// Incrementally read /proc/self/maps and append the mapping containing
    /// `pc`. Async-signal-safe (`open`/`read`/`close`); never allocates.
    unsafe fn emit_map(buf: &mut [u8], p: &mut usize, pc: u64) {
        let fd = libc::open(
            c"/proc/self/maps".as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        );
        if fd < 0 {
            put(buf, p, b"<open failed>");
            return;
        }
        let mut chunk = [0u8; 1024];
        let mut line = [0u8; 1024];
        let mut ll = 0usize;
        let mut found = false;
        let mut done = false;
        while !done {
            let n = libc::read(fd, chunk.as_mut_ptr().cast::<c_void>(), chunk.len());
            if n <= 0 {
                break;
            }
            for &c in &chunk[..n as usize] {
                if c == b'\n' {
                    if let Some((a, b)) = parse_range(&line[..ll]) {
                        if pc >= a && pc < b {
                            put(buf, p, &line[..ll.min(200)]);
                            found = true;
                            done = true;
                            break;
                        }
                        if pc < a {
                            // maps are address-ordered: nothing further matches.
                            done = true;
                            break;
                        }
                    }
                    ll = 0;
                } else if ll < line.len() - 1 {
                    line[ll] = c;
                    ll += 1;
                }
            }
        }
        libc::close(fd);
        if !found {
            put(buf, p, b"<not found in /proc/self/maps>");
        }
    }
}
