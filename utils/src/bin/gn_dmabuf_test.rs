// Listener vtables are untyped fn-pointer arrays by libwayland design;
// each entry is a deliberately-typed extern "C" fn transmuted once here.
#![allow(clippy::missing_transmute_annotations)]
// File-scope statics accessed via &raw — the static-mut-reference lint's
// recommended idiom; clippy's deref_addrof doesn't recognize it.
#![allow(clippy::deref_addrof)]

//! gn-dmabuf-test — gamescope client-dmabuf import smoke test.
//!
//! Validates the output half of the GameScope guest-driver proxy: a wayland
//! client that renders solid-color frames into dma-buf-exportable vulkan
//! images and presents them to gamescope via zwp_linux_dmabuf_v1 +
//! wl_surface attach/commit — no vkQueuePresentKHR, no WSI surface, exactly
//! how the shim's proxy hands frames to the compositor.
//!
//! Pass = the device screen cycles red/green/blue/white while a gamescope
//! session runs this as the guest command. Frames and milestones are logged
//! to stderr (System.out in logcat).
//!
//! Run as the session guest (container env):
//!   GUEST_PROGRAM_LAUNCHER_COMMAND=<filesDir>/gamescope/bin/gn-dmabuf-test

#![allow(non_snake_case)]

use gn_utils::vk::*;
use gn_utils::wlproto::{self, wl_display, wl_proxy};
use std::ffi::{c_char, c_void, CStr};
use std::ptr;

macro_rules! logf {
    ($fmt:literal $(, $($arg:tt)*)?) => {
        eprintln!("gn-dmabuf-test: {}", format_args!($fmt $(, $($arg)*)?))
    };
}

const DRM_FORMAT_XBGR8888: u32 = 0x34324258; // 'XB24'
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const TEST_WIDTH: u32 = 1280;
const TEST_HEIGHT: u32 = 960;
const NUM_BUFS: usize = 3;
const NUM_FRAMES: usize = 12;

struct Wayland {
    dpy: *mut wl_display,
    compositor: *mut wl_proxy,
    wm_base: *mut wl_proxy,
    dmabuf: *mut wl_proxy,
    surface: *mut wl_proxy,
    xdg_surface: *mut wl_proxy,
    toplevel: *mut wl_proxy,
    configured: bool,
    running: bool,
}

struct Vulkan {
    inst: VkInstance,
    pd: VkPhysicalDevice,
    dev: VkDevice,
    queue: VkQueue,
    pool: VkCommandPool,
    get_memory_fd: Option<PFN_vkGetMemoryFdKHR>,
}

#[derive(Default)]
struct TestBuf {
    image: VkImage,
    mem: VkDeviceMemory,
    fd: i32,
    wlb: *mut wl_proxy,
    free: bool,
    stride: u32,
    offset: u32,
    modifier: u64,
}

// Globals: single-threaded test client (C used file-scope statics; the
// wayland listener callbacks index into BUFS by proxy identity).
static mut WL: Wayland = Wayland {
    dpy: ptr::null_mut(),
    compositor: ptr::null_mut(),
    wm_base: ptr::null_mut(),
    dmabuf: ptr::null_mut(),
    surface: ptr::null_mut(),
    xdg_surface: ptr::null_mut(),
    toplevel: ptr::null_mut(),
    configured: false,
    running: true,
};
static mut VK: Vulkan = Vulkan {
    inst: ptr::null_mut(),
    pd: ptr::null_mut(),
    dev: ptr::null_mut(),
    queue: ptr::null_mut(),
    pool: 0,
    get_memory_fd: None,
};
static mut BUFS: [TestBuf; NUM_BUFS] = [
    TestBuf {
        image: 0,
        mem: 0,
        fd: -1,
        wlb: ptr::null_mut(),
        free: false,
        stride: 0,
        offset: 0,
        modifier: 0,
    },
    TestBuf {
        image: 0,
        mem: 0,
        fd: -1,
        wlb: ptr::null_mut(),
        free: false,
        stride: 0,
        offset: 0,
        modifier: 0,
    },
    TestBuf {
        image: 0,
        mem: 0,
        fd: -1,
        wlb: ptr::null_mut(),
        free: false,
        stride: 0,
        offset: 0,
        modifier: 0,
    },
];

// ---------------------------------------------------------------- wayland

type RawListenerFn = extern "C" fn();

/// The wayland listener callbacks flip this on the dispatch call; an
/// out-of-line read keeps the compiler from folding the loop condition.
#[inline(never)]
fn wl_running() -> bool {
    unsafe { WL.running }
}

/// Same as wl_running, for the initial xdg configure handshake.
#[inline(never)]
fn wl_configured() -> bool {
    unsafe { WL.configured }
}

extern "C" fn registry_global(
    _data: *mut c_void,
    registry: *mut wl_proxy,
    name: u32,
    interface: *const c_char,
    version: u32,
) {
    let iface = unsafe { CStr::from_ptr(interface) }.to_bytes();
    unsafe {
        let wl = &raw mut WL;
        if iface == b"wl_compositor" {
            let v = version.min(4);
            (*wl).compositor =
                wlproto::registry_bind(registry, name, &wlproto::wl_compositor_interface, v);
        } else if iface == b"xdg_wm_base" {
            (*wl).wm_base =
                wlproto::registry_bind(registry, name, &wlproto::XDG_WM_BASE_INTERFACE, 1);
        } else if iface == b"zwp_linux_dmabuf_v1" {
            let v = version.min(4);
            (*wl).dmabuf =
                wlproto::registry_bind(registry, name, &wlproto::ZWP_LINUX_DMABUF_V1_INTERFACE, v);
        }
    }
}

extern "C" fn registry_remove(_data: *mut c_void, _reg: *mut wl_proxy, _name: u32) {}

static REGISTRY_LISTENER: [RawListenerFn; 2] = [
    unsafe {
        std::mem::transmute(
            registry_global as extern "C" fn(*mut c_void, *mut wl_proxy, u32, *const c_char, u32),
        )
    },
    unsafe {
        std::mem::transmute(registry_remove as extern "C" fn(*mut c_void, *mut wl_proxy, u32))
    },
];

extern "C" fn wm_base_ping(_data: *mut c_void, wm_base: *mut wl_proxy, serial: u32) {
    unsafe { wlproto::wm_base_pong(wm_base, serial) };
}

static WM_BASE_LISTENER: [RawListenerFn; 1] = [unsafe {
    std::mem::transmute(wm_base_ping as extern "C" fn(*mut c_void, *mut wl_proxy, u32))
}];

extern "C" fn xdg_surface_configure(_data: *mut c_void, surface: *mut wl_proxy, serial: u32) {
    unsafe {
        wlproto::xdg_surface_ack_configure(surface, serial);
        WL.configured = true;
    }
}

static XDG_SURFACE_LISTENER: [RawListenerFn; 1] = [unsafe {
    std::mem::transmute(xdg_surface_configure as extern "C" fn(*mut c_void, *mut wl_proxy, u32))
}];

extern "C" fn toplevel_configure(
    _data: *mut c_void,
    _toplevel: *mut wl_proxy,
    _w: i32,
    _h: i32,
    _states: *mut wlproto::wl_array,
) {
}

extern "C" fn toplevel_close(_data: *mut c_void, _toplevel: *mut wl_proxy) {
    unsafe { WL.running = false };
}

static TOPLEVEL_LISTENER: [RawListenerFn; 2] = [
    unsafe {
        std::mem::transmute(
            toplevel_configure
                as extern "C" fn(*mut c_void, *mut wl_proxy, i32, i32, *mut wlproto::wl_array),
        )
    },
    unsafe { std::mem::transmute(toplevel_close as extern "C" fn(*mut c_void, *mut wl_proxy)) },
];

extern "C" fn buffer_release(_data: *mut c_void, buffer: *mut wl_proxy) {
    unsafe {
        for i in 0..NUM_BUFS {
            let b = (*&raw mut BUFS).as_mut_ptr().add(i);
            if (*b).wlb == buffer {
                (*b).free = true;
                break;
            }
        }
    }
}

static BUFFER_LISTENER: [RawListenerFn; 1] =
    [unsafe { std::mem::transmute(buffer_release as extern "C" fn(*mut c_void, *mut wl_proxy)) }];

// ----------------------------------------------------------------- vulkan

fn die_vk(what: &str, res: VkResult) -> i32 {
    logf!("FATAL: {what} failed ({res})");
    1
}

fn vulkan_init() -> i32 {
    let app = VkApplicationInfo {
        sType: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        pNext: ptr::null(),
        pApplicationName: c"gn-dmabuf-test".as_ptr(),
        applicationVersion: 0,
        pEngineName: ptr::null(),
        engineVersion: 0,
        apiVersion: VK_API_VERSION_1_3,
    };
    let ici = VkInstanceCreateInfo {
        sType: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        pNext: ptr::null(),
        flags: 0,
        pApplicationInfo: &app,
        enabledLayerCount: 0,
        ppEnabledLayerNames: ptr::null(),
        enabledExtensionCount: 0,
        ppEnabledExtensionNames: ptr::null(),
    };
    unsafe {
        let vk = &raw mut VK;
        let res = vkCreateInstance(&ici, ptr::null(), &mut (*vk).inst);
        if res != VK_SUCCESS {
            return die_vk("vkCreateInstance", res);
        }
        let mut nphys: u32 = 0;
        vkEnumeratePhysicalDevices((*vk).inst, &mut nphys, ptr::null_mut());
        if nphys == 0 {
            logf!("FATAL: no vulkan physical devices");
            return 1;
        }
        let mut phys = vec![ptr::null_mut::<c_void>(); nphys as usize];
        vkEnumeratePhysicalDevices((*vk).inst, &mut nphys, phys.as_mut_ptr());
        (*vk).pd = phys[0];

        let mut props = VkPhysicalDeviceProperties {
            apiVersion: 0,
            driverVersion: 0,
            vendorID: 0,
            deviceID: 0,
            deviceType: 0,
            deviceName: [0; VK_MAX_PHYSICAL_DEVICE_NAME_SIZE],
            pipelineCacheUUID: [0; 16],
            _pad0: [0; 4],
            limits: [0; 504],
            sparseProperties: [0; 20],
            _pad1: [0; 4],
        };
        vkGetPhysicalDeviceProperties((*vk).pd, &mut props);
        logf!(
            "device: {}",
            CStr::from_ptr(props.deviceName.as_ptr()).to_string_lossy()
        );

        let want_exts: [&CStr; 4] = [
            c"VK_KHR_external_memory",
            c"VK_KHR_external_memory_fd",
            c"VK_EXT_external_memory_dma_buf",
            c"VK_EXT_image_drm_format_modifier",
        ];
        let mut next: u32 = 0;
        vkEnumerateDeviceExtensionProperties((*vk).pd, ptr::null(), &mut next, ptr::null_mut());
        let mut exts = vec![
            VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: 0,
            };
            next as usize
        ];
        vkEnumerateDeviceExtensionProperties((*vk).pd, ptr::null(), &mut next, exts.as_mut_ptr());
        for want in want_exts {
            let found = exts[..next as usize]
                .iter()
                .any(|e| CStr::from_ptr(e.extensionName.as_ptr()) == want);
            if !found {
                logf!("FATAL: device lacks {}", want.to_str().unwrap());
                return 1;
            }
        }

        let mut nqf: u32 = 0;
        vkGetPhysicalDeviceQueueFamilyProperties((*vk).pd, &mut nqf, ptr::null_mut());
        let mut qfp = vec![
            VkQueueFamilyProperties {
                queueFlags: 0,
                queueCount: 0,
                timestampValidBits: 0,
                minImageTransferGranularity: VkExtent3D {
                    width: 0,
                    height: 0,
                    depth: 0,
                },
            };
            nqf as usize
        ];
        vkGetPhysicalDeviceQueueFamilyProperties((*vk).pd, &mut nqf, qfp.as_mut_ptr());
        let mut qf = u32::MAX;
        for (i, q) in qfp.iter().enumerate() {
            if q.queueFlags & VK_QUEUE_GRAPHICS_BIT != 0 {
                qf = i as u32;
            }
        }
        if qf == u32::MAX {
            logf!("FATAL: no graphics queue family");
            return 1;
        }

        let prio: f32 = 1.0;
        let qci = VkDeviceQueueCreateInfo {
            sType: VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            queueFamilyIndex: qf,
            queueCount: 1,
            pQueuePriorities: &prio,
        };
        let dev_ext_names: [*const c_char; 4] = [
            c"VK_KHR_external_memory".as_ptr(),
            c"VK_KHR_external_memory_fd".as_ptr(),
            c"VK_EXT_external_memory_dma_buf".as_ptr(),
            c"VK_EXT_image_drm_format_modifier".as_ptr(),
        ];
        let dci = VkDeviceCreateInfo {
            sType: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            queueCreateInfoCount: 1,
            pQueueCreateInfos: &qci,
            enabledLayerCount: 0,
            ppEnabledLayerNames: ptr::null(),
            enabledExtensionCount: dev_ext_names.len() as u32,
            ppEnabledExtensionNames: dev_ext_names.as_ptr(),
            pEnabledFeatures: ptr::null(),
        };
        let res = vkCreateDevice((*vk).pd, &dci, ptr::null(), &mut (*vk).dev);
        if res != VK_SUCCESS {
            return die_vk("vkCreateDevice", res);
        }
        vkGetDeviceQueue((*vk).dev, qf, 0, &mut (*vk).queue);

        let cpci = VkCommandPoolCreateInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            queueFamilyIndex: qf,
        };
        let res = vkCreateCommandPool((*vk).dev, &cpci, ptr::null(), &mut (*vk).pool);
        if res != VK_SUCCESS {
            return die_vk("vkCreateCommandPool", res);
        }
        (*vk).get_memory_fd =
            std::mem::transmute(vkGetDeviceProcAddr((*vk).dev, c"vkGetMemoryFdKHR".as_ptr()));
        if (*vk).get_memory_fd.is_none() {
            logf!("FATAL: vkGetMemoryFdKHR not available");
            return 1;
        }
    }
    0
}

/// Pick the modifier for XBGR8888: prefer LINEAR, else the first with
/// COLOR_ATTACHMENT in its tiling features.
fn pick_modifier() -> u64 {
    unsafe {
        let vk = &raw mut VK;
        let mut modlist = VkDrmFormatModifierPropertiesListEXT {
            sType: VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,
            pNext: ptr::null_mut(),
            drmFormatModifierCount: 0,
            pDrmFormatModifierProperties: ptr::null_mut(),
        };
        let mut fp2 = VkFormatProperties2 {
            sType: VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2,
            pNext: &mut modlist as *mut _ as *mut c_void,
            formatProperties: [0; 12],
        };
        vkGetPhysicalDeviceFormatProperties2((*vk).pd, VK_FORMAT_B8G8R8A8_UNORM, &mut fp2);
        let count = modlist.drmFormatModifierCount;
        if count == 0 {
            logf!("no drm format modifiers reported; assuming LINEAR");
            return DRM_FORMAT_MOD_LINEAR;
        }
        let mut mods = vec![
            VkDrmFormatModifierPropertiesEXT {
                drmFormatModifier: 0,
                drmFormatModifierPlaneCount: 0,
                drmFormatModifierTilingFeatures: 0,
            };
            count as usize
        ];
        modlist.pDrmFormatModifierProperties = mods.as_mut_ptr();
        vkGetPhysicalDeviceFormatProperties2((*vk).pd, VK_FORMAT_B8G8R8A8_UNORM, &mut fp2);
        let count = modlist.drmFormatModifierCount as usize;
        let mut chosen = u64::MAX;
        for m in &mods[..count] {
            if m.drmFormatModifierTilingFeatures & VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT == 0 {
                continue;
            }
            if m.drmFormatModifier == DRM_FORMAT_MOD_LINEAR {
                chosen = DRM_FORMAT_MOD_LINEAR;
                break;
            }
            if chosen == u64::MAX {
                chosen = m.drmFormatModifier;
            }
        }
        if chosen == u64::MAX {
            logf!("WARNING: no color-attachable modifier; trying LINEAR anyway");
            chosen = DRM_FORMAT_MOD_LINEAR;
        }
        logf!(
            "using drm format modifier 0x{chosen:x} ({} planes)",
            if chosen == DRM_FORMAT_MOD_LINEAR {
                1
            } else {
                mods[0].drmFormatModifierPlaneCount as u64
            }
        );
        chosen
    }
}

unsafe fn buffer_create(b: *mut TestBuf, modifier: u64) -> i32 {
    let vk = &raw mut VK;
    unsafe {
        let modinfo = VkImageDrmFormatModifierListCreateInfoEXT {
            sType: VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
            pNext: ptr::null(),
            drmFormatModifierCount: 1,
            pDrmFormatModifiers: &modifier,
        };
        let extimg = VkExternalMemoryImageCreateInfo {
            sType: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
            pNext: &modinfo as *const _ as *const c_void,
            handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        };
        let ici = VkImageCreateInfo {
            sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            pNext: &extimg as *const _ as *const c_void,
            flags: 0,
            imageType: VK_IMAGE_TYPE_2D,
            format: VK_FORMAT_B8G8R8A8_UNORM,
            extent: VkExtent3D {
                width: TEST_WIDTH,
                height: TEST_HEIGHT,
                depth: 1,
            },
            mipLevels: 1,
            arrayLayers: 1,
            samples: VK_SAMPLE_COUNT_1_BIT,
            tiling: VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
            usage: VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
            sharingMode: VK_SHARING_MODE_EXCLUSIVE,
            queueFamilyIndexCount: 0,
            pQueueFamilyIndices: ptr::null(),
            initialLayout: VK_IMAGE_LAYOUT_UNDEFINED,
        };
        let res = vkCreateImage((*vk).dev, &ici, ptr::null(), &mut (*b).image);
        if res != VK_SUCCESS {
            return die_vk("vkCreateImage", res);
        }

        let mut mreq = VkMemoryRequirements {
            size: 0,
            alignment: 0,
            memoryTypeBits: 0,
        };
        vkGetImageMemoryRequirements((*vk).dev, (*b).image, &mut mreq);
        let mut mp = VkPhysicalDeviceMemoryProperties {
            memoryTypeCount: 0,
            memoryTypes: [VkMemoryType {
                propertyFlags: 0,
                heapIndex: 0,
            }; VK_MAX_MEMORY_TYPES],
            memoryHeapCount: 0,
            memoryHeaps: [VkMemoryHeap { size: 0, flags: 0 }; VK_MAX_MEMORY_HEAPS],
        };
        vkGetPhysicalDeviceMemoryProperties((*vk).pd, &mut mp);
        let mut mt = u32::MAX;
        for i in 0..mp.memoryTypeCount {
            if mreq.memoryTypeBits & (1 << i) != 0 {
                mt = i;
            }
        }
        if mt == u32::MAX {
            logf!("FATAL: no memory type for image");
            return 1;
        }

        let expmem = VkExportMemoryAllocateInfo {
            sType: VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
            pNext: ptr::null(),
            handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        };
        let mai = VkMemoryAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            pNext: &expmem as *const _ as *const c_void,
            allocationSize: mreq.size,
            memoryTypeIndex: mt,
        };
        let res = vkAllocateMemory((*vk).dev, &mai, ptr::null(), &mut (*b).mem);
        if res != VK_SUCCESS {
            return die_vk("vkAllocateMemory", res);
        }
        let res = vkBindImageMemory((*vk).dev, (*b).image, (*b).mem, 0);
        if res != VK_SUCCESS {
            return die_vk("vkBindImageMemory", res);
        }

        let fdinfo = VkMemoryGetFdInfoKHR {
            sType: VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
            pNext: ptr::null(),
            memory: (*b).mem,
            handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        };
        let res = (*vk).get_memory_fd.unwrap()((*vk).dev, &fdinfo, &mut (*b).fd);
        if res != VK_SUCCESS {
            return die_vk("vkGetMemoryFdKHR", res);
        }

        let sub = VkImageSubresource {
            aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
            mipLevel: 0,
            arrayLayer: 0,
        };
        let mut layout = VkSubresourceLayout {
            offset: 0,
            size: 0,
            rowPitch: 0,
            arrayPitch: 0,
            depthPitch: 0,
        };
        vkGetImageSubresourceLayout((*vk).dev, (*b).image, &sub, &mut layout);
        (*b).offset = layout.offset as u32;
        (*b).stride = layout.rowPitch as u32;
        (*b).modifier = modifier;

        let wl = &raw mut WL;
        let params = wlproto::dmabuf_create_params((*wl).dmabuf);
        wlproto::params_add(params, (*b).fd, 0, (*b).offset, (*b).stride, (*b).modifier);
        (*b).wlb = wlproto::params_create_immed(
            params,
            TEST_WIDTH as i32,
            TEST_HEIGHT as i32,
            DRM_FORMAT_XBGR8888,
            0,
        );
        wlproto::wl_proxy_destroy(params);
        wlproto::wl_proxy_add_listener(
            (*b).wlb,
            BUFFER_LISTENER.as_ptr() as *mut _,
            ptr::null_mut(),
        );
        (*b).free = true;
        logf!(
            "buffer: stride={} offset={} modifier=0x{:x}",
            (*b).stride,
            (*b).offset,
            (*b).modifier
        );
    }
    0
}

unsafe fn render_clear(b: *mut TestBuf, color: [f32; 4]) -> i32 {
    let vk = &raw mut VK;
    unsafe {
        let mut cmd: VkCommandBuffer = ptr::null_mut();
        let cbai = VkCommandBufferAllocateInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            pNext: ptr::null(),
            commandPool: (*vk).pool,
            level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
            commandBufferCount: 1,
        };
        if vkAllocateCommandBuffers((*vk).dev, &cbai, &mut cmd) != VK_SUCCESS {
            return 1;
        }
        let begin = VkCommandBufferBeginInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            pNext: ptr::null(),
            flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
            pInheritanceInfo: ptr::null(),
        };
        vkBeginCommandBuffer(cmd, &begin);
        let bar = VkImageMemoryBarrier {
            sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            pNext: ptr::null(),
            srcAccessMask: 0,
            dstAccessMask: VK_ACCESS_TRANSFER_WRITE_BIT,
            oldLayout: VK_IMAGE_LAYOUT_UNDEFINED,
            newLayout: VK_IMAGE_LAYOUT_GENERAL,
            srcQueueFamilyIndex: 0,
            dstQueueFamilyIndex: 0,
            image: (*b).image,
            subresourceRange: VkImageSubresourceRange {
                aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                baseMipLevel: 0,
                levelCount: 1,
                baseArrayLayer: 0,
                layerCount: 1,
            },
        };
        vkCmdPipelineBarrier(
            cmd,
            VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
            VK_PIPELINE_STAGE_TRANSFER_BIT,
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            1,
            &bar,
        );
        let cc = VkClearColorValue { float32: color };
        let range = VkImageSubresourceRange {
            aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
            baseMipLevel: 0,
            levelCount: 1,
            baseArrayLayer: 0,
            layerCount: 1,
        };
        vkCmdClearColorImage(cmd, (*b).image, VK_IMAGE_LAYOUT_GENERAL, &cc, 1, &range);
        vkEndCommandBuffer(cmd);
        let submit = VkSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            pNext: ptr::null(),
            waitSemaphoreCount: 0,
            pWaitSemaphores: ptr::null(),
            pWaitDstStageMask: ptr::null(),
            commandBufferCount: 1,
            pCommandBuffers: &cmd,
            signalSemaphoreCount: 0,
            pSignalSemaphores: ptr::null(),
        };
        if vkQueueSubmit((*vk).queue, 1, &submit, 0) != VK_SUCCESS {
            return 1;
        }
        vkQueueWaitIdle((*vk).queue);
        vkFreeCommandBuffers((*vk).dev, (*vk).pool, 1, &cmd);
    }
    0
}

// ------------------------------------------------------------------- main

fn run() -> i32 {
    logf!("starting");
    unsafe {
        let wl = &raw mut WL;
        (*wl).dpy = wlproto::wl_display_connect(ptr::null());
        if (*wl).dpy.is_null() {
            logf!(
                "FATAL: wl_display_connect failed (WAYLAND_DISPLAY={})",
                std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "<unset>".into())
            );
            return 1;
        }
        let registry = wlproto::display_get_registry((*wl).dpy);
        wlproto::wl_proxy_add_listener(
            registry,
            REGISTRY_LISTENER.as_ptr() as *mut _,
            ptr::null_mut(),
        );
        wlproto::wl_display_roundtrip((*wl).dpy);
        wlproto::wl_proxy_destroy(registry);
        if (*wl).compositor.is_null() || (*wl).wm_base.is_null() || (*wl).dmabuf.is_null() {
            logf!(
                "FATAL: missing globals compositor={:?} xdg_wm_base={:?} dmabuf={:?}",
                (*wl).compositor,
                (*wl).wm_base,
                (*wl).dmabuf
            );
            return 1;
        }
        wlproto::wl_proxy_add_listener(
            (*wl).wm_base,
            WM_BASE_LISTENER.as_ptr() as *mut _,
            ptr::null_mut(),
        );

        (*wl).surface = wlproto::compositor_create_surface((*wl).compositor);
        (*wl).xdg_surface = wlproto::wm_base_get_xdg_surface((*wl).wm_base, (*wl).surface);
        wlproto::wl_proxy_add_listener(
            (*wl).xdg_surface,
            XDG_SURFACE_LISTENER.as_ptr() as *mut _,
            ptr::null_mut(),
        );
        (*wl).toplevel = wlproto::xdg_surface_get_toplevel((*wl).xdg_surface);
        wlproto::wl_proxy_add_listener(
            (*wl).toplevel,
            TOPLEVEL_LISTENER.as_ptr() as *mut _,
            ptr::null_mut(),
        );
        wlproto::toplevel_set_title((*wl).toplevel, c"gn-dmabuf-test".as_ptr());
        wlproto::surface_commit((*wl).surface);
        while !wl_configured() {
            if wlproto::wl_display_dispatch((*wl).dpy) < 0 {
                logf!("FATAL: dispatch failed waiting for configure");
                return 1;
            }
        }
        logf!("xdg surface configured");

        if vulkan_init() != 0 {
            return 1;
        }
        let modifier = pick_modifier();
        for i in 0..NUM_BUFS {
            if buffer_create((*&raw mut BUFS).as_mut_ptr().add(i), modifier) != 0 {
                return 1;
            }
        }
        wlproto::wl_display_roundtrip((*wl).dpy);

        const COLORS: [[f32; 4]; 4] = [
            [1.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0, 1.0],
            [0.0, 0.0, 1.0, 1.0],
            [1.0, 1.0, 1.0, 1.0],
        ];
        let mut next = 0usize;
        for frame in 0..NUM_FRAMES {
            if !wl_running() {
                break;
            }
            let mut chosen: *mut TestBuf = ptr::null_mut();
            for tries in 0..NUM_BUFS {
                let idx = (next + tries) % NUM_BUFS;
                if BUFS[idx].free {
                    chosen = (*&raw mut BUFS).as_mut_ptr().add(idx);
                    break;
                }
            }
            if chosen.is_null() {
                wlproto::wl_display_roundtrip((*wl).dpy);
                continue;
            }
            next = (chosen as usize - (*&raw const BUFS).as_ptr() as usize)
                / std::mem::size_of::<TestBuf>()
                + 1;
            let color = COLORS[frame % 4];
            if render_clear(chosen, color) != 0 {
                logf!("FATAL: render failed at frame {frame}");
                return 1;
            }
            (*chosen).free = false;
            wlproto::surface_attach((*wl).surface, (*chosen).wlb, 0, 0);
            wlproto::surface_damage_buffer(
                (*wl).surface,
                0,
                0,
                TEST_WIDTH as i32,
                TEST_HEIGHT as i32,
            );
            wlproto::surface_commit((*wl).surface);
            wlproto::wl_display_flush((*wl).dpy);
            logf!(
                "frame {frame} presented ({:.0} {:.0} {:.0})",
                color[0],
                color[1],
                color[2]
            );
            wlproto::wl_display_roundtrip((*wl).dpy);
            std::thread::sleep(std::time::Duration::from_millis(400));
        }

        logf!(
            "done — {}",
            if (*wl).running {
                "all frames presented"
            } else {
                "closed early"
            }
        );
    }
    0
}

fn main() {
    std::process::exit(run());
}
