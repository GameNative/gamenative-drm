#!/usr/bin/env python3
"""gen-vk-dispatch.py — generate `icd_shim/src/vk_dispatch_gen.rs` from vk.xml.

The ICD shim must serve every entry point the Khronos loader can hand out, not
just the ones it implements. Which Vulkan objects a command receives in
argument 0 decides whether the shim's wrapper has to be swapped for the real
driver handle (VkDevice / VkQueue / VkCommandBuffer) or passed through
untouched (VkInstance / VkPhysicalDevice / no dispatchable). That classification
is a property of the registry, not of our code, so it is generated here instead
of maintained by hand — the hand table drifted from the loader's export set and
cost the `vkCreateSamplerYcbcrConversion` crash class.

Level / arg0 are read from the command's first <param><type>:

    VkDevice|VkQueue|VkCommandBuffer -> Level::Device         (arg0 is swapped)
    VkPhysicalDevice                 -> Level::PhysicalDevice (pass-through)
    VkInstance                       -> Level::Instance       (pass-through)
    anything else / no params        -> Level::Global         (pass-through)

Alias commands (`<command name=... alias=.../>`) carry no prototype of their
own; their level is inherited from the alias target, transitively. The alias
edges are emitted too, replacing the hand-written core<->KHR remap list.

Usage:
    tools/gen-vk-dispatch.py [--xml PATH] [--out PATH] [--check]

Default XML is the vendored `icd_shim/registry/vk.xml`; default output is
`icd_shim/src/vk_dispatch_gen.rs`. `--check` regenerates to a temp copy and
exits non-zero if it differs from the committed file (staleness guard).
"""

import argparse
import hashlib
import io
import re
import sys
import xml.etree.ElementTree as ET

ROOT = re.sub(r"/tools/[^/]*$", "", __file__)
DEFAULT_XML = f"{ROOT}/icd_shim/registry/vk.xml"
DEFAULT_OUT = f"{ROOT}/icd_shim/src/vk_dispatch_gen.rs"

DEVICE_ARG0 = {"VkDevice", "VkQueue", "VkCommandBuffer"}
LEVELS = ["Global", "Instance", "PhysicalDevice", "Device"]
ARG0S = ["None", "Instance", "PhysicalDevice", "Device", "Queue", "CommandBuffer", "Other"]

# The handle types the shim wraps (it substitutes the driver's real handle for
# these). Nested translation is limited to them, because they are the only
# handles a guest can hold as a shim pointer. Map: registry type -> enum name.
WRAPPED_HANDLES = {
    "VkDevice": "Device",
    "VkQueue": "Queue",
    "VkCommandBuffer": "CommandBuffer",
}


def command_name(c):
    if c.get("name"):
        return c.get("name")
    n = c.find("proto/name")
    return n.text if n is not None else None


def struct_members(root):
    """Registry struct/union member tables: name -> {"alias", "members"} with
    each member a dict of name/type/ptr/const/len. Alias structs carry no
    members of their own and are resolved on lookup."""
    structs = {}
    for t in root.findall("types/type"):
        if t.get("category") not in ("struct", "union"):
            continue
        name = t.get("name")
        if name is None:
            continue
        if t.get("alias"):
            structs[name] = {"alias": t.get("alias"), "members": []}
            continue
        members = []
        for m in t.findall("member"):
            mt = m.find("type")
            mn = m.find("name")
            if mt is None or mn is None:
                continue
            text = "".join(m.itertext())
            members.append(
                {
                    "name": mn.text,
                    "type": mt.text,
                    "ptr": "*" in text,
                    "const": "const" in text,
                    "len": m.get("len"),
                }
            )
        structs[name] = {"alias": None, "members": members}
    return structs


def members_of(structs, sname, seen=None):
    """Members of `sname`, following struct aliases transitively."""
    seen = seen or set()
    if sname in seen or sname not in structs:
        return []
    d = structs[sname]
    if d["alias"]:
        return members_of(structs, d["alias"], seen | {sname})
    return d["members"]


def walk_struct_handles(structs, sname):
    """Wrapped-handle inputs reachable inside `sname` through a counted pointer
    array. Returns `(kind, counted, hops)` where each hop describes one array
    level: the byte offset of the child pointer, its element stride, and the
    byte offset of its `uint32_t` count (`None` = a single handle value).

    A pointer member that is not `const` is an output and is skipped; a member
    that is a `const` pointer to a handle array is counted. Terminal by-value
    handle members are a single value per element."""
    out = []
    for m in members_of(structs, sname):
        mt = m["type"]
        if mt in WRAPPED_HANDLES:
            if m["ptr"] and not m["const"]:
                continue  # array of handles the command writes (an output)
            if m["ptr"]:
                out.append(
                    (
                        WRAPPED_HANDLES[mt],
                        True,
                        [
                            {
                                "off": (sname, m["name"]),
                                "stride": ("handle",),
                                "count_off": (sname, m["len"]) if m["len"] else None,
                            }
                        ],
                    )
                )
            else:
                out.append(
                    (
                        WRAPPED_HANDLES[mt],
                        False,
                        [
                            {
                                "off": (sname, m["name"]),
                                "stride": None,
                                "count_off": None,
                            }
                        ],
                    )
                )
        elif mt in structs and m["ptr"] and m["const"]:
            head = {
                "off": (sname, m["name"]),
                "stride": ("struct", mt),
                "count_off": (sname, m["len"]) if m["len"] else None,
            }
            for kind, counted, sub in walk_struct_handles(structs, mt):
                out.append((kind, True or counted, [head] + sub))
    return out


def command_nested(c, params, structs):
    """Registry-derived nested wrapped-handle *inputs* a command consumes
    through at least one counted pointer array — the pCommandBuffers-style
    forwarding hazard. Returns a list of
    `(param, count_param, kind, elem_stride, hops)`."""
    def param_index(name):
        if not name:
            return 255
        for i, p in enumerate(params):
            pn = p.find("name")
            if pn is not None and pn.text == name:
                return i
        return 255

    out = []
    for i, p in enumerate(params):
        pt = p.find("type")
        pn = p.find("name")
        if pt is None or pn is None:
            continue
        ptype = pt.text
        text = "".join(p.itertext())
        ptr = "*" in text
        const = "const" in text
        cp = param_index(p.get("len"))
        if ptype in WRAPPED_HANDLES:
            # A by-value or non-const pointer handle parameter is not a nested
            # array hazard; only a `const` pointer to a handle array is.
            if ptr and const:
                out.append((i, cp, WRAPPED_HANDLES[ptype], ("handle",), []))
        elif ptype in structs and ptr and const:
            for kind, counted, hops in walk_struct_handles(structs, ptype):
                if counted or cp != 255:
                    out.append((i, cp, kind, ("struct", ptype), hops))
    return out



def arg0_type(c):
    ps = c.findall("param")
    if not ps:
        return None
    t = ps[0].find("type")
    return t.text if t is not None else None


def level_of(t):
    if t in DEVICE_ARG0:
        return "Device"
    if t == "VkPhysicalDevice":
        return "PhysicalDevice"
    if t == "VkInstance":
        return "Instance"
    return "Global"


def arg0_of(t):
    if t is None:
        return "None"
    if t in DEVICE_ARG0:
        return t[2:]  # VkDevice -> Device, VkQueue -> Queue, VkCommandBuffer -> CommandBuffer
    if t in ("VkPhysicalDevice", "VkInstance"):
        return t[2:]
    return "Other"


def build(xml_path):
    raw = open(xml_path, "rb").read()
    sha = hashlib.sha256(raw).hexdigest()
    m = re.search(rb'<name>VK_HEADER_VERSION</name>\s*(\d+)</type>', raw)
    hdr = m.group(1).decode() if m else "unknown"
    root = ET.fromstring(raw)

    cmds = {}
    for c in root.findall("commands/command"):
        n = command_name(c)
        if n:
            cmds[n] = c

    # Resolve each command's level/arg0 from its own prototype, following
    # `alias` edges transitively when the command has no prototype of its own.
    resolved = {}
    visiting = set()

    def resolve(n):
        if n in resolved:
            return resolved[n]
        c = cmds.get(n)
        if c is None:
            return None
        if n in visiting:  # defensive: registry has no alias cycles
            return None
        t = arg0_type(c)
        if t is not None:
            resolved[n] = (level_of(t), arg0_of(t))
            return resolved[n]
        al = c.get("alias")
        if al is None:
            resolved[n] = ("Global", "None")
            return resolved[n]
        visiting.add(n)
        r = resolve(al)
        visiting.discard(n)
        resolved[n] = r if r is not None else ("Global", "None")
        return resolved[n]

    entries = []
    aliases = []
    for n, c in cmds.items():
        r = resolve(n)
        if r is None:
            continue
        lv, a0 = r
        entries.append((n, lv, a0))
        if c.get("alias"):
            aliases.append((n, c.get("alias")))
    entries.sort()
    aliases.sort()

    # Nested wrapped-handle inputs, per command. Alias commands carry no
    # prototype of their own, so they inherit the target's set transitively.
    structs = struct_members(root)
    nested = {}
    for n, c in cmds.items():
        if c.get("alias"):
            continue
        e = command_nested(c, c.findall("param"), structs)
        if e:
            nested[n] = e

    def nested_of(n, seen=None):
        seen = seen or set()
        if n in nested:
            return nested[n]
        if n in seen:
            return []
        c = cmds.get(n)
        if c is None or c.get("alias") is None:
            return []
        return nested_of(c.get("alias"), seen | {n})

    for n, c in cmds.items():
        if c.get("alias"):
            e = nested_of(n)
            if e:
                nested[n] = e

    levels = {n: lv for n, lv, _ in entries}
    return hdr, sha, entries, aliases, nested, build_wsi(root), build_resolve(root, levels)


def emit_path(elem):
    if elem[0] == "handle":
        return "std::mem::size_of::<crate::vk::VkCommandBuffer>()"
    return f"std::mem::size_of::<crate::vk::{elem[1]}>()"


def emit_hop_stride(stride):
    if stride is None:
        return "0"
    return emit_path(stride)


def emit_hop(hop):
    sname, mname = hop["off"]
    off = f"std::mem::offset_of!(crate::vk::{sname}, {mname})"
    stride = emit_hop_stride(hop["stride"])
    co = hop["count_off"]
    count_off = (
        f"std::mem::offset_of!(crate::vk::{co[0]}, {co[1]})" if co else "usize::MAX"
    )
    return (
        f"Hop {{ off: {off}, stride: {stride}, count_off: {count_off} }}"
    )


def render(xml_path, hdr, sha, entries, aliases, nested):
    b = io.StringIO()
    w = b.write
    w("// Generated by tools/gen-vk-dispatch.py from the Vulkan registry — do not edit.\n")
    w("//\n")
    w("// Source: icd_shim/registry/vk.xml (Khronos Vulkan-Headers, Apache-2.0 OR MIT).\n")
    w(f"// VK_HEADER_VERSION {hdr}; sha256 {sha}\n")
    w("//\n")
    w("// One entry per registry command: the dispatch level and argument-0 kind\n")
    w("// derived from its first parameter. `Level::Device` means argument 0 is a\n")
    w("// device-child handle the shim wraps (VkDevice/VkQueue/VkCommandBuffer), so\n")
    w("// an unlisted name at that level is served by a wrapper-swapping thunk;\n")
    w("// every other level's argument 0 is the driver's own handle and is passed\n")
    w("// through untouched.\n")
    w("\n")
    w("#[derive(Clone, Copy, PartialEq, Eq, Debug)]\n")
    w("pub enum Level {\n")
    for lv in LEVELS:
        w(f"    {lv},\n")
    w("}\n\n")
    w("#[derive(Clone, Copy, PartialEq, Eq, Debug)]\n")
    w("pub enum Arg0 {\n")
    for a in ARG0S:
        w(f"    {a},\n")
    w("}\n\n")
    w("/// `(name, level, arg0)` for one registry command. A bare tuple, not a\n")
    w("/// struct, so rustfmt keeps each entry on one line and the table stays\n")
    w("/// grep-able.\n")
    w("pub type Dispatch = (&'static str, Level, Arg0);\n\n")
    w("// One row per command; kept single-line so the table stays diff- and\n")
    w("// grep-friendly (rustfmt would otherwise expand every row).\n")
    w("#[rustfmt::skip]\n")
    w("pub static VK_DISPATCH: &[Dispatch] = &[\n")
    for n, lv, a0 in entries:
        w(f'    ("{n}", Level::{lv}, Arg0::{a0}),\n')
    w("];\n\n")
    w("/// Command aliases from the registry: `name` resolves to `target` when the\n")
    w("/// driver exports only one spelling of the pair. Replaces the hand-written\n")
    w("/// core<->KHR remap list.\n")
    w("#[rustfmt::skip]\n")
    w("pub static VK_ALIASES: &[(&str, &str)] = &[\n")
    for n, t in aliases:
        w(f'    ("{n}", "{t}"),\n')
    w("];\n\n")
    w("/// Registry classification for `name`, or `None` when the name is not a\n")
    w("/// registry command (an extension name, a typo, or vendor-specific).\n")
    w("pub fn classify(name: &str) -> Option<&'static Dispatch> {\n")
    w("    VK_DISPATCH\n")
    w("        .binary_search_by(|d| d.0.cmp(name))\n")
    w("        .ok()\n")
    w("        .map(|i| &VK_DISPATCH[i])\n")
    w("}\n\n")
    w("/// The other spelling of `name` from the registry alias table, or `None`\n")
    w("/// when `name` is not part of an alias pair. The registry records the\n")
    w("/// extension spelling as the alias of the core name; a driver may export\n")
    w("/// either, so the lookup resolves both directions.\n")
    w("pub fn alias_of(name: &str) -> Option<&'static str> {\n")
    w("    if let Ok(i) = VK_ALIASES.binary_search_by(|(n, _)| n.cmp(&name)) {\n")
    w("        return Some(VK_ALIASES[i].1);\n")
    w("    }\n")
    w("    VK_ALIASES.iter().find(|(_, t)| *t == name).map(|(n, _)| *n)\n")
    w("}\n\n")
    w("/// A wrapped-handle type the shim substitutes for the driver's real\n")
    w("/// handle when it forwards a command.\n")
    w("#[derive(Clone, Copy, PartialEq, Eq, Debug)]\n")
    w("pub enum HandleKind {\n")
    w("    Device,\n")
    w("    Queue,\n")
    w("    CommandBuffer,\n")
    w("}\n\n")
    w("/// One array level on the way to a nested handle value. `off` is the byte\n")
    w("/// offset, within the enclosing element, of the pointer to the child array\n")
    w("/// (or of a single handle value when `count_off` is `usize::MAX`); `stride`\n")
    w("/// is the child's element stride (`0` for a single value); `count_off` is\n")
    w("/// the byte offset of the child's `uint32_t` count.\n")
    w("#[derive(Clone, Copy, PartialEq, Eq, Debug)]\n")
    w("pub struct Hop {\n")
    w("    pub off: usize,\n")
    w("    pub stride: usize,\n")
    w("    pub count_off: usize,\n")
    w("}\n\n")
    w("/// A shim-wrapped handle a command consumes in a nested position. `param`\n")
    w("/// is the parameter index; when `count_param` is `255` the parameter is a\n")
    w("/// single value, otherwise it is an array of `elem_stride`-byte elements\n")
    w("/// counted by parameter `count_param`. Each element is walked through\n")
    w("/// `hops`; an empty path means the element *is* a handle value.\n")
    w("#[derive(Clone, Copy, PartialEq, Eq, Debug)]\n")
    w("pub struct NestedHandle {\n")
    w("    pub param: u8,\n")
    w("    pub count_param: u8,\n")
    w("    pub kind: HandleKind,\n")
    w("    pub elem_stride: usize,\n")
    w("    pub hops: &'static [Hop],\n")
    w("}\n\n")
    w("// One row per command that consumes a wrapped handle through a counted\n")
    w("// pointer array; sorted by name for the binary search in `nested_handles`.\n")
    w("// Kept single-line so the table stays grep-friendly.\n")
    w("#[rustfmt::skip]\n")
    w("pub static VK_NESTED_HANDLES: &[(&str, &[NestedHandle])] = &[\n")
    for n in sorted(nested):
        w(f'    ("{n}", &[\n')
        for (param, cp, kind, elem, hops) in nested[n]:
            hs = ", ".join(emit_hop(h) for h in hops)
            w(
                f"        NestedHandle {{ param: {param}, count_param: {cp}, "
                f"kind: HandleKind::{kind}, elem_stride: {emit_path(elem)}, "
                f"hops: &[{hs}] }},\n"
            )
        w("    ]),\n")
    w("];\n\n")
    w("/// Registry-derived nested wrapped handles for `name`; empty when the\n")
    w("/// command consumes none.\n")
    w("pub fn nested_handles(name: &str) -> &'static [NestedHandle] {\n")
    w("    match VK_NESTED_HANDLES.binary_search_by(|(n, _)| n.cmp(&name)) {\n")
    w("        Ok(i) => VK_NESTED_HANDLES[i].1,\n")
    w("        Err(_) => &[],\n")
    w("    }\n")
    w("}\n")
    return b.getvalue()


# ---------------------------------------------------------------- WSI scope
#
# Under gamescope the shim owns the swapchain and the surfaces, so every
# extension whose subject is a surface, a swapchain or a present is a question
# about the shim, not about the driver. Which extensions those are is a
# property of the registry; `wsi_policy.rs` decides, per name, whether the shim
# serves it or hides it, and a test fails when the registry gains a WSI-scoped
# name the policy does not classify.

# The WSI roots: an extension that depends on one of these (directly, or
# through another WSI-scoped extension) is WSI-scoped.
WSI_ROOTS = {
    "VK_KHR_surface",
    "VK_KHR_swapchain",
    "VK_KHR_display",
    "VK_KHR_get_surface_capabilities2",
}
# Structures whose extension structs only say something about a surface, a
# swapchain or a present. An extension with a struct that extends nothing but
# these is WSI-scoped. A struct that also extends a non-WSI structure
# (VkImageCompressionControlEXT extends VkImageCreateInfo too) does not count:
# its extension is about images and only touches swapchains on the side.
WSI_STRUCTS = {
    "VkPresentInfoKHR",
    "VkSwapchainCreateInfoKHR",
    "VkSurfaceCapabilities2KHR",
    "VkPhysicalDeviceSurfaceInfo2KHR",
    "VkAcquireNextImageInfoKHR",
}
# Name tokens: `_present` matches present_id and shared_presentable_image but
# not representative_fragment_test.
WSI_NAME = re.compile(r"_(surface|swapchain|present|display)", re.I)
# Platforms an extension can exist on here: Android drivers (android), and the
# three surface platforms the shim proxies for its guests. The rest (win32,
# metal, fuchsia, ...) are never listed by a driver on this device.
WSI_PLATFORMS = {None, "android", "xlib", "xcb", "wayland"}


def stype_values(root):
    """VK_STRUCTURE_TYPE_* name -> value, core and extension-defined."""
    vals = {}
    aliases = {}
    for en in root.findall("enums"):
        if en.get("name") != "VkStructureType":
            continue
        for e in en.findall("enum"):
            if e.get("value") is not None:
                vals[e.get("name")] = int(e.get("value"), 0)
            elif e.get("alias"):
                aliases[e.get("name")] = e.get("alias")

    def ext_enum(e, extnumber):
        if e.get("extends") != "VkStructureType":
            return
        n = e.get("name")
        if e.get("offset") is not None:
            num = int(e.get("extnumber") or extnumber)
            v = 1_000_000_000 + (num - 1) * 1000 + int(e.get("offset"))
            vals[n] = -v if e.get("dir") == "-" else v
        elif e.get("value") is not None:
            vals[n] = int(e.get("value"), 0)
        elif e.get("alias"):
            aliases[n] = e.get("alias")

    for x in root.findall("extensions/extension"):
        for r in x.findall("require"):
            for e in r.findall("enum"):
                ext_enum(e, x.get("number"))
    for f in root.findall("feature"):
        for r in f.findall("require"):
            for e in r.findall("enum"):
                ext_enum(e, None if e.get("extnumber") is None else e.get("extnumber"))
    for n, a in aliases.items():
        seen = set()
        while a in aliases and a not in seen:
            seen.add(a)
            a = aliases[a]
        if a in vals:
            vals[n] = vals[a]
    return vals


def resolve_struct(types, tn):
    """The registry type `tn`, following struct aliases."""
    t = types.get(tn)
    seen = set()
    while t is not None and t.get("alias") and tn not in seen:
        seen.add(tn)
        tn = t.get("alias")
        t = types.get(tn)
    return t


def struct_feature(t, stypes):
    """`(sType, leading VkBool32 count)` of a feature struct, or None."""
    if t is None or t.get("category") != "struct":
        return None
    if "VkPhysicalDeviceFeatures2" not in (t.get("structextends") or "").split(","):
        return None
    st = None
    bools = 0
    for m in t.findall("member"):
        mn = m.find("name").text
        mt = m.find("type").text
        if mn == "sType":
            st = stypes.get(m.get("values"))
        elif mn == "pNext":
            continue
        elif mt == "VkBool32" and "*" not in "".join(m.itertext()):
            bools += 1
        else:
            break
    return None if st is None else (st, bools)


def require_features(r, types, stypes):
    """The feature structs a <require> block names."""
    for tr in r.findall("type"):
        f = struct_feature(resolve_struct(types, tr.get("name")), stypes)
        if f is not None:
            yield f


def build_wsi(root):
    """The WSI-scoped extensions: `(name, type, [(feature sType, leading
    VkBool32 count)], [command])`, sorted by name. Commands are the ones only
    WSI-scoped extensions require; a command a core version or a non-WSI
    extension also requires (vkAcquireNextImage2KHR, for VK_KHR_device_group)
    is not the WSI extension's to refuse."""
    types = {t.get("name"): t for t in root.findall("types/type") if t.get("name")}
    stypes = stype_values(root)
    exts = {}
    for x in root.findall("extensions/extension"):
        if "vulkan" not in (x.get("supported") or "").split(","):
            continue
        exts[x.get("name")] = x

    def own_types(x):
        # Types of the extension proper: a require block with a `depends` is
        # an interaction with another extension, not the extension's subject.
        for r in x.findall("require"):
            if r.get("depends"):
                continue
            for t in r.findall("type"):
                yield t.get("name")

    def direct(n, x):
        if n in WSI_ROOTS or WSI_NAME.search(n[2:]):
            return True
        for tn in own_types(x):
            t = types.get(tn)
            ext = set(filter(None, (t.get("structextends") or "").split(","))) if t is not None else set()
            if ext and ext <= WSI_STRUCTS:
                return True
        return False

    wsi = {n for n, x in exts.items() if direct(n, x)}
    grew = True
    while grew:
        grew = False
        for n, x in exts.items():
            if n not in wsi and set(re.findall(r"VK_\w+", x.get("depends") or "")) & wsi:
                wsi.add(n)
                grew = True
    wsi = {n for n in wsi if exts[n].get("platform") in WSI_PLATFORMS}

    # Every command a core version or a non-WSI extension requires.
    shared = set()
    for f in root.findall("feature"):
        for r in f.findall("require"):
            for c in r.findall("command"):
                shared.add(c.get("name"))
    for n, x in exts.items():
        if n in wsi:
            continue
        for r in x.findall("require"):
            for c in r.findall("command"):
                shared.add(c.get("name"))

    out = []
    for n in sorted(wsi):
        x = exts[n]
        feats = []
        cmds = []
        for r in x.findall("require"):
            for f in require_features(r, types, stypes):
                if f not in feats:
                    feats.append(f)
            for c in r.findall("command"):
                cn = c.get("name")
                if cn not in shared and cn not in cmds:
                    cmds.append(cn)
        out.append((n, x.get("type"), sorted(feats), sorted(cmds)))
    return out


def render_wsi(wsi):
    b = io.StringIO()
    w = b.write
    w("\n")
    w("/// Whether a WSI-scoped extension is enumerated per instance or per device.\n")
    w("#[derive(Clone, Copy, PartialEq, Eq, Debug)]\n")
    w("pub enum ExtKind {\n")
    w("    Instance,\n")
    w("    Device,\n")
    w("}\n\n")
    w("/// One WSI-scoped extension: its name, kind, feature structs\n")
    w("/// `(sType, leading VkBool32 members)` and the commands only WSI-scoped\n")
    w("/// extensions require.\n")
    w("pub type WsiExt = (\n")
    w("    &'static str,\n")
    w("    ExtKind,\n")
    w("    &'static [(i32, u32)],\n")
    w("    &'static [&'static str],\n")
    w(");\n\n")
    w("// WSI-scoped extensions, sorted by name: an extension whose `depends`\n")
    w("// names VK_KHR_surface, VK_KHR_swapchain, VK_KHR_display or\n")
    w("// VK_KHR_get_surface_capabilities2 (transitively through other WSI-scoped\n")
    w("// extensions), one with a struct extending only present/swapchain/surface\n")
    w("// structures, or one named for a surface, swapchain, present or display;\n")
    w("// platform-specific ones only for android/xlib/xcb/wayland.\n")
    w("#[rustfmt::skip]\n")
    w("pub static VK_WSI_EXTENSIONS: &[WsiExt] = &[\n")
    for n, kind, feats, cmds in wsi:
        k = "Instance" if kind == "instance" else "Device"
        fs = ", ".join(f"({s}, {c})" for s, c in feats)
        cs = ", ".join(f'"{c}"' for c in cmds)
        w(f'    ("{n}", ExtKind::{k}, &[{fs}], &[{cs}]),\n')
    w("];\n")
    return b.getvalue()


# ------------------------------------------------------------ resolve probe
#
# The shim must not advertise a core version whose commands it cannot resolve
# on the guest's device (`resolve_probe.rs`, docs/22-unresolvable-extensions.md).
# Which commands those are is the registry's: per core version the device-level
# commands of its feature blocks. Its `VkPhysicalDeviceVulkan1xFeatures` struct
# rides along so a clamped version's features read false.

CORE_VERSION = re.compile(r"^VK_(?:BASE_|COMPUTE_|GRAPHICS_)?VERSION_1_(\d+)$")


def api_has(elem, api="vulkan"):
    a = elem.get("api")
    return a is None or api in a.split(",")


def build_resolve(root, levels):
    """`[(minor, [device command], [(sType, bools)])]` for VK_VERSION_1_1 and
    up, ascending; the feature list is the version's own
    `VkPhysicalDeviceVulkan1<minor>Features` (promoted extensions' structs stay
    with their extensions)."""
    types = {t.get("name"): t for t in root.findall("types/type") if t.get("name")}
    stypes = stype_values(root)
    cores = {}
    for f in root.findall("feature"):
        m = CORE_VERSION.match(f.get("name") or "")
        if not m or not api_has(f) or int(m.group(1)) < 1:
            continue
        minor = int(m.group(1))
        cmds, feats = cores.setdefault(minor, ([], []))
        ft = struct_feature(types.get(f"VkPhysicalDeviceVulkan1{minor}Features"), stypes)
        if ft is not None and ft not in feats:
            feats.append(ft)
        for r in f.findall("require"):
            if not api_has(r):
                continue
            for c in r.findall("command"):
                cn = c.get("name")
                if levels.get(cn) == "Device" and cn not in cmds:
                    cmds.append(cn)
    return [(k, sorted(v[0]), sorted(v[1])) for k, v in sorted(cores.items())]


def render_resolve(cores):
    b = io.StringIO()
    w = b.write
    w("\n")
    w("/// One core version for the resolve probe: `VK_API_VERSION_1_x`, the\n")
    w("/// device-level commands it adds and its `VkPhysicalDeviceVulkan1xFeatures`\n")
    w("/// struct `(sType, leading VkBool32 members)`.\n")
    w("pub type CoreCommands = (u32, &'static [&'static str], &'static [(i32, u32)]);\n\n")
    w("// VK_VERSION_1_1 and up, ascending; each version's base, compute and\n")
    w("// graphics feature blocks folded in.\n")
    w("#[rustfmt::skip]\n")
    w("pub static VK_CORE_DEVICE_COMMANDS: &[CoreCommands] = &[\n")
    for minor, cmds, feats in cores:
        cs = ", ".join(f'"{c}"' for c in cmds)
        fs = ", ".join(f"({s}, {c})" for s, c in feats)
        w(f"    ((1 << 22) | ({minor} << 12), &[{cs}], &[{fs}]),\n")
    w("];\n")
    return b.getvalue()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--xml", default=DEFAULT_XML)
    ap.add_argument("--out", default=DEFAULT_OUT)
    ap.add_argument("--check", action="store_true",
                    help="verify the committed output is up to date; do not write")
    args = ap.parse_args()

    hdr, sha, entries, aliases, nested, wsi, resolve = build(args.xml)
    text = (
        render(args.xml, hdr, sha, entries, aliases, nested)
        + render_wsi(wsi)
        + render_resolve(resolve)
    )

    if args.check:
        try:
            cur = open(args.out, encoding="utf-8").read()
        except FileNotFoundError:
            print(f"gen-vk-dispatch: {args.out} is missing", file=sys.stderr)
            sys.exit(1)
        if cur != text:
            print(f"gen-vk-dispatch: {args.out} is stale vs {args.xml}; "
                  f"run tools/gen-vk-dispatch.py", file=sys.stderr)
            sys.exit(1)
        print(f"gen-vk-dispatch: {args.out} up to date "
              f"({len(entries)} commands, {len(aliases)} aliases, "
              f"{len(wsi)} WSI extensions, {len(resolve)} core versions)")
        return

    with open(args.out, "w", encoding="utf-8") as f:
        f.write(text)
    print(f"gen-vk-dispatch: wrote {args.out} "
          f"({len(entries)} commands, {len(aliases)} aliases, {len(wsi)} WSI extensions, "
          f"{len(resolve)} core versions, "
          f"VK_HEADER_VERSION {hdr})")


if __name__ == "__main__":
    main()
