#!/usr/bin/env python3
"""Audit the hand-transcribed VK_* constants in the Rust vk.rs FFI files
against a reference vulkan_core.h. The turnip driver dispatches pNext chain
walks on sType values, so a mis-transcribed constant is not a cosmetic
error: unrecognized chain entries are silently skipped (and entries the
driver requires are then NULL).

Usage: tools/check-vk-constants.py [path/to/vulkan_core.h]
Default header: /usr/include/vulkan/vulkan_core.h (vulkan-headers package).
Exit 1 on any mismatch.
"""

import re
import sys

HDR = sys.argv[1] if len(sys.argv) > 1 else "/usr/include/vulkan/vulkan_core.h"
ROOT = re.sub(r"/tools/[^/]*$", "", __file__)

hdr = open(HDR).read()
vals = {}
for m in re.finditer(r"^\s+(VK_\w+)\s*=\s*(0x[0-9A-Fa-f]+|\d+)\s*,", hdr, re.M):
    vals[m.group(1)] = int(m.group(2), 0)
for m in re.finditer(r"^#define\s+(VK_\w+)\s+(0x[0-9A-Fa-f]+|\d+)[uUlL]*\s*$", hdr, re.M):
    vals[m.group(1)] = int(m.group(2), 0)

bad = 0
for path in [f"{ROOT}/utils/src/vk.rs", f"{ROOT}/icd_shim/src/vk.rs"]:
    src = open(path).read()
    for m in re.finditer(r"pub const (VK_\w+)\s*:\s*\w+\s*=\s*(0x[0-9A-Fa-f]+|\d+)", src):
        name, v = m.group(1), int(m.group(2), 0)
        ref = vals.get(name)
        if ref is not None and ref != v:
            print(f"MISMATCH {path}: {name}: rust={v:#x} header={ref:#x}")
            bad += 1
if bad:
    print(f"check-vk-constants: {bad} mismatch(es)")
    sys.exit(1)
print("check-vk-constants: all VK_* constants match the reference header")
