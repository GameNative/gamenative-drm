#!/usr/bin/env bash
# str-count.sh — hit-like string census for a built library (acceptance metric).
#
# "message-like" = a `strings` run shaped like prose rather than an identifier:
# two 3+ letter words separated by a space (e.g. "scan-out acceptor poll",
# "connect(%s) failed"), excluding rust-std noise (constant across before/after
# so the delta is the shipped message text that was removed). Requiring a space
# keeps identifier-like strings (Vulkan names, underscored symbols) out.
set -euo pipefail
f="$1"
n="${2:-6}"
all=$(strings -a -n "$n" "$f" | wc -l)
std=$(strings -a -n "$n" "$f" | grep -cE '\.rustup/toolchains|/rustc/|library/|\.llvm\.|closure|__rust|::<|\.dwo|core::' || true)
msg=$(strings -a -n "$n" "$f" \
    | grep -E '[A-Za-z]{3} +[A-Za-z]{3}' \
    | grep -vE '\.rustup/toolchains|/rustc/|library/|\.llvm\.|closure|__rust|::<|\.dwo|core::' \
    | wc -l)
panicloc=$(strings -a "$f" | grep -c '\.rustup/toolchains' || true)
printf '%s\n  strings(n>=%s): %s\n  message-like: %s\n  std/backtrace noise: %s\n  std panic-location: %s\n' \
    "$(basename "$f")" "$n" "$all" "$msg" "$std" "$panicloc"
