#!/usr/bin/env python3
"""Byte-anchored edit of one container's envVars value, for the step-1 arm.

The container file (`.container`) is read by Container.java's save/load, which
serialises *every* field; a round-trip through a JSON writer is therefore safe
only if every key survives. This tool never re-serialises: it replaces the
characters between the two quotes of the `"envVars"` value and nothing else, so
every other byte of the file (key order, spacing, the huge dxwrapperConfig and
extraData/ sessionMetadata blobs) is preserved verbatim.

  plan1-flip.py edit    <in> <out> K=V [K=V ...]   # add or replace K=V
  plan1-flip.py unset   <in> <out> K [K ...]       # remove K entirely
  plan1-flip.py check   <in> <out> [K=V ...]       # prove only envVars moved
  plan1-flip.py show    <file>                     # print envVars as tokens
"""
import json
import re
import sys

PAT = re.compile(r'"envVars"\s*:\s*"((?:[^"\\]|\\.)*)"')


def split(blob):
    s = blob.decode("utf-8")
    m = PAT.search(s)
    if not m:
        sys.exit("FATAL: no \"envVars\" string in the file (wrong file?)")
    raw = m.group(1)
    cur = json.loads('"' + raw + '"')
    return s, m, cur


def rewrite(blob, tokens):
    s, m, _ = split(blob)
    new = " ".join(tokens)
    enc = json.dumps(new, ensure_ascii=False)[1:-1]
    out = s[: m.start(1)] + enc + s[m.end(1):]
    try:
        json.loads(out)
    except Exception as e:                        # never push invalid JSON
        sys.exit(f"FATAL: edited file is not valid JSON: {e}")
    return out.encode("utf-8")


def tokens_of(cur):
    return [t for t in cur.split(" ") if t]


def apply(blob, pairs, remove=()):
    _, _, cur = split(blob)
    toks = tokens_of(cur)
    drop = {k for k, _ in pairs} | set(remove)
    toks = [t for t in toks if t.split("=", 1)[0] not in drop]
    for k, v in pairs:
        toks.append(f"{k}={v}")
    return rewrite(blob, toks)


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    cmd, src = sys.argv[1], sys.argv[2]
    blob = open(src, "rb").read()
    if cmd == "show":
        _, _, cur = split(blob)
        for t in tokens_of(cur):
            print(t)
        return
    out = sys.argv[3]
    if cmd == "edit":
        ob = apply(blob, [a.split("=", 1) for a in sys.argv[4:]])
    elif cmd == "unset":
        ob = apply(blob, [], remove=sys.argv[4:])
    elif cmd == "check":
        ob = open(out, "rb").read()
        a, b = json.loads(blob), json.loads(ob)
        keys = sorted(set(a) | set(b))
        bad = [k for k in keys if k != "envVars" and a.get(k) != b.get(k)]
        if bad:
            sys.exit(f"FATAL: keys changed besides envVars: {bad}")
        print(f"ok: envVars only; {len(keys) - 1} other keys identical")
        print(f"    bytes {len(blob)} -> {len(ob)}  ({len(ob) - len(blob):+d})")
        return
    else:
        sys.exit(f"unknown command {cmd}")
    open(out, "wb").write(ob)
    print(f"{src} -> {out}: {len(blob)} -> {len(ob)} bytes "
          f"({len(ob) - len(blob):+d})")


if __name__ == "__main__":
    main()
