#!/usr/bin/env python3
"""Turn one instrumented logcat capture into the step-1 decision numbers.

Usage:  plan1-parse.py capture.txt [--settle 30]

Input: `adb logcat -v threadtime` text, unfiltered (lines are matched on
content, never on tag: the guest's stderr is re-emitted by the app through
Timber, so the tag depends on the build).

Output: per-layer frame rates, the acquire/starvation census, the present-wait
census, the present-mode census, and one verdict line per cause.

Marker literals below are copied from
  gamenative-drm @ performance-report
  icd_shim/src/proxy.rs, icd_shim/src/present.rs, icd_shim/src/lib.rs,
  modules/gndrm-shim/gndrm_shim.c
and cited with file:line in plan-1-instrumentation.md.
"""
import re
import sys

TS = re.compile(r"^(\d\d-\d\d\s+\d\d:\d\d:\d\d\.\d{3})")
SCALAR = re.compile(r"(?<![#\w])([A-Za-z_][A-Za-z0-9_]*)=([^\s\[\]]+)")
BRACKET = re.compile(r"(?<![#\w])([A-Za-z_][A-Za-z0-9_]*)=\[([^\]]*)\]")
KEYCOLON = re.compile(r"([A-Za-z_][A-Za-z0-9_<>.\-]*)[=:]([^\s\]]+)")

# VK_STRUCTURE_TYPE_PRESENT_ID_KHR (vulkan_core.h)
ST_PRESENT_ID = 1000294000

# Every child line reaches logcat twice (System.out at ProcessHelper.java:424 and
# the Timber bridge at XServerScreen.kt:1893), under different tags. Counters are
# first-vs-last deltas and immune to that; line *counts* are not, so a line is
# skipped when the same marker body repeats inside 50 ms.
MARKERS = ("ICD loaded", "proxy hb: ", "proxy commit: sc=",
           "proxy present: first pNext chain", "present wait #",
           "gn-vkshim stall: acquire found no free slot",
           "gn-vkshim release: slot=", "gn-vkshim idle:", "proxy acquire: sc=",
           "proxy wait: n=", "ATOMIC heartbeat #", "ATOMIC FAIL",
           "x11 stall ring (")


def num(v):
    try:
        return int(v)
    except ValueError:
        return v


def kv(line):
    out = {}
    for k, body in BRACKET.findall(line):
        out[k] = {k2: num(v2) for k2, v2 in KEYCOLON.findall(body)}
    for k, v in SCALAR.findall(line):
        out[k] = num(v)
    return out


def parse(path):
    """-> dict of series. Each series is [(t_seconds, payload)]."""
    s = {k: [] for k in ("hb", "cen", "cen_short", "rel", "commits", "chain",
                         "waits", "stalls", "acqs", "semwaits", "atomics", "rings",
                         "slots", "acq_census")}
    epoch = None
    seen = set()
    with open(path, "r", errors="replace") as fh:
        for line in fh:
            m = TS.match(line)
            if m:
                hh, mm, ss = int(m.group(1)[6:8]), int(m.group(1)[9:11]), m.group(1)[12:]
                t = hh * 3600 + mm * 60 + float(ss)
                if epoch is None:
                    epoch = t
                t -= epoch
            else:
                t = s["hb"][-1][0] if s["hb"] else 0.0
            cuts = [line.find(m) for m in MARKERS if m in line]
            if cuts:
                key = (round(t * 20), line[min(cuts):])
                if key in seen:
                    continue
                seen.add(key)
            if "proxy hb: fmt=" in line:
                s["hb"].append((t, kv(line.split("proxy hb: ", 1)[1])))
            elif "present mode: declared=" in line:
                d = kv(line.split("present mode: ", 1)[1])
                (s["cen"] if "acquires" in d else s["cen_short"]).append((t, d))
            elif "proxy commit: sc=" in line:
                tail = line.rsplit(" #", 1)
                if len(tail) == 2 and tail[1].split()[0].isdigit():
                    s["commits"].append((t, int(tail[1].split()[0])))
            elif "proxy present: first pNext chain" in line:
                ty = re.search(r"sTypes=\[([^\]]*)\]", line)
                tl = [int(x) for x in ty.group(1).split(",") if x.strip()] if ty else []
                pid = re.search(r"present_id=(\d+)", line)
                s["chain"].append((t, (tl, int(pid.group(1)) if pid else 0)))
            elif "present wait #" in line:
                m2 = re.search(r"present wait #(\d+): id=(\d+) (\w+) after (\d+) us", line)
                if m2:
                    s["waits"].append((t, (int(m2.group(1)), int(m2.group(2)),
                                           m2.group(3), int(m2.group(4)))))
            elif "gn-vkshim stall: acquire found no free slot" in line:
                s["stalls"].append((t, kv(line)))
            elif "gn-vkshim release: slot=" in line or "gn-vkshim idle:" in line:
                s["rel"].append((t, kv(line)))
            elif "proxy acquire: sc=" in line:
                m3 = re.search(r"wait=(\d+)ms", line)
                if m3:
                    s["acqs"].append((t, int(m3.group(1))))
            elif "proxy wait: n=" in line:
                s["semwaits"].append((t, kv(line)))
            elif "ATOMIC heartbeat #" in line:
                m4 = re.search(r"ATOMIC heartbeat #(\d+)", line)
                if m4:
                    s["atomics"].append((t, int(m4.group(1))))
            elif "x11 stall ring (" in line:
                s["rings"].append((t, line.strip()))
            if "acqus=[" in line:
                s["acq_census"].append((t, kv(line)))
            if "slots=[" in line:
                mm2 = re.search(r"slots=\[([^\]]*)\]", line)
                if mm2:
                    toks = [x for x in mm2.group(1).split() if x]
                    if toks:
                        a = sum(1 for x in toks if len(x) > 2 and x[2] == "A")
                        f = sum(1 for x in toks if len(x) > 3 and x[3] == "F")
                        s["slots"].append((t, (len(toks), a, f, len(toks) - a - f)))
    return s


def window(series, settle):
    src = series["cen"] or series["rel"]
    if not src:
        return (None, None)
    t_first = src[0][0]
    keep = [t for t, _ in src if t - t_first >= settle]
    if not keep:
        return (src[0][0], src[-1][0])
    return (keep[0], src[-1][0])


def d(series, key, t0, t1, sub=None):
    """First-vs-last delta of a cumulative counter inside [t0, t1]."""
    vals = []
    for t, payload in series:
        if t0 is not None and t < t0:
            continue
        if t1 is not None and t > t1:
            continue
        if isinstance(payload, dict):
            v = payload.get(key, {}).get(sub) if sub else payload.get(key)
        else:
            v = payload
        if isinstance(v, (int, float)):
            vals.append(v)
    return (vals[-1] - vals[0]) if len(vals) >= 2 else None


def per_s(series, key, t0, t1, sub=None):
    """Rate over the samples' OWN span, not the census window: the 1 Hz and the
    10 s series do not start and end on the same line."""
    pts = []
    for t, payload in series:
        if t0 is not None and t < t0:
            continue
        if t1 is not None and t > t1:
            continue
        v = payload.get(key, {}).get(sub) if (sub and isinstance(payload, dict)) else (
            payload.get(key) if isinstance(payload, dict) else payload)
        if isinstance(v, (int, float)):
            pts.append((t, v))
    if len(pts) < 2 or pts[-1][0] <= pts[0][0]:
        return None
    return (pts[-1][1] - pts[0][1]) / (pts[-1][0] - pts[0][0])





def main():
    path = sys.argv[1]
    settle = float(sys.argv[sys.argv.index("--settle") + 1]) if "--settle" in sys.argv else 30.0
    s = parse(path)
    t0, t1 = window(s, settle)
    dt = None if (t0 is None or t1 is None) else t1 - t0

    rows = []
    add = rows.append

    # ---- per-layer frame rate --------------------------------------------
    add(("guest presents/s        (vkQueuePresentKHR)", per_s(s["hb"], "present", t0, t1)))
    add(("guest acquires/s        (vkAcquireNextImageKHR)", per_s(s["hb"], "acq", t0, t1)))
    add(("guest wal commits/s     (N_COMMIT_OK, 'commits=')", per_s(s["cen"], "commits", t0, t1)))
    add(("daemon atomic ioctls/s  (ATOMIC heartbeat #N)", per_s(s["atomics"], None, t0, t1)))
    add(("compositor releases/s   (wl_buffer.release)", per_s(s["cen"], "releases", t0, t1)))
    add(("presented/s             (wp presentation feedback)", per_s(s["cen"], "presented", t0, t1)))
    add(("discarded/s             (mailbox replace)", per_s(s["cen"], "discarded", t0, t1)))
    if len(s["commits"]) >= 2:
        add(("shim commit probe #N/s  (proxy commit, 1/120)", (
            s["commits"][-1][1] - s["commits"][0][1]) /
            max(1e-9, s["commits"][-1][0] - s["commits"][0][0])))

    # ---- acquire dwell + starvation (cause C) ----------------------------
    acqus_src = s["acq_census"]
    dwell = ge8 = ge16 = nz = ghold = gus = None
    if acqus_src:
        first = next((p for t, p in acqus_src if t0 is None or t >= t0), acqus_src[0][1])
        last = acqus_src[-1][1]
        try:
            n0, n1 = first["acqus"]["n"], last["acqus"]["n"]
            if n1 > n0:
                dn = n1 - n0
                dwell = (last["acqus"]["sum"] - first["acqus"]["sum"]) / dn / 1000.0
                ge8 = (last["acqus"]["ge8"] - first["acqus"]["ge8"]) / dn
                ge16 = (last["acqus"]["ge16"] - first["acqus"]["ge16"]) / dn
                nz = (last["acqus"]["nz"] - first["acqus"]["nz"]) / dn
        except (KeyError, TypeError, ZeroDivisionError):
            pass
        try:
            ghold = last["cap"]["gate_holds"] - first["cap"]["gate_holds"]
            gus = last["cap"]["gate_us"] - first["cap"]["gate_us"]
        except (KeyError, TypeError):
            pass
    n_acq = d(s["cen"], "acquires", t0, t1)
    n_slot = d(s["cen"], "slot_waits", t0, t1)
    add(("acquire dwell mean (ms)", dwell))
    add(("acquire >=8.33 ms fraction", ge8))
    add(("acquire >=16.67 ms fraction", ge16))
    add(("acquire blocked at all (acqus nz/n)", nz))
    add(("gate hold mean (ms) [cap, not slot]", (gus / ghold / 1000.0) if ghold else 0.0))
    add(("slot_waits / acquires", (n_slot / n_acq) if (n_slot is not None and n_acq) else None))
    add(("starved (finite-timeout acquire failures)", d(s["acq_census"], "starved", t0, t1)))
    add(("gate_holds (acquires parked by the cap)", d(s["cen"], "gate_holds", t0, t1)))
    add(("gate_late / skipped", (d(s["cen"], "gate_late", t0, t1),
                                 d(s["cen"], "skipped", t0, t1))))
    if s["slots"]:
        depths = [p[0] for _, p in s["slots"]]
        win = [(t, p) for t, p in s["slots"] if t0 is None or t >= t0] or s["slots"]
        add(("slot depth (len of slots=[…])", depths[-1]))
        add(("mean in_flight slots", round(sum(p[2] for _, p in win) / len(win), 2)))
        add(("mean free slots (never A, never F)", round(sum(p[3] for _, p in win) / len(win), 2)))

    # ---- present-wait (cause B) ------------------------------------------
    stypes, ids = [], None
    if s["chain"]:
        stypes, pid0 = s["chain"][0][1]
        ids = (ST_PRESENT_ID in stypes) or pid0 != 0
    add(("present_id on first present chain", ids))
    add(("first pNext sTypes", stypes))
    outcomes = {}
    for _, (_, _, what, us) in s["waits"]:
        outcomes.setdefault(what, []).append(us)
    in_win = [w for w in s["waits"] if t0 is None or w[0] >= t0]
    add(("present-wait lines in window", len(in_win)))
    if len(s["waits"]) >= 2:
        add(("present waits/s (counter #N)", (s["waits"][-1][1][0] - s["waits"][0][1][0]) /
            max(1e-9, s["waits"][-1][0] - s["waits"][0][0])))
    add(("wait answered at queue (at_queue)", d(s["cen"], "at_queue", t0, t1)))
    add(("wait answered w/o latch (release or grace)", d(s["cen"], "fallback", t0, t1)))
    for what, us in sorted(outcomes.items()):
        add((f"  wait outcome '{what}' n / mean us / max us",
             (len(us), round(sum(us) / len(us), 1), max(us))))
    add(("vkWaitSemaphores calls/s", (len([w for w in s["semwaits"] if t0 is None or w[0] >= t0]) / dt)
         if dt else None))

    # ---- present-mode census ---------------------------------------------
    mode = {}
    if s["cen_short"]:
        mode.update(s["cen_short"][-1][1])
    if s["cen"]:
        mode.update(s["cen"][-1][1])
    add(("declared / requested / source / gate", (mode.get("declared"), mode.get("requested"),
                                                  mode.get("source"), mode.get("gate"))))
    add(("declared_fifo / declared_other", (d(s["cen"], "declared_fifo", t0, t1),
                                            d(s["cen"], "declared_other", t0, t1))))
    add(("feedback / lsfg", (mode.get("feedback"), mode.get("lsfg"))))
    add(("commit_lat_us (last)", s["cen"][-1][1].get("commit_lat_us") if s["cen"] else None))
    add(("stall lines", len(s["stalls"])))
    add(("stall-ring dumps (acquire >=1 s)", len(s["rings"])))

    print(f"# capture {path}")
    print(f"# window {t0:.1f}..{t1:.1f} s  ({dt:.1f} s, settle {settle:g} s)"
          if dt is not None else "# window: NO CENSUS LINE FOUND")
    w = max(len(k) for k, _ in rows)
    for k, v in rows:
        print(f"{k:<{w}}  {round(v, 3) if isinstance(v, float) else v}")

    print("\n# verdicts")
    total_waits = sum(len(v) for v in outcomes.values())
    if ids is False:
        print("B REFUTED (precondition): no VkPresentIdKHR chained, no vkWaitForPresentKHR.")
    elif outcomes:
        slow = [u for v in outcomes.values() for u in v if u > 1000]
        print(f"B ACTIVE (ids+waits): {total_waits} sampled waits, {len(slow)} >1 ms"
              f" -> {'CONFIRMED: the wait is the per-frame stall' if slow else 'mechanism REFUTED: waits answered fast'}")
    else:
        print("B: ids advertised, zero wait calls sampled -> present_wait unused")
    sw = (n_slot / n_acq) if (n_slot is not None and n_acq) else None
    if sw is None:
        # No census line at all is NOT "the gate never reached the guest": with
        # displayServer=x11 the ICD shim is never loaded (the guest's WSI is
        # Mesa's), so the control arm cannot carry this measurement by design.
        print("C: UNDECIDED in this capture - no shim census line. If this arm "
              "ran with displayServer=x11 that is expected (the shim is "
              "gamescope-only) and the arm is a control, not a measurement.")
    else:
        print(f"C {'CONFIRMED' if (sw >= 0.5 and (ge8 or 0) > 0.1) else ('REFUTED' if sw <= 0.05 else 'PARTIAL')}:"
              f" slot_waits/acquires={sw:.1%}, dwell={dwell if dwell is None else round(dwell, 2)} ms,"
              f" >=8.33 ms={ge8 if ge8 is None else round(ge8, 1)}")


if __name__ == "__main__":
    main()
