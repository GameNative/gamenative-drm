// Listener vtables are untyped fn-pointer arrays by libwayland design;
// each entry is a deliberately-typed extern "C" fn transmuted once here.
#![allow(clippy::missing_transmute_annotations)]

//! gn-wsi-test — gamescope WSI-proxy smoke test.
//!
//! Validates the guest-driver proxy end to end: a plain vulkan WSI client —
//! instance with VK_KHR_wayland_surface, vkCreateWaylandSurfaceKHR, FIFO
//! swapchain, clear-color presents. Inside a GameScope session the ICD shim
//! drives this through the proxy: the guest driver (no wayland WSI of its
//! own) renders into the shim's dma-buf swapchain, and every present is
//! committed onto this client's wl_surface by the proxy's worker.
//!
//! Pass = the device screen cycles red/green/blue/white while a gamescope
//! session runs this as the guest command. Milestones are logged to stderr
//! (System.out in logcat) with the gn-wsi-test tag.
//!
//! Run as the session guest (container env):
//!   GUEST_PROGRAM_LAUNCHER_COMMAND=<filesDir>/gamescope/bin/gn-wsi-test

#![allow(non_snake_case)]

use gn_utils::vk::*;
use gn_utils::wlproto::{self, wl_display, wl_proxy};
use std::ffi::{c_char, c_void, CStr};
use std::ptr;

macro_rules! logf {
    ($fmt:literal $(, $($arg:tt)*)?) => {
        eprintln!("gn-wsi-test: {}", format_args!($fmt $(, $($arg)*)?))
    };
}

const TEST_WIDTH: u32 = 1280;
const TEST_HEIGHT: u32 = 960;
const NUM_FRAMES: usize = 12;

struct Wayland {
    dpy: *mut wl_display,
    compositor: *mut wl_proxy,
    wm_base: *mut wl_proxy,
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
    qf: u32,
    pool: VkCommandPool,
    surface: VkSurfaceKHR,
    swapchain: VkSwapchainKHR,
    sc_format: VkFormat,
    sc_extent: VkExtent2D,
    n_images: u32,
    images: [VkImage; 8],
}

// Single-threaded test client (C used file-scope statics).
static mut WL: Wayland = Wayland {
    dpy: ptr::null_mut(),
    compositor: ptr::null_mut(),
    wm_base: ptr::null_mut(),
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
    qf: 0,
    pool: 0,
    surface: 0,
    swapchain: 0,
    sc_format: 0,
    sc_extent: VkExtent2D {
        width: 0,
        height: 0,
    },
    n_images: 0,
    images: [0; 8],
};

// ---------------------------------------------------------------- wayland

type RawListenerFn = extern "C" fn();

/// The wayland listener callbacks flip this on the dispatch call; an
/// out-of-line read keeps the compiler from folding the loop condition.
#[inline(never)]
fn wl_running() -> bool {
    unsafe { WL.running }
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

fn wayland_init() -> i32 {
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
        let reg = wlproto::display_get_registry((*wl).dpy);
        wlproto::wl_proxy_add_listener(reg, REGISTRY_LISTENER.as_ptr() as *mut _, ptr::null_mut());
        wlproto::wl_display_roundtrip((*wl).dpy);
        wlproto::wl_proxy_destroy(reg);
        if (*wl).compositor.is_null() || (*wl).wm_base.is_null() {
            logf!("FATAL: missing wl_compositor or xdg_wm_base");
            return 1;
        }
        (*wl).surface = wlproto::compositor_create_surface((*wl).compositor);
        wlproto::wl_proxy_add_listener(
            (*wl).wm_base,
            WM_BASE_LISTENER.as_ptr() as *mut _,
            ptr::null_mut(),
        );
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
        wlproto::toplevel_set_title((*wl).toplevel, c"gn-wsi-test".as_ptr());
        wlproto::surface_commit((*wl).surface);
        while !(*wl).configured && wl_running() {
            if wlproto::wl_display_dispatch((*wl).dpy) < 0 {
                break;
            }
        }
        if !(*wl).configured {
            logf!("FATAL: no xdg configure");
            return 1;
        }
        logf!("xdg surface configured");
    }
    0
}

// ----------------------------------------------------------------- vulkan

fn die_vk(what: &str, res: VkResult) -> i32 {
    logf!("FATAL: {what} failed ({res})");
    1
}

fn vulkan_init() -> i32 {
    unsafe {
        let vk = &raw mut VK;
        let mut next: u32 = 0;
        vkEnumerateInstanceExtensionProperties(ptr::null(), &mut next, ptr::null_mut());
        let mut exts = vec![
            VkExtensionProperties {
                extensionName: [0; VK_MAX_EXTENSION_NAME_SIZE],
                specVersion: 0,
            };
            next as usize
        ];
        vkEnumerateInstanceExtensionProperties(ptr::null(), &mut next, exts.as_mut_ptr());
        let have_wayland = exts[..next as usize]
            .iter()
            .any(|e| CStr::from_ptr(e.extensionName.as_ptr()) == c"VK_KHR_wayland_surface");
        logf!(
            "instance extension VK_KHR_wayland_surface: {}",
            if have_wayland { "present" } else { "MISSING" }
        );
        if !have_wayland {
            return 1;
        }

        let enable_exts: [*const c_char; 2] = [
            c"VK_KHR_surface".as_ptr(),
            c"VK_KHR_wayland_surface".as_ptr(),
        ];
        let app = VkApplicationInfo {
            sType: VK_STRUCTURE_TYPE_APPLICATION_INFO,
            pNext: ptr::null(),
            pApplicationName: c"gn-wsi-test".as_ptr(),
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
            enabledExtensionCount: enable_exts.len() as u32,
            ppEnabledExtensionNames: enable_exts.as_ptr(),
        };
        let res = vkCreateInstance(&ici, ptr::null(), &mut (*vk).inst);
        if res != VK_SUCCESS {
            return die_vk("vkCreateInstance", res);
        }

        let wl = &raw mut WL;
        let wci = VkWaylandSurfaceCreateInfoKHR {
            sType: VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR,
            pNext: ptr::null(),
            flags: 0,
            display: (*wl).dpy.cast(),
            surface: (*wl).surface.cast(),
        };
        let res = vkCreateWaylandSurfaceKHR((*vk).inst, &wci, ptr::null(), &mut (*vk).surface);
        if res != VK_SUCCESS {
            return die_vk("vkCreateWaylandSurfaceKHR", res);
        }
        logf!("vulkan wayland surface created");

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
        (*vk).qf = u32::MAX;
        for (i, q) in qfp.iter().enumerate() {
            let mut present = VK_FALSE;
            vkGetPhysicalDeviceSurfaceSupportKHR((*vk).pd, i as u32, (*vk).surface, &mut present);
            if q.queueFlags & VK_QUEUE_GRAPHICS_BIT != 0 && present != 0 {
                (*vk).qf = i as u32;
                break;
            }
        }
        if (*vk).qf == u32::MAX {
            logf!("FATAL: no graphics+present queue family");
            return 1;
        }

        let prio: f32 = 1.0;
        let qci = VkDeviceQueueCreateInfo {
            sType: VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            queueFamilyIndex: (*vk).qf,
            queueCount: 1,
            pQueuePriorities: &prio,
        };
        // GNDRAW_REQ_GPL=1 also asks for the pipeline-library extensions: with
        // the shim's mask in place the guest never gets them (it logs the
        // mask), which is what a DXVK guest sees in Gamescope mode.
        let req_gpl = std::env::var_os("GNDRAW_REQ_GPL").is_some();
        let dev_exts: [*const c_char; 3] = [
            c"VK_KHR_swapchain".as_ptr(),
            c"VK_EXT_graphics_pipeline_library".as_ptr(),
            c"VK_KHR_pipeline_library".as_ptr(),
        ];
        let n_dev_exts = if req_gpl { 3u32 } else { 1u32 };
        let dci = VkDeviceCreateInfo {
            sType: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            queueCreateInfoCount: 1,
            pQueueCreateInfos: &qci,
            enabledLayerCount: 0,
            ppEnabledLayerNames: ptr::null(),
            enabledExtensionCount: n_dev_exts,
            ppEnabledExtensionNames: dev_exts.as_ptr(),
            pEnabledFeatures: ptr::null(),
        };
        let res = vkCreateDevice((*vk).pd, &dci, ptr::null(), &mut (*vk).dev);
        if res != VK_SUCCESS {
            return die_vk("vkCreateDevice", res);
        }
        vkGetDeviceQueue((*vk).dev, (*vk).qf, 0, &mut (*vk).queue);

        let cpci = VkCommandPoolCreateInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
            pNext: ptr::null(),
            flags: VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
            queueFamilyIndex: (*vk).qf,
        };
        let res = vkCreateCommandPool((*vk).dev, &cpci, ptr::null(), &mut (*vk).pool);
        if res != VK_SUCCESS {
            return die_vk("vkCreateCommandPool", res);
        }
    }
    0
}

fn swapchain_init() -> i32 {
    unsafe {
        let vk = &raw mut VK;
        let mut caps = VkSurfaceCapabilitiesKHR {
            minImageCount: 0,
            maxImageCount: 0,
            currentExtent: VkExtent2D {
                width: 0,
                height: 0,
            },
            minImageExtent: VkExtent2D {
                width: 0,
                height: 0,
            },
            maxImageExtent: VkExtent2D {
                width: 0,
                height: 0,
            },
            maxImageArrayLayers: 0,
            supportedTransforms: 0,
            currentTransform: 0,
            supportedCompositeAlpha: 0,
            supportedUsageFlags: 0,
        };
        let res = vkGetPhysicalDeviceSurfaceCapabilitiesKHR((*vk).pd, (*vk).surface, &mut caps);
        if res != VK_SUCCESS {
            return die_vk("vkGetPhysicalDeviceSurfaceCapabilitiesKHR", res);
        }

        let mut nfmt: u32 = 0;
        vkGetPhysicalDeviceSurfaceFormatsKHR((*vk).pd, (*vk).surface, &mut nfmt, ptr::null_mut());
        let mut fmts = vec![
            VkSurfaceFormatKHR {
                format: 0,
                colorSpace: 0,
            };
            nfmt as usize
        ];
        vkGetPhysicalDeviceSurfaceFormatsKHR((*vk).pd, (*vk).surface, &mut nfmt, fmts.as_mut_ptr());
        let mut pick = fmts[0];
        for f in &fmts[..nfmt as usize] {
            if f.format == VK_FORMAT_B8G8R8A8_UNORM {
                pick = *f;
                break;
            }
        }
        (*vk).sc_format = pick.format;
        logf!(
            "swapchain format {} ({nfmt} formats offered)",
            (*vk).sc_format
        );

        let mut ext = caps.currentExtent;
        if ext.width == u32::MAX {
            ext.width = TEST_WIDTH;
            ext.height = TEST_HEIGHT;
        }
        (*vk).sc_extent = ext;

        let sci = VkSwapchainCreateInfoKHR {
            sType: VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
            pNext: ptr::null(),
            flags: 0,
            surface: (*vk).surface,
            minImageCount: caps.minImageCount.max(2),
            imageFormat: pick.format,
            imageColorSpace: pick.colorSpace,
            imageExtent: ext,
            imageArrayLayers: 1,
            // TRANSFER_SRC: the scenes copy the present image back to host memory.
            imageUsage: VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT
                | VK_IMAGE_USAGE_TRANSFER_DST_BIT
                | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
            imageSharingMode: VK_SHARING_MODE_EXCLUSIVE,
            queueFamilyIndexCount: 0,
            pQueueFamilyIndices: ptr::null(),
            preTransform: caps.currentTransform,
            compositeAlpha: VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
            presentMode: VK_PRESENT_MODE_FIFO_KHR,
            clipped: VK_TRUE,
            oldSwapchain: 0,
        };
        let res = vkCreateSwapchainKHR((*vk).dev, &sci, ptr::null(), &mut (*vk).swapchain);
        if res != VK_SUCCESS {
            return die_vk("vkCreateSwapchainKHR", res);
        }
        let res = vkGetSwapchainImagesKHR(
            (*vk).dev,
            (*vk).swapchain,
            &mut (*vk).n_images,
            ptr::null_mut(),
        );
        if res != VK_SUCCESS || (*vk).n_images > 8 {
            return die_vk("vkGetSwapchainImagesKHR(count)", res);
        }
        let res = vkGetSwapchainImagesKHR(
            (*vk).dev,
            (*vk).swapchain,
            &mut (*vk).n_images,
            (*vk).images.as_mut_ptr(),
        );
        if res != VK_SUCCESS {
            return die_vk("vkGetSwapchainImagesKHR", res);
        }
        logf!(
            "swapchain: {}x{}, {} images",
            (*vk).sc_extent.width,
            (*vk).sc_extent.height,
            (*vk).n_images
        );
    }
    0
}

/// One frame: acquire -> clear color -> present. Serialized with
/// vkQueueWaitIdle (1 fps cycle; simplicity over pipelining).
fn present_frame(r: f32, g: f32, b: f32) -> i32 {
    unsafe {
        let vk = &raw mut VK;
        let sci = VkSemaphoreCreateInfo {
            sType: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
        };
        let mut acquire_sem: VkSemaphore = 0;
        let mut render_sem: VkSemaphore = 0;
        let res = vkCreateSemaphore((*vk).dev, &sci, ptr::null(), &mut acquire_sem);
        if res != VK_SUCCESS {
            return die_vk("vkCreateSemaphore(acquire)", res);
        }
        let res = vkCreateSemaphore((*vk).dev, &sci, ptr::null(), &mut render_sem);
        if res != VK_SUCCESS {
            return die_vk("vkCreateSemaphore(render)", res);
        }

        let mut idx: u32 = 0;
        let res = vkAcquireNextImageKHR(
            (*vk).dev,
            (*vk).swapchain,
            3_000_000_000,
            acquire_sem,
            0,
            &mut idx,
        );
        if res != VK_SUCCESS && res != VK_SUBOPTIMAL_KHR {
            return die_vk("vkAcquireNextImageKHR", res);
        }

        let mut cmd: VkCommandBuffer = ptr::null_mut();
        let cai = VkCommandBufferAllocateInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            pNext: ptr::null(),
            commandPool: (*vk).pool,
            level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
            commandBufferCount: 1,
        };
        let res = vkAllocateCommandBuffers((*vk).dev, &cai, &mut cmd);
        if res != VK_SUCCESS {
            return die_vk("vkAllocateCommandBuffers", res);
        }
        let begin = VkCommandBufferBeginInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            pNext: ptr::null(),
            flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
            pInheritanceInfo: ptr::null(),
        };
        vkBeginCommandBuffer(cmd, &begin);
        let range = VkImageSubresourceRange {
            aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
            baseMipLevel: 0,
            levelCount: 1,
            baseArrayLayer: 0,
            layerCount: 1,
        };
        let to_clear = VkImageMemoryBarrier {
            sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            pNext: ptr::null(),
            srcAccessMask: 0,
            dstAccessMask: VK_ACCESS_TRANSFER_WRITE_BIT,
            oldLayout: VK_IMAGE_LAYOUT_UNDEFINED,
            newLayout: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
            dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
            image: (*vk).images[idx as usize],
            subresourceRange: range,
        };
        vkCmdPipelineBarrier(
            cmd,
            VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
            VK_PIPELINE_STAGE_TRANSFER_BIT,
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            1,
            &to_clear,
        );
        let color = VkClearColorValue {
            float32: [r, g, b, 1.0],
        };
        vkCmdClearColorImage(
            cmd,
            (*vk).images[idx as usize],
            VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            &color,
            1,
            &range,
        );
        let to_present = VkImageMemoryBarrier {
            sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            pNext: ptr::null(),
            srcAccessMask: VK_ACCESS_TRANSFER_WRITE_BIT,
            dstAccessMask: 0,
            oldLayout: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            newLayout: VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
            srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
            dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
            image: (*vk).images[idx as usize],
            subresourceRange: range,
        };
        vkCmdPipelineBarrier(
            cmd,
            VK_PIPELINE_STAGE_TRANSFER_BIT,
            VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            1,
            &to_present,
        );
        vkEndCommandBuffer(cmd);

        let wait_stage = VK_PIPELINE_STAGE_TRANSFER_BIT;
        let submit = VkSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            pNext: ptr::null(),
            waitSemaphoreCount: 1,
            pWaitSemaphores: &acquire_sem,
            pWaitDstStageMask: &wait_stage,
            commandBufferCount: 1,
            pCommandBuffers: &cmd,
            signalSemaphoreCount: 1,
            pSignalSemaphores: &render_sem,
        };
        let res = vkQueueSubmit((*vk).queue, 1, &submit, 0);
        if res != VK_SUCCESS {
            return die_vk("vkQueueSubmit", res);
        }

        let pi = VkPresentInfoKHR {
            sType: VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
            pNext: ptr::null(),
            waitSemaphoreCount: 1,
            pWaitSemaphores: &render_sem,
            swapchainCount: 1,
            pSwapchains: &(*vk).swapchain,
            pImageIndices: &idx,
            pResults: ptr::null_mut(),
        };
        let res = vkQueuePresentKHR((*vk).queue, &pi);
        if res != VK_SUCCESS && res != VK_SUBOPTIMAL_KHR {
            return die_vk("vkQueuePresentKHR", res);
        }

        vkQueueWaitIdle((*vk).queue);
        vkDestroySemaphore((*vk).dev, acquire_sem, ptr::null());
        vkDestroySemaphore((*vk).dev, render_sem, ptr::null());
    }
    0
}

// ------------------------------------------------------------------- main

// ------------------------------------------------- draw scenes (GNDRAW_SCENES)
// A controlled client: instead of only clearing, each "scene" exercises one
// class of draw against the proxy swapchain, then copies the present image
// back to host memory and checks the pixels against what the scene drew. The
// verdict is printed, so a class of draw that loses its content in Gamescope
// mode names itself instead of having to be inferred from a game.
//
// Scenes: clear | quad | blend | load | vtx | tex

const SPV_FULL_VERT: &[u8] = include_bytes!("../../shaders/full.vert.spv");
const SPV_RECT_VERT: &[u8] = include_bytes!("../../shaders/rect.vert.spv");
const SPV_COLOUR_FRAG: &[u8] = include_bytes!("../../shaders/colour.frag.spv");
const SPV_TEX_FRAG: &[u8] = include_bytes!("../../shaders/tex.frag.spv");
const SPV_UV_FRAG: &[u8] = include_bytes!("../../shaders/uv.frag.spv");
const SPV_TEXEL_FRAG: &[u8] = include_bytes!("../../shaders/texel.frag.spv");
const SPV_LOD_FRAG: &[u8] = include_bytes!("../../shaders/lod.frag.spv");
const SPV_VB_VERT: &[u8] = include_bytes!("../../shaders/vb.vert.spv");
const SPV_VB_FRAG: &[u8] = include_bytes!("../../shaders/vb.frag.spv");

const NUM_SCENE_FRAMES: usize = 40;
const PUSH_BYTES: u32 = 32;

struct Draw {
    rp_clear: VkRenderPass,
    rp_load: VkRenderPass,
    rp_depth: VkRenderPass,
    fbs: [VkFramebuffer; 8],
    fbs_depth: [VkFramebuffer; 8],
    _views: [VkImageView; 8],
    depth_img: VkImage,
    _depth_view: VkImageView,
    _depth_mem: VkDeviceMemory,
    p_depth: VkPipeline,
    p_cull: VkPipeline,
    p_cull_cw: VkPipeline,
    off_img: VkImage,
    _off_view: VkImageView,
    _off_mem: VkDeviceMemory,
    off_fb: VkFramebuffer,
    p_full: VkPipeline,
    p_rect: VkPipeline,
    p_blend: VkPipeline,
    p_tex: VkPipeline,
    p_uv: VkPipeline,
    p_texel: VkPipeline,
    p_lod: VkPipeline,
    p_vb: VkPipeline,
    layout: VkPipelineLayout,
    build_pool: VkDescriptorPool,
    set_layout: VkDescriptorSetLayout,
    set_tex: VkDescriptorSet,
    readback: VkBuffer,
    readback_mem: VkDeviceMemory,
    vb: VkBuffer,
}

static mut DRAW: Option<Draw> = None;

/// The draw state, reached through a raw pointer: DRAW is written once in
/// draw_init and only read afterwards.
fn draw() -> &'static Draw {
    unsafe {
        let p: *const Option<Draw> = &raw const DRAW;
        (*p).as_ref().unwrap()
    }
}

fn spv_words(b: &[u8]) -> Vec<u32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect()
}

unsafe fn make_shader(code: &[u8]) -> VkShaderModule {
    unsafe {
        let vk = &raw mut VK;
        let words = spv_words(code);
        let ci = VkShaderModuleCreateInfo {
            sType: VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            codeSize: words.len() * 4,
            pCode: words.as_ptr(),
        };
        let mut m: VkShaderModule = 0;
        let res = vkCreateShaderModule((*vk).dev, &ci, ptr::null(), &mut m);
        if res != VK_SUCCESS {
            logf!("vkCreateShaderModule failed: {res}");
            std::process::exit(1);
        }
        m
    }
}

unsafe fn make_render_pass(load_op: VkAttachmentLoadOp) -> VkRenderPass {
    unsafe {
        let vk = &raw mut VK;
        let att = VkAttachmentDescription {
            flags: 0,
            format: (*vk).sc_format,
            samples: VK_SAMPLE_COUNT_1_BIT,
            loadOp: load_op,
            storeOp: VK_ATTACHMENT_STORE_OP_STORE,
            stencilLoadOp: VK_ATTACHMENT_LOAD_OP_DONT_CARE,
            stencilStoreOp: VK_ATTACHMENT_STORE_OP_DONT_CARE,
            initialLayout: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
            finalLayout: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        };
        let refr = VkAttachmentReference {
            attachment: 0,
            layout: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        };
        let sub = VkSubpassDescription {
            flags: 0,
            pipelineBindPoint: VK_PIPELINE_BIND_POINT_GRAPHICS,
            inputAttachmentCount: 0,
            pInputAttachments: ptr::null(),
            colorAttachmentCount: 1,
            pColorAttachments: &refr,
            pResolveAttachments: ptr::null(),
            pDepthStencilAttachment: ptr::null(),
            preserveAttachmentCount: 0,
            pPreserveAttachments: ptr::null(),
        };
        let dep = VkSubpassDependency {
            srcSubpass: u32::MAX,
            dstSubpass: 0,
            srcStageMask: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT
                | VK_PIPELINE_STAGE_TRANSFER_BIT,
            dstStageMask: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
            srcAccessMask: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT | VK_ACCESS_TRANSFER_WRITE_BIT,
            dstAccessMask: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
            dependencyFlags: 0,
        };
        let ci = VkRenderPassCreateInfo {
            sType: VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            attachmentCount: 1,
            pAttachments: &att,
            subpassCount: 1,
            pSubpasses: &sub,
            dependencyCount: 1,
            pDependencies: &dep,
        };
        let mut rp: VkRenderPass = 0;
        let res = vkCreateRenderPass((*vk).dev, &ci, ptr::null(), &mut rp);
        if res != VK_SUCCESS {
            logf!("vkCreateRenderPass failed: {res}");
            std::process::exit(1);
        }
        rp
    }
}

/// One graphics pipeline; every scene shares the render pass and layout.
/// Colour + depth attachment render pass: the depth path the game uses.
unsafe fn make_depth_pass(colour_format: VkFormat) -> VkRenderPass {
    unsafe {
        let vk = &raw mut VK;
        let atts = [
            VkAttachmentDescription {
                flags: 0,
                format: colour_format,
                samples: VK_SAMPLE_COUNT_1_BIT,
                loadOp: VK_ATTACHMENT_LOAD_OP_CLEAR,
                storeOp: VK_ATTACHMENT_STORE_OP_STORE,
                stencilLoadOp: VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                stencilStoreOp: VK_ATTACHMENT_STORE_OP_DONT_CARE,
                initialLayout: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                finalLayout: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
            },
            VkAttachmentDescription {
                flags: 0,
                format: VK_FORMAT_D24_UNORM_S8_UINT,
                samples: VK_SAMPLE_COUNT_1_BIT,
                loadOp: VK_ATTACHMENT_LOAD_OP_CLEAR,
                storeOp: VK_ATTACHMENT_STORE_OP_DONT_CARE,
                stencilLoadOp: VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                stencilStoreOp: VK_ATTACHMENT_STORE_OP_DONT_CARE,
                initialLayout: VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
                finalLayout: VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
            },
        ];
        let colour_ref = VkAttachmentReference {
            attachment: 0,
            layout: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        };
        let depth_ref = VkAttachmentReference {
            attachment: 1,
            layout: VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
        };
        let sub = VkSubpassDescription {
            flags: 0,
            pipelineBindPoint: VK_PIPELINE_BIND_POINT_GRAPHICS,
            inputAttachmentCount: 0,
            pInputAttachments: ptr::null(),
            colorAttachmentCount: 1,
            pColorAttachments: &colour_ref,
            pResolveAttachments: ptr::null(),
            pDepthStencilAttachment: &depth_ref,
            preserveAttachmentCount: 0,
            pPreserveAttachments: ptr::null(),
        };
        let dep = VkSubpassDependency {
            srcSubpass: u32::MAX,
            dstSubpass: 0,
            srcStageMask: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT
                | VK_PIPELINE_STAGE_EARLY_FRAGMENT_TESTS_BIT,
            dstStageMask: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT
                | VK_PIPELINE_STAGE_EARLY_FRAGMENT_TESTS_BIT
                | VK_PIPELINE_STAGE_LATE_FRAGMENT_TESTS_BIT,
            srcAccessMask: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT
                | VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT,
            dstAccessMask: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT
                | VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT,
            dependencyFlags: 0,
        };
        let ci = VkRenderPassCreateInfo {
            sType: VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            attachmentCount: 2,
            pAttachments: atts.as_ptr(),
            subpassCount: 1,
            pSubpasses: &sub,
            dependencyCount: 1,
            pDependencies: &dep,
        };
        let mut rp: VkRenderPass = 0;
        let res = vkCreateRenderPass((*vk).dev, &ci, ptr::null(), &mut rp);
        if res != VK_SUCCESS {
            logf!("vkCreateRenderPass(depth) failed: {res}");
            std::process::exit(1);
        }
        rp
    }
}

// Diagnostic helper: a flag per pipeline state axis is clearer here than a
// packed struct, at the cost of arity.
#[allow(clippy::too_many_arguments)]
unsafe fn make_pipeline(
    rp: VkRenderPass,
    layout: VkPipelineLayout,
    vert: VkShaderModule,
    frag: VkShaderModule,
    use_vb: bool,
    blend: bool,
    depth: bool,
    cull: VkCullModeFlags,
    front: VkFrontFace,
) -> VkPipeline {
    unsafe {
        let vk = &raw mut VK;
        let stages = [
            VkPipelineShaderStageCreateInfo {
                sType: VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                stage: VK_SHADER_STAGE_VERTEX_BIT,
                module: vert,
                pName: c"main".as_ptr(),
                pSpecializationInfo: ptr::null(),
            },
            VkPipelineShaderStageCreateInfo {
                sType: VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                stage: VK_SHADER_STAGE_FRAGMENT_BIT,
                module: frag,
                pName: c"main".as_ptr(),
                pSpecializationInfo: ptr::null(),
            },
        ];
        let binding = VkVertexInputBindingDescription {
            binding: 0,
            stride: 24,
            inputRate: VK_VERTEX_INPUT_RATE_VERTEX,
        };
        let attrs = [
            VkVertexInputAttributeDescription {
                location: 0,
                binding: 0,
                format: VK_FORMAT_R32G32_SFLOAT,
                offset: 0,
            },
            VkVertexInputAttributeDescription {
                location: 1,
                binding: 0,
                format: VK_FORMAT_R32G32B32A32_SFLOAT,
                offset: 8,
            },
        ];
        let vi = VkPipelineVertexInputStateCreateInfo {
            sType: VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            vertexBindingDescriptionCount: if use_vb { 1 } else { 0 },
            pVertexBindingDescriptions: if use_vb { &binding } else { ptr::null() },
            vertexAttributeDescriptionCount: if use_vb { 2 } else { 0 },
            pVertexAttributeDescriptions: if use_vb { attrs.as_ptr() } else { ptr::null() },
        };
        let ia = VkPipelineInputAssemblyStateCreateInfo {
            sType: VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            topology: VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP,
            primitiveRestartEnable: VK_FALSE,
        };
        let vs = VkPipelineViewportStateCreateInfo {
            sType: VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            viewportCount: 1,
            pViewports: ptr::null(),
            scissorCount: 1,
            pScissors: ptr::null(),
        };
        let rs = VkPipelineRasterizationStateCreateInfo {
            sType: VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            depthClampEnable: VK_FALSE,
            rasterizerDiscardEnable: VK_FALSE,
            polygonMode: VK_POLYGON_MODE_FILL,
            cullMode: cull,
            frontFace: front,
            depthBiasEnable: VK_FALSE,
            depthBiasConstantFactor: 0.0,
            depthBiasClamp: 0.0,
            depthBiasSlopeFactor: 0.0,
            lineWidth: 1.0,
        };
        let ms = VkPipelineMultisampleStateCreateInfo {
            sType: VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            rasterizationSamples: VK_SAMPLE_COUNT_1_BIT,
            sampleShadingEnable: VK_FALSE,
            minSampleShading: 1.0,
            pSampleMask: ptr::null(),
            alphaToCoverageEnable: VK_FALSE,
            alphaToOneEnable: VK_FALSE,
        };
        let ds = VkPipelineDepthStencilStateCreateInfo {
            sType: VK_STRUCTURE_TYPE_PIPELINE_DEPTH_STENCIL_STATE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            depthTestEnable: if depth { VK_TRUE } else { VK_FALSE },
            depthWriteEnable: if depth { VK_TRUE } else { VK_FALSE },
            depthCompareOp: if depth {
                VK_COMPARE_OP_LESS_OR_EQUAL
            } else {
                VK_COMPARE_OP_NEVER
            },
            depthBoundsTestEnable: VK_FALSE,
            stencilTestEnable: VK_FALSE,
            front: VkStencilOpState {
                failOp: VK_STENCIL_OP_KEEP,
                passOp: VK_STENCIL_OP_KEEP,
                depthFailOp: VK_STENCIL_OP_KEEP,
                compareOp: VK_COMPARE_OP_NEVER,
                compareMask: 0,
                writeMask: 0,
                reference: 0,
            },
            back: VkStencilOpState {
                failOp: VK_STENCIL_OP_KEEP,
                passOp: VK_STENCIL_OP_KEEP,
                depthFailOp: VK_STENCIL_OP_KEEP,
                compareOp: VK_COMPARE_OP_NEVER,
                compareMask: 0,
                writeMask: 0,
                reference: 0,
            },
            minDepthBounds: 0.0,
            maxDepthBounds: 1.0,
        };
        let cba = VkPipelineColorBlendAttachmentState {
            blendEnable: if blend { VK_TRUE } else { VK_FALSE },
            srcColorBlendFactor: if blend {
                VK_BLEND_FACTOR_SRC_ALPHA
            } else {
                VK_BLEND_FACTOR_ONE
            },
            dstColorBlendFactor: if blend {
                VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA
            } else {
                VK_BLEND_FACTOR_ONE
            },
            colorBlendOp: VK_BLEND_OP_ADD,
            srcAlphaBlendFactor: VK_BLEND_FACTOR_ONE,
            dstAlphaBlendFactor: VK_BLEND_FACTOR_ONE,
            alphaBlendOp: VK_BLEND_OP_ADD,
            colorWriteMask: VK_COLOR_COMPONENT_R_BIT
                | VK_COLOR_COMPONENT_G_BIT
                | VK_COLOR_COMPONENT_B_BIT
                | VK_COLOR_COMPONENT_A_BIT,
        };
        let cb = VkPipelineColorBlendStateCreateInfo {
            sType: VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            logicOpEnable: VK_FALSE,
            logicOp: VK_LOGIC_OP_CLEAR,
            attachmentCount: 1,
            pAttachments: &cba,
            blendConstants: [0.0; 4],
        };
        let dyn_states = [VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR];
        let dy = VkPipelineDynamicStateCreateInfo {
            sType: VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            dynamicStateCount: dyn_states.len() as u32,
            pDynamicStates: dyn_states.as_ptr(),
        };
        let gp = VkGraphicsPipelineCreateInfo {
            sType: VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            stageCount: 2,
            pStages: stages.as_ptr(),
            pVertexInputState: &vi,
            pInputAssemblyState: &ia,
            pTessellationState: ptr::null(),
            pViewportState: &vs,
            pRasterizationState: &rs,
            pMultisampleState: &ms,
            pDepthStencilState: &ds,
            pColorBlendState: &cb,
            pDynamicState: &dy,
            layout,
            renderPass: rp,
            subpass: 0,
            basePipelineHandle: 0,
            basePipelineIndex: -1,
        };
        let mut p: VkPipeline = 0;
        let res = vkCreateGraphicsPipelines((*vk).dev, 0, 1, &gp, ptr::null(), &mut p);
        if res != VK_SUCCESS {
            logf!("vkCreateGraphicsPipelines failed: {res}");
            std::process::exit(1);
        }
        p
    }
}

/// Builds everything the scenes share: render passes, per-image framebuffers,
/// pipelines, a push-constant layout, a 2x2 texture, a vertex buffer and the
/// host-visible readback buffer.
unsafe fn draw_init() -> i32 {
    unsafe {
        let vk = &raw mut VK;
        let rp_clear = make_render_pass(VK_ATTACHMENT_LOAD_OP_CLEAR);
        let rp_load = make_render_pass(VK_ATTACHMENT_LOAD_OP_LOAD);

        let pcr = VkPushConstantRange {
            stageFlags: VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT,
            offset: 0,
            size: PUSH_BYTES,
        };
        let dsl_bind = VkDescriptorSetLayoutBinding {
            binding: 0,
            descriptorType: VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
            descriptorCount: 1,
            stageFlags: VK_SHADER_STAGE_FRAGMENT_BIT,
            pImmutableSamplers: ptr::null(),
        };
        let dsl_ci = VkDescriptorSetLayoutCreateInfo {
            sType: VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            bindingCount: 1,
            pBindings: &dsl_bind,
        };
        let mut set_layout: VkDescriptorSetLayout = 0;
        let res = vkCreateDescriptorSetLayout((*vk).dev, &dsl_ci, ptr::null(), &mut set_layout);
        if res != VK_SUCCESS {
            logf!("vkCreateDescriptorSetLayout failed: {res}");
            return 1;
        }
        let plci = VkPipelineLayoutCreateInfo {
            sType: VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            setLayoutCount: 1,
            pSetLayouts: &set_layout,
            pushConstantRangeCount: 1,
            pPushConstantRanges: &pcr,
        };
        let mut layout: VkPipelineLayout = 0;
        let res = vkCreatePipelineLayout((*vk).dev, &plci, ptr::null(), &mut layout);
        if res != VK_SUCCESS {
            logf!("vkCreatePipelineLayout failed: {res}");
            return 1;
        }

        // Framebuffers: one per swapchain image (a view each).
        let mut fbs: [VkFramebuffer; 8] = [0; 8];
        let mut views: [VkImageView; 8] = [0; 8];
        for (i, slot) in fbs.iter_mut().enumerate() {
            if i >= (*vk).n_images as usize {
                break;
            }
            let range = VkImageSubresourceRange {
                aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                baseMipLevel: 0,
                levelCount: 1,
                baseArrayLayer: 0,
                layerCount: 1,
            };
            let vci = VkImageViewCreateInfo {
                sType: VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                image: (*vk).images[i],
                viewType: VK_IMAGE_VIEW_TYPE_2D,
                format: (*vk).sc_format,
                components: VkComponentMapping {
                    r: VK_COMPONENT_SWIZZLE_IDENTITY,
                    g: VK_COMPONENT_SWIZZLE_IDENTITY,
                    b: VK_COMPONENT_SWIZZLE_IDENTITY,
                    a: VK_COMPONENT_SWIZZLE_IDENTITY,
                },
                subresourceRange: range,
            };
            let mut view: VkImageView = 0;
            let res = vkCreateImageView((*vk).dev, &vci, ptr::null(), &mut view);
            if res != VK_SUCCESS {
                logf!("vkCreateImageView({i}) failed: {res}");
                return 1;
            }
            let fci = VkFramebufferCreateInfo {
                sType: VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                renderPass: rp_clear,
                attachmentCount: 1,
                pAttachments: &view,
                width: (*vk).sc_extent.width,
                height: (*vk).sc_extent.height,
                layers: 1,
            };
            let res = vkCreateFramebuffer((*vk).dev, &fci, ptr::null(), slot);
            if res != VK_SUCCESS {
                logf!("vkCreateFramebuffer({i}) failed: {res}");
                return 1;
            }
            views[i] = view;
        }

        // Host-visible buffers: readback (frame copy) and the vertex buffer.
        let want = VkPhysicalDeviceMemoryProperties {
            memoryTypeCount: 0,
            memoryTypes: [VkMemoryType {
                propertyFlags: 0,
                heapIndex: 0,
            }; 32],
            memoryHeapCount: 0,
            memoryHeaps: [VkMemoryHeap { size: 0, flags: 0 }; 16],
        };
        let mut mprops = want;
        vkGetPhysicalDeviceMemoryProperties((*vk).pd, &mut mprops);

        let make_host_buffer =
            |size: u64, usage: VkBufferUsageFlags| -> (VkBuffer, VkDeviceMemory) {
                let bci = VkBufferCreateInfo {
                    sType: VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                    pNext: ptr::null(),
                    flags: 0,
                    size,
                    usage,
                    sharingMode: VK_SHARING_MODE_EXCLUSIVE,
                    queueFamilyIndexCount: 0,
                    pQueueFamilyIndices: ptr::null(),
                };
                let mut buf: VkBuffer = 0;
                let res = vkCreateBuffer((*vk).dev, &bci, ptr::null(), &mut buf);
                if res != VK_SUCCESS {
                    logf!("vkCreateBuffer failed: {res}");
                    std::process::exit(1);
                }
                let mut reqs = VkMemoryRequirements {
                    size: 0,
                    alignment: 0,
                    memoryTypeBits: 0,
                };
                vkGetBufferMemoryRequirements((*vk).dev, buf, &mut reqs);
                let want_bits =
                    VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
                let mut mt = u32::MAX;
                for i in 0..mprops.memoryTypeCount {
                    if reqs.memoryTypeBits & (1 << i) != 0
                        && mprops.memoryTypes[i as usize].propertyFlags & want_bits == want_bits
                    {
                        mt = i;
                        break;
                    }
                }
                if mt == u32::MAX {
                    logf!("no host-visible memory type");
                    std::process::exit(1);
                }
                let ai = VkMemoryAllocateInfo {
                    sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                    pNext: ptr::null(),
                    allocationSize: reqs.size,
                    memoryTypeIndex: mt,
                };
                let mut mem: VkDeviceMemory = 0;
                let res = vkAllocateMemory((*vk).dev, &ai, ptr::null(), &mut mem);
                if res != VK_SUCCESS {
                    logf!("vkAllocateMemory failed: {res}");
                    std::process::exit(1);
                }
                vkBindBufferMemory((*vk).dev, buf, mem, 0);
                (buf, mem)
            };

        let (readback, readback_mem) = make_host_buffer(
            ((*vk).sc_extent.width * (*vk).sc_extent.height * 4) as u64,
            VK_BUFFER_USAGE_TRANSFER_DST_BIT,
        );

        // Vertex buffer: 4 vertices, pos (vec2) + colour (vec4).
        #[rustfmt::skip]
        let verts: [f32; 24] = [
            -1.0, -1.0,  1.0, 0.0, 0.0, 1.0,   // bottom-left  red
             1.0, -1.0,  0.0, 1.0, 0.0, 1.0,   // bottom-right green
            -1.0,  1.0,  0.0, 0.0, 1.0, 1.0,   // top-left     blue
             1.0,  1.0,  1.0, 1.0, 1.0, 1.0,   // top-right    white
        ];
        let (vb, vb_mem) = make_host_buffer(96, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT);
        let mut map: *mut c_void = ptr::null_mut();
        vkMapMemory((*vk).dev, vb_mem, 0, 96, 0, &mut map);
        ptr::copy_nonoverlapping(verts.as_ptr(), map as *mut f32, 24);
        vkUnmapMemory((*vk).dev, vb_mem);

        // Texture: 2x2 RGBA8 checkerboard, uploaded from a host buffer.
        let tex_w = 2u32;
        let tex_h = 2u32;
        let ici = VkImageCreateInfo {
            sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            imageType: VK_IMAGE_TYPE_2D,
            format: VK_FORMAT_R8G8B8A8_UNORM,
            extent: VkExtent3D {
                width: tex_w,
                height: tex_h,
                depth: 1,
            },
            mipLevels: 1,
            arrayLayers: 1,
            samples: VK_SAMPLE_COUNT_1_BIT,
            tiling: VK_IMAGE_TILING_LINEAR,
            usage: VK_IMAGE_USAGE_SAMPLED_BIT
                | VK_IMAGE_USAGE_TRANSFER_DST_BIT
                | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
            sharingMode: VK_SHARING_MODE_EXCLUSIVE,
            queueFamilyIndexCount: 0,
            pQueueFamilyIndices: ptr::null(),
            initialLayout: VK_IMAGE_LAYOUT_UNDEFINED,
        };
        let mut tex: VkImage = 0;
        let res = vkCreateImage((*vk).dev, &ici, ptr::null(), &mut tex);
        if res != VK_SUCCESS {
            logf!("vkCreateImage(tex) failed: {res}");
            return 1;
        }
        let mut treqs = VkMemoryRequirements {
            size: 0,
            alignment: 0,
            memoryTypeBits: 0,
        };
        vkGetImageMemoryRequirements((*vk).dev, tex, &mut treqs);
        let mut tmt = u32::MAX;
        for i in 0..mprops.memoryTypeCount {
            if treqs.memoryTypeBits & (1 << i) != 0 {
                tmt = i;
                break;
            }
        }
        let tai = VkMemoryAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            pNext: ptr::null(),
            allocationSize: treqs.size,
            memoryTypeIndex: tmt,
        };
        let mut tmem: VkDeviceMemory = 0;
        let res = vkAllocateMemory((*vk).dev, &tai, ptr::null(), &mut tmem);
        if res != VK_SUCCESS {
            logf!("vkAllocateMemory(tex) failed: {res}");
            return 1;
        }
        vkBindImageMemory((*vk).dev, tex, tmem, 0);

        // 2x2: (0,0) white, (1,0) black, (0,1) black, (1,1) white
        #[rustfmt::skip]
        let texels: [u8; 16] = [
            255, 255, 255, 255,   0,   0,   0, 255,
              0,   0,   0, 255, 255, 255, 255, 255,
        ];
        let (staging, staging_mem) = make_host_buffer(16, VK_BUFFER_USAGE_TRANSFER_SRC_BIT);
        let mut smap: *mut c_void = ptr::null_mut();
        let smres = vkMapMemory((*vk).dev, staging_mem, 0, 16, 0, &mut smap);
        if smres == VK_SUCCESS {
            ptr::copy_nonoverlapping(texels.as_ptr(), smap as *mut u8, 16);
            // Read the mapping straight back before unmapping: separates a
            // failed host write from a failed device-side copy.
            let mut back = [0u8; 16];
            ptr::copy_nonoverlapping(smap as *const u8, back.as_mut_ptr(), 16);
            logf!("staging write: map={smres} bytes={back:?}");
            vkUnmapMemory((*vk).dev, staging_mem);
        } else {
            logf!("staging write: vkMapMemory failed ({smres})");
        }

        let tex_view_range = VkImageSubresourceRange {
            aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
            baseMipLevel: 0,
            levelCount: 1,
            baseArrayLayer: 0,
            layerCount: 1,
        };
        let tvci = VkImageViewCreateInfo {
            sType: VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            image: tex,
            viewType: VK_IMAGE_VIEW_TYPE_2D,
            format: VK_FORMAT_R8G8B8A8_UNORM,
            components: VkComponentMapping {
                r: VK_COMPONENT_SWIZZLE_IDENTITY,
                g: VK_COMPONENT_SWIZZLE_IDENTITY,
                b: VK_COMPONENT_SWIZZLE_IDENTITY,
                a: VK_COMPONENT_SWIZZLE_IDENTITY,
            },
            subresourceRange: tex_view_range,
        };
        let mut tex_view: VkImageView = 0;
        vkCreateImageView((*vk).dev, &tvci, ptr::null(), &mut tex_view);
        let sci = VkSamplerCreateInfo {
            sType: VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            magFilter: VK_FILTER_NEAREST,
            minFilter: VK_FILTER_NEAREST,
            mipmapMode: VK_SAMPLER_MIPMAP_MODE_NEAREST,
            addressModeU: VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
            addressModeV: VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
            addressModeW: VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
            mipLodBias: 0.0,
            anisotropyEnable: VK_FALSE,
            maxAnisotropy: 1.0,
            compareEnable: VK_FALSE,
            compareOp: VK_COMPARE_OP_NEVER,
            minLod: 0.0,
            maxLod: 0.0,
            borderColor: 0,
            unnormalizedCoordinates: VK_FALSE,
        };
        let mut sampler: VkSampler = 0;
        let res = vkCreateSampler((*vk).dev, &sci, ptr::null(), &mut sampler);
        if res != VK_SUCCESS {
            logf!("vkCreateSampler failed: {res}");
            return 1;
        }

        let psize = VkDescriptorPoolSize {
            type_: VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
            descriptorCount: 8,
        };
        let dpci = VkDescriptorPoolCreateInfo {
            sType: VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            maxSets: 8,
            poolSizeCount: 1,
            pPoolSizes: &psize,
        };
        let mut dpool: VkDescriptorPool = 0;
        let res = vkCreateDescriptorPool((*vk).dev, &dpci, ptr::null(), &mut dpool);
        if res != VK_SUCCESS {
            logf!("vkCreateDescriptorPool failed: {res}");
            return 1;
        }
        let dsai = VkDescriptorSetAllocateInfo {
            sType: VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
            pNext: ptr::null(),
            descriptorPool: dpool,
            descriptorSetCount: 1,
            pSetLayouts: &set_layout,
        };
        let mut set_tex: VkDescriptorSet = 0;
        let res = vkAllocateDescriptorSets((*vk).dev, &dsai, &mut set_tex);
        if res != VK_SUCCESS {
            logf!("vkAllocateDescriptorSets failed: {res}");
            return 1;
        }
        let dii = VkDescriptorImageInfo {
            sampler,
            imageView: tex_view,
            imageLayout: VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
        };
        let wds = VkWriteDescriptorSet {
            sType: VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
            pNext: ptr::null(),
            dstSet: set_tex,
            dstBinding: 0,
            dstArrayElement: 0,
            descriptorCount: 1,
            descriptorType: VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
            pImageInfo: &dii,
            pBufferInfo: ptr::null(),
            pTexelBufferView: ptr::null(),
        };
        vkUpdateDescriptorSets((*vk).dev, 1, &wds, 0, ptr::null());

        // Upload the texture and leave it sampled: one throwaway command buffer.
        {
            let cai = VkCommandBufferAllocateInfo {
                sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                pNext: ptr::null(),
                commandPool: (*vk).pool,
                level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                commandBufferCount: 1,
            };
            let mut cmd: VkCommandBuffer = ptr::null_mut();
            vkAllocateCommandBuffers((*vk).dev, &cai, &mut cmd);
            let begin = VkCommandBufferBeginInfo {
                sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                pNext: ptr::null(),
                flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
                pInheritanceInfo: ptr::null(),
            };
            vkBeginCommandBuffer(cmd, &begin);
            let to_dst = VkImageMemoryBarrier {
                sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                pNext: ptr::null(),
                srcAccessMask: 0,
                dstAccessMask: VK_ACCESS_TRANSFER_WRITE_BIT,
                oldLayout: VK_IMAGE_LAYOUT_UNDEFINED,
                newLayout: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                image: tex,
                subresourceRange: tex_view_range,
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
                &to_dst,
            );
            let copy = VkBufferImageCopy {
                bufferOffset: 0,
                bufferRowLength: 0,
                bufferImageHeight: 0,
                imageSubresource: VkImageSubresourceLayers {
                    aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                    mipLevel: 0,
                    baseArrayLayer: 0,
                    layerCount: 1,
                },
                imageOffset: VkOffset3D { x: 0, y: 0, z: 0 },
                imageExtent: VkExtent3D {
                    width: tex_w,
                    height: tex_h,
                    depth: 1,
                },
            };
            vkCmdCopyBufferToImage(
                cmd,
                staging,
                tex,
                VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                1,
                &copy,
            );
            let to_sampled = VkImageMemoryBarrier {
                sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                pNext: ptr::null(),
                srcAccessMask: VK_ACCESS_TRANSFER_WRITE_BIT,
                dstAccessMask: VK_ACCESS_SHADER_READ_BIT,
                oldLayout: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                newLayout: VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                image: tex,
                subresourceRange: tex_view_range,
            };
            vkCmdPipelineBarrier(
                cmd,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                0,
                0,
                ptr::null(),
                0,
                ptr::null(),
                1,
                &to_sampled,
            );
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
            vkQueueSubmit((*vk).queue, 1, &submit, 0);
            vkQueueWaitIdle((*vk).queue);
            vkFreeCommandBuffers((*vk).dev, (*vk).pool, 1, &cmd);
        }

        // Self-check: read the uploaded texels back through the same readback
        // path the scenes use. A non-imported image, so this also says whether
        // the readback mechanism itself works.
        {
            let cai = VkCommandBufferAllocateInfo {
                sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                pNext: ptr::null(),
                commandPool: (*vk).pool,
                level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                commandBufferCount: 1,
            };
            let mut cmd: VkCommandBuffer = ptr::null_mut();
            vkAllocateCommandBuffers((*vk).dev, &cai, &mut cmd);
            let begin = VkCommandBufferBeginInfo {
                sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                pNext: ptr::null(),
                flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
                pInheritanceInfo: ptr::null(),
            };
            vkBeginCommandBuffer(cmd, &begin);
            let t = Transition {
                from: VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                to: VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                src_access: VK_ACCESS_SHADER_READ_BIT,
                dst_access: VK_ACCESS_TRANSFER_READ_BIT,
                src_stage: VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                dst_stage: VK_PIPELINE_STAGE_TRANSFER_BIT,
            };
            vkCmdPipelineBarrier(
                cmd,
                t.src_stage,
                t.dst_stage,
                0,
                0,
                ptr::null(),
                0,
                ptr::null(),
                1,
                &VkImageMemoryBarrier {
                    sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                    pNext: ptr::null(),
                    srcAccessMask: t.src_access,
                    dstAccessMask: t.dst_access,
                    oldLayout: t.from,
                    newLayout: t.to,
                    srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                    dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                    image: tex,
                    subresourceRange: tex_view_range,
                },
            );
            let copy = VkBufferImageCopy {
                bufferOffset: 0,
                bufferRowLength: 0,
                bufferImageHeight: 0,
                imageSubresource: VkImageSubresourceLayers {
                    aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                    mipLevel: 0,
                    baseArrayLayer: 0,
                    layerCount: 1,
                },
                imageOffset: VkOffset3D { x: 0, y: 0, z: 0 },
                imageExtent: VkExtent3D {
                    width: tex_w,
                    height: tex_h,
                    depth: 1,
                },
            };
            vkCmdCopyImageToBuffer(
                cmd,
                tex,
                VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                readback,
                1,
                &copy,
            );
            vkCmdPipelineBarrier(
                cmd,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                0,
                0,
                ptr::null(),
                0,
                ptr::null(),
                1,
                &VkImageMemoryBarrier {
                    sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                    pNext: ptr::null(),
                    srcAccessMask: VK_ACCESS_TRANSFER_READ_BIT,
                    dstAccessMask: VK_ACCESS_SHADER_READ_BIT,
                    oldLayout: VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                    newLayout: VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                    srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                    dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                    image: tex,
                    subresourceRange: tex_view_range,
                },
            );
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
            let res = vkQueueSubmit((*vk).queue, 1, &submit, 0);
            vkQueueWaitIdle((*vk).queue);
            let mut base: *mut c_void = ptr::null_mut();
            let mres = vkMapMemory((*vk).dev, readback_mem, 0, 0, 0, &mut base);
            if res == VK_SUCCESS && mres == VK_SUCCESS {
                let p = base as *const u8;
                let mut got = [0u8; 16];
                ptr::copy_nonoverlapping(p, got.as_mut_ptr(), 16);
                logf!("texture self-check: submit={res} map={mres} texels={got:?}");
            } else {
                logf!("texture self-check: submit={res} map={mres} (no readback)");
            }
            if mres == VK_SUCCESS {
                vkUnmapMemory((*vk).dev, readback_mem);
            }
            vkFreeCommandBuffers((*vk).dev, (*vk).pool, 1, &cmd);
        }

        // Transfer probe: which transfer ops actually produce results here.
        //   1. buffer -> buffer                      (readback[0..16])
        //   2. clear the LINEAR image, copy it out  (readback[16..32])
        //   3. same upload into an OPTIMAL image    (readback[32..48])
        {
            let o_w = tex_w;
            let o_h = tex_h;
            let o_ici = VkImageCreateInfo {
                sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                imageType: VK_IMAGE_TYPE_2D,
                format: VK_FORMAT_R8G8B8A8_UNORM,
                extent: VkExtent3D {
                    width: o_w,
                    height: o_h,
                    depth: 1,
                },
                mipLevels: 1,
                arrayLayers: 1,
                samples: VK_SAMPLE_COUNT_1_BIT,
                tiling: VK_IMAGE_TILING_OPTIMAL,
                usage: VK_IMAGE_USAGE_SAMPLED_BIT
                    | VK_IMAGE_USAGE_TRANSFER_DST_BIT
                    | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
                sharingMode: VK_SHARING_MODE_EXCLUSIVE,
                queueFamilyIndexCount: 0,
                pQueueFamilyIndices: ptr::null(),
                initialLayout: VK_IMAGE_LAYOUT_UNDEFINED,
            };
            let mut o_tex: VkImage = 0;
            let o_res = vkCreateImage((*vk).dev, &o_ici, ptr::null(), &mut o_tex);
            let mut o_reqs = VkMemoryRequirements {
                size: 0,
                alignment: 0,
                memoryTypeBits: 0,
            };
            let mut o_mem: VkDeviceMemory = 0;
            let mut o_bind = -1;
            if o_res == VK_SUCCESS {
                vkGetImageMemoryRequirements((*vk).dev, o_tex, &mut o_reqs);
                let mut ot = u32::MAX;
                for i in 0..mprops.memoryTypeCount {
                    if o_reqs.memoryTypeBits & (1 << i) != 0 {
                        ot = i;
                        break;
                    }
                }
                let oai = VkMemoryAllocateInfo {
                    sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                    pNext: ptr::null(),
                    allocationSize: o_reqs.size,
                    memoryTypeIndex: ot,
                };
                if vkAllocateMemory((*vk).dev, &oai, ptr::null(), &mut o_mem) == VK_SUCCESS {
                    o_bind = vkBindImageMemory((*vk).dev, o_tex, o_mem, 0);
                }
            }
            logf!(
                "probe: optimal image create={o_res} typebits={:#x} bind={o_bind}",
                o_reqs.memoryTypeBits
            );

            if o_res == VK_SUCCESS && o_bind == VK_SUCCESS {
                let cai = VkCommandBufferAllocateInfo {
                    sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                    pNext: ptr::null(),
                    commandPool: (*vk).pool,
                    level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                    commandBufferCount: 1,
                };
                let mut cmd: VkCommandBuffer = ptr::null_mut();
                vkAllocateCommandBuffers((*vk).dev, &cai, &mut cmd);
                let begin = VkCommandBufferBeginInfo {
                    sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                    pNext: ptr::null(),
                    flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
                    pInheritanceInfo: ptr::null(),
                };
                vkBeginCommandBuffer(cmd, &begin);
                let region = VkBufferCopy {
                    srcOffset: 0,
                    dstOffset: 0,
                    size: 16,
                };
                vkCmdCopyBuffer(cmd, staging, readback, 1, &region);

                let range = VkImageSubresourceRange {
                    aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                    baseMipLevel: 0,
                    levelCount: 1,
                    baseArrayLayer: 0,
                    layerCount: 1,
                };
                let t = Transition {
                    from: VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                    to: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                    src_access: VK_ACCESS_SHADER_READ_BIT,
                    dst_access: VK_ACCESS_TRANSFER_WRITE_BIT,
                    src_stage: VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                    dst_stage: VK_PIPELINE_STAGE_TRANSFER_BIT,
                };
                vkCmdPipelineBarrier(
                    cmd,
                    t.src_stage,
                    t.dst_stage,
                    0,
                    0,
                    ptr::null(),
                    0,
                    ptr::null(),
                    1,
                    &VkImageMemoryBarrier {
                        sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                        pNext: ptr::null(),
                        srcAccessMask: t.src_access,
                        dstAccessMask: t.dst_access,
                        oldLayout: t.from,
                        newLayout: t.to,
                        srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        image: tex,
                        subresourceRange: range,
                    },
                );
                let k = VkClearColorValue {
                    float32: [0.25, 0.5, 0.75, 1.0],
                };
                vkCmdClearColorImage(
                    cmd,
                    tex,
                    VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                    &k,
                    1,
                    &range,
                );
                vkCmdPipelineBarrier(
                    cmd,
                    VK_PIPELINE_STAGE_TRANSFER_BIT,
                    VK_PIPELINE_STAGE_TRANSFER_BIT,
                    0,
                    0,
                    ptr::null(),
                    0,
                    ptr::null(),
                    1,
                    &VkImageMemoryBarrier {
                        sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                        pNext: ptr::null(),
                        srcAccessMask: VK_ACCESS_TRANSFER_WRITE_BIT,
                        dstAccessMask: VK_ACCESS_TRANSFER_READ_BIT,
                        oldLayout: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                        newLayout: VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                        srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        image: tex,
                        subresourceRange: range,
                    },
                );
                let mut r2 = VkBufferImageCopy {
                    bufferOffset: 16,
                    bufferRowLength: 0,
                    bufferImageHeight: 0,
                    imageSubresource: VkImageSubresourceLayers {
                        aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                        mipLevel: 0,
                        baseArrayLayer: 0,
                        layerCount: 1,
                    },
                    imageOffset: VkOffset3D { x: 0, y: 0, z: 0 },
                    imageExtent: VkExtent3D {
                        width: o_w,
                        height: o_h,
                        depth: 1,
                    },
                };
                vkCmdCopyImageToBuffer(
                    cmd,
                    tex,
                    VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                    readback,
                    1,
                    &r2,
                );
                vkCmdPipelineBarrier(
                    cmd,
                    VK_PIPELINE_STAGE_TRANSFER_BIT,
                    VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                    0,
                    0,
                    ptr::null(),
                    0,
                    ptr::null(),
                    1,
                    &VkImageMemoryBarrier {
                        sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                        pNext: ptr::null(),
                        srcAccessMask: VK_ACCESS_TRANSFER_READ_BIT,
                        dstAccessMask: VK_ACCESS_SHADER_READ_BIT,
                        oldLayout: VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                        newLayout: VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                        srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        image: tex,
                        subresourceRange: range,
                    },
                );

                // 3) OPTIMAL image: same upload, then copy it straight out.
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
                    &VkImageMemoryBarrier {
                        sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                        pNext: ptr::null(),
                        srcAccessMask: 0,
                        dstAccessMask: VK_ACCESS_TRANSFER_WRITE_BIT,
                        oldLayout: VK_IMAGE_LAYOUT_UNDEFINED,
                        newLayout: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                        srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        image: o_tex,
                        subresourceRange: range,
                    },
                );
                r2.bufferOffset = 32;
                vkCmdCopyBufferToImage(
                    cmd,
                    staging,
                    o_tex,
                    VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                    1,
                    &r2,
                );
                vkCmdPipelineBarrier(
                    cmd,
                    VK_PIPELINE_STAGE_TRANSFER_BIT,
                    VK_PIPELINE_STAGE_TRANSFER_BIT,
                    0,
                    0,
                    ptr::null(),
                    0,
                    ptr::null(),
                    1,
                    &VkImageMemoryBarrier {
                        sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                        pNext: ptr::null(),
                        srcAccessMask: VK_ACCESS_TRANSFER_WRITE_BIT,
                        dstAccessMask: VK_ACCESS_TRANSFER_READ_BIT,
                        oldLayout: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                        newLayout: VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                        srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        image: o_tex,
                        subresourceRange: range,
                    },
                );
                let mut r3 = r2;
                r3.bufferOffset = 32;
                vkCmdCopyImageToBuffer(
                    cmd,
                    o_tex,
                    VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                    readback,
                    1,
                    &r3,
                );
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
                let res = vkQueueSubmit((*vk).queue, 1, &submit, 0);
                vkQueueWaitIdle((*vk).queue);
                vkFreeCommandBuffers((*vk).dev, (*vk).pool, 1, &cmd);
                let mut base: *mut c_void = ptr::null_mut();
                if vkMapMemory((*vk).dev, readback_mem, 0, 0, 0, &mut base) == VK_SUCCESS {
                    let p = base as *const u8;
                    let mut got = [0u8; 48];
                    ptr::copy_nonoverlapping(p, got.as_mut_ptr(), 48);
                    logf!("probe submit={res} buffer->buffer={:?}", &got[0..16]);
                    logf!(
                        "probe linear clear+copyout={:?} (want 64,128,191,255 x4)",
                        &got[16..32]
                    );
                    logf!("probe optimal upload+copyout={:?}", &got[32..48]);
                    vkUnmapMemory((*vk).dev, readback_mem);
                }
            }
        }

        // Depth attachment (D24S8, like the guest images the game creates).
        let mut depth_img: VkImage = 0;
        let mut depth_mem: VkDeviceMemory = 0;
        let mut depth_view: VkImageView = 0;
        {
            let ici = VkImageCreateInfo {
                sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                imageType: VK_IMAGE_TYPE_2D,
                format: VK_FORMAT_D24_UNORM_S8_UINT,
                extent: VkExtent3D {
                    width: (*vk).sc_extent.width,
                    height: (*vk).sc_extent.height,
                    depth: 1,
                },
                mipLevels: 1,
                arrayLayers: 1,
                samples: VK_SAMPLE_COUNT_1_BIT,
                tiling: VK_IMAGE_TILING_OPTIMAL,
                usage: VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT,
                sharingMode: VK_SHARING_MODE_EXCLUSIVE,
                queueFamilyIndexCount: 0,
                pQueueFamilyIndices: ptr::null(),
                initialLayout: VK_IMAGE_LAYOUT_UNDEFINED,
            };
            let res = vkCreateImage((*vk).dev, &ici, ptr::null(), &mut depth_img);
            let mut reqs = std::mem::zeroed::<VkMemoryRequirements>();
            let mut bres = -1;
            let mut mt = u32::MAX;
            if res == VK_SUCCESS {
                vkGetImageMemoryRequirements((*vk).dev, depth_img, &mut reqs);
                for i in 0..mprops.memoryTypeCount {
                    let f = mprops.memoryTypes[i as usize].propertyFlags;
                    if reqs.memoryTypeBits & (1 << i) != 0
                        && f & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT != 0
                        && f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT == 0
                    {
                        mt = i;
                        break;
                    }
                }
                if mt == u32::MAX {
                    for i in 0..mprops.memoryTypeCount {
                        if reqs.memoryTypeBits & (1 << i) != 0 {
                            mt = i;
                            break;
                        }
                    }
                }
                let ai = VkMemoryAllocateInfo {
                    sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                    pNext: ptr::null(),
                    allocationSize: reqs.size,
                    memoryTypeIndex: mt,
                };
                if vkAllocateMemory((*vk).dev, &ai, ptr::null(), &mut depth_mem) == VK_SUCCESS {
                    bres = vkBindImageMemory((*vk).dev, depth_img, depth_mem, 0);
                }
            }
            logf!("depth image: create={res} bind={bres} memtype={mt}");
            let vci = VkImageViewCreateInfo {
                sType: VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                image: depth_img,
                viewType: VK_IMAGE_VIEW_TYPE_2D,
                format: VK_FORMAT_D24_UNORM_S8_UINT,
                components: VkComponentMapping {
                    r: VK_COMPONENT_SWIZZLE_IDENTITY,
                    g: VK_COMPONENT_SWIZZLE_IDENTITY,
                    b: VK_COMPONENT_SWIZZLE_IDENTITY,
                    a: VK_COMPONENT_SWIZZLE_IDENTITY,
                },
                subresourceRange: VkImageSubresourceRange {
                    aspectMask: VK_IMAGE_ASPECT_DEPTH_BIT,
                    baseMipLevel: 0,
                    levelCount: 1,
                    baseArrayLayer: 0,
                    layerCount: 1,
                },
            };
            vkCreateImageView((*vk).dev, &vci, ptr::null(), &mut depth_view);
        }
        let rp_depth = make_depth_pass((*vk).sc_format);
        let mut fbs_depth: [VkFramebuffer; 8] = [0; 8];
        for (i, slot) in fbs_depth.iter_mut().enumerate() {
            if i >= (*vk).n_images as usize {
                break;
            }
            let atts = [views[i], depth_view];
            let fci = VkFramebufferCreateInfo {
                sType: VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                renderPass: rp_depth,
                attachmentCount: 2,
                pAttachments: atts.as_ptr(),
                width: (*vk).sc_extent.width,
                height: (*vk).sc_extent.height,
                layers: 1,
            };
            let res = vkCreateFramebuffer((*vk).dev, &fci, ptr::null(), slot);
            if res != VK_SUCCESS {
                logf!("vkCreateFramebuffer(depth {i}) failed: {res}");
                return 1;
            }
        }

        // Offscreen colour target (320x240) for the blit scene.
        let mut off_img: VkImage = 0;
        let mut off_mem: VkDeviceMemory = 0;
        let mut off_view: VkImageView = 0;
        let mut off_fb: VkFramebuffer = 0;
        {
            let ici = VkImageCreateInfo {
                sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                imageType: VK_IMAGE_TYPE_2D,
                format: (*vk).sc_format,
                extent: VkExtent3D {
                    width: 320,
                    height: 240,
                    depth: 1,
                },
                mipLevels: 1,
                arrayLayers: 1,
                samples: VK_SAMPLE_COUNT_1_BIT,
                tiling: VK_IMAGE_TILING_OPTIMAL,
                usage: VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
                sharingMode: VK_SHARING_MODE_EXCLUSIVE,
                queueFamilyIndexCount: 0,
                pQueueFamilyIndices: ptr::null(),
                initialLayout: VK_IMAGE_LAYOUT_UNDEFINED,
            };
            let res = vkCreateImage((*vk).dev, &ici, ptr::null(), &mut off_img);
            let mut reqs = std::mem::zeroed::<VkMemoryRequirements>();
            let mut bres = -1;
            let mut mt = u32::MAX;
            if res == VK_SUCCESS {
                vkGetImageMemoryRequirements((*vk).dev, off_img, &mut reqs);
                for i in 0..mprops.memoryTypeCount {
                    let f = mprops.memoryTypes[i as usize].propertyFlags;
                    if reqs.memoryTypeBits & (1 << i) != 0
                        && f & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT != 0
                        && f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT == 0
                    {
                        mt = i;
                        break;
                    }
                }
                if mt == u32::MAX {
                    mt = 0;
                }
                let ai = VkMemoryAllocateInfo {
                    sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                    pNext: ptr::null(),
                    allocationSize: reqs.size,
                    memoryTypeIndex: mt,
                };
                if vkAllocateMemory((*vk).dev, &ai, ptr::null(), &mut off_mem) == VK_SUCCESS {
                    bres = vkBindImageMemory((*vk).dev, off_img, off_mem, 0);
                }
            }
            logf!("offscreen image: create={res} bind={bres} memtype={mt}");
            let vci = VkImageViewCreateInfo {
                sType: VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                image: off_img,
                viewType: VK_IMAGE_VIEW_TYPE_2D,
                format: (*vk).sc_format,
                components: VkComponentMapping {
                    r: VK_COMPONENT_SWIZZLE_IDENTITY,
                    g: VK_COMPONENT_SWIZZLE_IDENTITY,
                    b: VK_COMPONENT_SWIZZLE_IDENTITY,
                    a: VK_COMPONENT_SWIZZLE_IDENTITY,
                },
                subresourceRange: VkImageSubresourceRange {
                    aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                    baseMipLevel: 0,
                    levelCount: 1,
                    baseArrayLayer: 0,
                    layerCount: 1,
                },
            };
            vkCreateImageView((*vk).dev, &vci, ptr::null(), &mut off_view);
            let fci = VkFramebufferCreateInfo {
                sType: VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                renderPass: rp_clear,
                attachmentCount: 1,
                pAttachments: &off_view,
                width: 320,
                height: 240,
                layers: 1,
            };
            vkCreateFramebuffer((*vk).dev, &fci, ptr::null(), &mut off_fb);
        }

        let vert_full = make_shader(SPV_FULL_VERT);
        let vert_rect = make_shader(SPV_RECT_VERT);
        let vert_vb = make_shader(SPV_VB_VERT);
        let frag_colour = make_shader(SPV_COLOUR_FRAG);
        let frag_tex = make_shader(SPV_TEX_FRAG);
        let frag_vb = make_shader(SPV_VB_FRAG);

        let p_full = make_pipeline(
            rp_clear,
            layout,
            vert_full,
            frag_colour,
            false,
            false,
            false,
            VK_CULL_MODE_NONE,
            VK_FRONT_FACE_COUNTER_CLOCKWISE,
        );
        let p_rect = make_pipeline(
            rp_clear,
            layout,
            vert_rect,
            frag_colour,
            false,
            false,
            false,
            VK_CULL_MODE_NONE,
            VK_FRONT_FACE_COUNTER_CLOCKWISE,
        );
        let p_blend = make_pipeline(
            rp_clear,
            layout,
            vert_rect,
            frag_colour,
            false,
            true,
            false,
            VK_CULL_MODE_NONE,
            VK_FRONT_FACE_COUNTER_CLOCKWISE,
        );
        let frag_uv = make_shader(SPV_UV_FRAG);
        let frag_texel = make_shader(SPV_TEXEL_FRAG);
        let p_tex = make_pipeline(
            rp_clear,
            layout,
            vert_rect,
            frag_tex,
            false,
            false,
            false,
            VK_CULL_MODE_NONE,
            VK_FRONT_FACE_COUNTER_CLOCKWISE,
        );
        let p_uv = make_pipeline(
            rp_clear,
            layout,
            vert_rect,
            frag_uv,
            false,
            false,
            false,
            VK_CULL_MODE_NONE,
            VK_FRONT_FACE_COUNTER_CLOCKWISE,
        );
        let p_texel = make_pipeline(
            rp_clear,
            layout,
            vert_rect,
            frag_texel,
            false,
            false,
            false,
            VK_CULL_MODE_NONE,
            VK_FRONT_FACE_COUNTER_CLOCKWISE,
        );
        let frag_lod = make_shader(SPV_LOD_FRAG);
        let p_lod = make_pipeline(
            rp_clear,
            layout,
            vert_rect,
            frag_lod,
            false,
            false,
            false,
            VK_CULL_MODE_NONE,
            VK_FRONT_FACE_COUNTER_CLOCKWISE,
        );
        let p_depth = make_pipeline(
            rp_depth,
            layout,
            vert_rect,
            frag_colour,
            false,
            false,
            true,
            VK_CULL_MODE_NONE,
            VK_FRONT_FACE_COUNTER_CLOCKWISE,
        );
        let p_vb = make_pipeline(
            rp_clear,
            layout,
            vert_vb,
            frag_vb,
            true,
            false,
            false,
            VK_CULL_MODE_NONE,
            VK_FRONT_FACE_COUNTER_CLOCKWISE,
        );
        let p_cull = make_pipeline(
            rp_clear,
            layout,
            vert_rect,
            frag_colour,
            false,
            false,
            false,
            VK_CULL_MODE_BACK_BIT,
            VK_FRONT_FACE_COUNTER_CLOCKWISE,
        );
        let p_cull_cw = make_pipeline(
            rp_clear,
            layout,
            vert_rect,
            frag_colour,
            false,
            false,
            false,
            VK_CULL_MODE_BACK_BIT,
            VK_FRONT_FACE_CLOCKWISE,
        );

        DRAW = Some(Draw {
            rp_clear,
            rp_load,
            rp_depth,
            fbs_depth,
            _views: views,
            depth_img,
            _depth_view: depth_view,
            _depth_mem: depth_mem,
            p_depth,
            p_cull,
            p_cull_cw,
            off_img,
            _off_view: off_view,
            _off_mem: off_mem,
            off_fb,
            fbs,
            p_full,
            p_rect,
            p_blend,
            p_tex,
            p_uv,
            p_texel,
            p_vb,
            layout,
            build_pool: dpool,
            set_layout,
            set_tex,
            p_lod,
            readback,
            readback_mem,
            vb,
        });
    }
    0
}

struct Transition {
    from: VkImageLayout,
    to: VkImageLayout,
    src_access: VkAccessFlags,
    dst_access: VkAccessFlags,
    src_stage: VkPipelineStageFlags,
    dst_stage: VkPipelineStageFlags,
}

unsafe fn barrier_img(cmd: VkCommandBuffer, img: VkImage, t: Transition) {
    unsafe {
        let b = VkImageMemoryBarrier {
            sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            pNext: ptr::null(),
            srcAccessMask: t.src_access,
            dstAccessMask: t.dst_access,
            oldLayout: t.from,
            newLayout: t.to,
            srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
            dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
            image: img,
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
            t.src_stage,
            t.dst_stage,
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            1,
            &b,
        );
    }
}

unsafe fn begin_pass(cmd: VkCommandBuffer, load: bool, fb: VkFramebuffer, clear: [f32; 4]) {
    unsafe {
        let d = draw();
        let vk = &raw mut VK;
        let cv = VkClearValue {
            color: VkClearColorValue { float32: clear },
        };
        let bi = VkRenderPassBeginInfo {
            sType: VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
            pNext: ptr::null(),
            renderPass: if load { d.rp_load } else { d.rp_clear },
            framebuffer: fb,
            renderArea: VkRect2D {
                offset: VkOffset2D { x: 0, y: 0 },
                extent: (*vk).sc_extent,
            },
            clearValueCount: if load { 0 } else { 1 },
            pClearValues: if load { ptr::null() } else { &cv },
        };
        vkCmdBeginRenderPass(cmd, &bi, VK_SUBPASS_CONTENTS_INLINE);
    }
}

unsafe fn push_rect(cmd: VkCommandBuffer, rect: [f32; 4], colour: [f32; 4]) {
    unsafe {
        let d = draw();
        let vals: [f32; 8] = [
            rect[0], rect[1], rect[2], rect[3], colour[0], colour[1], colour[2], colour[3],
        ];
        vkCmdPushConstants(
            cmd,
            d.layout,
            VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT,
            0,
            PUSH_BYTES,
            vals.as_ptr() as *const c_void,
        );
    }
}

unsafe fn record_scene(cmd: VkCommandBuffer, scene: &str, idx: u32, last: bool) {
    unsafe {
        let d = draw();
        let vk = &raw mut VK;
        let img = (*vk).images[idx as usize];
        let fb = d.fbs[idx as usize];
        let vp = VkViewport {
            x: 0.0,
            y: 0.0,
            width: (*vk).sc_extent.width as f32,
            height: (*vk).sc_extent.height as f32,
            minDepth: 0.0,
            maxDepth: 1.0,
        };
        let scissor = VkRect2D {
            offset: VkOffset2D { x: 0, y: 0 },
            extent: (*vk).sc_extent,
        };
        let BLACK = [0.0, 0.0, 0.0, 1.0];
        let HALF = [-1.0, -1.0, 1.0, 2.0];
        let OTHER_HALF = [0.0, -1.0, 1.0, 2.0];
        let FULL = [-1.0, -1.0, 2.0, 2.0];

        barrier_img(
            cmd,
            img,
            Transition {
                from: VK_IMAGE_LAYOUT_UNDEFINED,
                to: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                src_access: 0,
                dst_access: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                src_stage: VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                dst_stage: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
            },
        );

        match scene {
            "clear" => {
                // Transfer clear: the path the original smoke test proved.
                barrier_img(
                    cmd,
                    img,
                    Transition {
                        from: VK_IMAGE_LAYOUT_UNDEFINED,
                        to: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                        src_access: 0,
                        dst_access: VK_ACCESS_TRANSFER_WRITE_BIT,
                        src_stage: VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        dst_stage: VK_PIPELINE_STAGE_TRANSFER_BIT,
                    },
                );
                let col = VkClearColorValue {
                    float32: [0.1, 0.4, 1.0, 1.0],
                };
                vkCmdClearColorImage(
                    cmd,
                    img,
                    VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                    &col,
                    1,
                    &VkImageSubresourceRange {
                        aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                        baseMipLevel: 0,
                        levelCount: 1,
                        baseArrayLayer: 0,
                        layerCount: 1,
                    },
                );
                barrier_img(
                    cmd,
                    img,
                    Transition {
                        from: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                        to: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                        src_access: VK_ACCESS_TRANSFER_WRITE_BIT,
                        dst_access: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                        src_stage: VK_PIPELINE_STAGE_TRANSFER_BIT,
                        dst_stage: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                    },
                );
            }
            "rpclear" => {
                // Render-pass clear: attachment path only.
                begin_pass(cmd, false, fb, [0.9, 0.1, 0.6, 1.0]);
                vkCmdEndRenderPass(cmd);
            }
            "cull" => {
                // Face culling + winding, both controls, same viewport and
                // geometry: left uses frontFace=CCW (front-facing, must draw),
                // right uses frontFace=CW with cullMode=BACK (the same
                // triangle is then back-facing and must be culled).
                begin_pass(cmd, false, fb, BLACK);
                let half_w = (*vk).sc_extent.width / 2;
                let full_h = (*vk).sc_extent.height;
                let vp_full = VkViewport {
                    x: 0.0,
                    y: 0.0,
                    width: (*vk).sc_extent.width as f32,
                    height: full_h as f32,
                    minDepth: 0.0,
                    maxDepth: 1.0,
                };
                let sc_left = VkRect2D {
                    offset: VkOffset2D { x: 0, y: 0 },
                    extent: VkExtent2D {
                        width: half_w,
                        height: full_h,
                    },
                };
                let sc_right = VkRect2D {
                    offset: VkOffset2D {
                        x: half_w as i32,
                        y: 0,
                    },
                    extent: VkExtent2D {
                        width: half_w,
                        height: full_h,
                    },
                };
                vkCmdSetViewport(cmd, 0, 1, &vp_full);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_cull);
                vkCmdSetScissor(cmd, 0, 1, &sc_left);
                push_rect(cmd, FULL, [0.8, 0.2, 0.2, 1.0]);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_cull_cw);
                vkCmdSetScissor(cmd, 0, 1, &sc_right);
                push_rect(cmd, FULL, [0.8, 0.2, 0.2, 1.0]);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                vkCmdEndRenderPass(cmd);
            }
            "yflip" => {
                // Negative-height viewport: does the driver honour it? The vtx
                // quad has distinct corner colours (bl red, br green, tl blue,
                // tr white), so where RED lands says whether the flip applied:
                // bottom-left = ignored, top-left = honoured.
                begin_pass(cmd, false, fb, BLACK);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_vb);
                let vp_flip = VkViewport {
                    x: 0.0,
                    y: (*vk).sc_extent.height as f32,
                    width: (*vk).sc_extent.width as f32,
                    height: -((*vk).sc_extent.height as f32),
                    minDepth: 0.0,
                    maxDepth: 1.0,
                };
                vkCmdSetViewport(cmd, 0, 1, &vp_flip);
                vkCmdSetScissor(cmd, 0, 1, &scissor);
                let offs: VkDeviceSize = 0;
                vkCmdBindVertexBuffers(cmd, 0, 1, &d.vb, &offs);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                vkCmdEndRenderPass(cmd);
            }
            "resize" | "quad" => {
                begin_pass(cmd, false, fb, BLACK);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_full);
                vkCmdSetViewport(cmd, 0, 1, &vp);
                vkCmdSetScissor(cmd, 0, 1, &scissor);
                push_rect(cmd, FULL, [0.8, 0.2, 0.2, 1.0]);
                vkCmdDraw(cmd, 3, 1, 0, 0);
                vkCmdEndRenderPass(cmd);
            }
            "blend" => {
                begin_pass(cmd, false, fb, [0.0, 0.6, 0.0, 1.0]);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_blend);
                vkCmdSetViewport(cmd, 0, 1, &vp);
                vkCmdSetScissor(cmd, 0, 1, &scissor);
                push_rect(cmd, HALF, [1.0, 0.0, 0.0, 0.5]);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                vkCmdEndRenderPass(cmd);
            }
            "load" => {
                begin_pass(cmd, false, fb, BLACK);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_rect);
                vkCmdSetViewport(cmd, 0, 1, &vp);
                vkCmdSetScissor(cmd, 0, 1, &scissor);
                push_rect(cmd, HALF, [1.0, 1.0, 1.0, 1.0]);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                vkCmdEndRenderPass(cmd);
                // Second pass keeps what the first one wrote.
                begin_pass(cmd, true, fb, BLACK);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_rect);
                vkCmdSetViewport(cmd, 0, 1, &vp);
                vkCmdSetScissor(cmd, 0, 1, &scissor);
                push_rect(cmd, OTHER_HALF, [1.0, 0.0, 0.0, 1.0]);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                vkCmdEndRenderPass(cmd);
            }
            "vtx" => {
                begin_pass(cmd, false, fb, BLACK);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_vb);
                vkCmdSetViewport(cmd, 0, 1, &vp);
                vkCmdSetScissor(cmd, 0, 1, &scissor);
                let offs: VkDeviceSize = 0;
                vkCmdBindVertexBuffers(cmd, 0, 1, &d.vb, &offs);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                vkCmdEndRenderPass(cmd);
            }
            "bigtex" | "bigtex_bgra" | "bigtex_srgb" => {
                // OPTIMAL, 8 mips, uploaded one level per copy: left half
                // reads LOD 0, right half LOD 1.
                let fmt = match scene {
                    "bigtex_bgra" => VK_FORMAT_B8G8R8A8_UNORM,
                    "bigtex_srgb" => VK_FORMAT_R8G8B8A8_SRGB,
                    _ => VK_FORMAT_R8G8B8A8_UNORM,
                };
                match bigtex(fmt) {
                    Some(t) => {
                        begin_pass(cmd, false, fb, BLACK);
                        vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_lod);
                        vkCmdSetViewport(cmd, 0, 1, &vp);
                        vkCmdSetScissor(cmd, 0, 1, &scissor);
                        let sets = [t.set];
                        vkCmdBindDescriptorSets(
                            cmd,
                            VK_PIPELINE_BIND_POINT_GRAPHICS,
                            d.layout,
                            0,
                            1,
                            sets.as_ptr(),
                            0,
                            ptr::null(),
                        );
                        push_rect(cmd, HALF, [0.0, 0.0, 0.0, 0.0]);
                        vkCmdDraw(cmd, 4, 1, 0, 0);
                        push_rect(cmd, OTHER_HALF, [1.0, 0.0, 0.0, 0.0]);
                        vkCmdDraw(cmd, 4, 1, 0, 0);
                        vkCmdEndRenderPass(cmd);
                    }
                    None => logf!("scene {scene}: texture build failed"),
                }
            }
            "depth" => {
                // Near quad (z=0.25) drawn first, far quad (z=0.5) second:
                // LEQUAL with depth writes must keep the near one.
                let drange = VkImageSubresourceRange {
                    aspectMask: VK_IMAGE_ASPECT_DEPTH_BIT,
                    baseMipLevel: 0,
                    levelCount: 1,
                    baseArrayLayer: 0,
                    layerCount: 1,
                };
                barrier_img(
                    cmd,
                    img,
                    Transition {
                        from: VK_IMAGE_LAYOUT_UNDEFINED,
                        to: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                        src_access: 0,
                        dst_access: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                        src_stage: VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        dst_stage: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                    },
                );
                vkCmdPipelineBarrier(
                    cmd,
                    VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                    VK_PIPELINE_STAGE_EARLY_FRAGMENT_TESTS_BIT,
                    0,
                    0,
                    ptr::null(),
                    0,
                    ptr::null(),
                    1,
                    &VkImageMemoryBarrier {
                        sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                        pNext: ptr::null(),
                        srcAccessMask: 0,
                        dstAccessMask: VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT,
                        oldLayout: VK_IMAGE_LAYOUT_UNDEFINED,
                        newLayout: VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
                        srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                        image: d.depth_img,
                        subresourceRange: drange,
                    },
                );
                let cvs = [
                    VkClearValue {
                        color: VkClearColorValue { float32: BLACK },
                    },
                    VkClearValue {
                        depthStencil: VkClearDepthStencilValue {
                            depth: 1.0,
                            stencil: 0,
                        },
                    },
                ];
                let bi = VkRenderPassBeginInfo {
                    sType: VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
                    pNext: ptr::null(),
                    renderPass: d.rp_depth,
                    framebuffer: d.fbs_depth[idx as usize],
                    renderArea: VkRect2D {
                        offset: VkOffset2D { x: 0, y: 0 },
                        extent: (*vk).sc_extent,
                    },
                    clearValueCount: 2,
                    pClearValues: cvs.as_ptr(),
                };
                vkCmdBeginRenderPass(cmd, &bi, VK_SUBPASS_CONTENTS_INLINE);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_depth);
                vkCmdSetViewport(cmd, 0, 1, &vp);
                vkCmdSetScissor(cmd, 0, 1, &scissor);
                push_rect(cmd, FULL, [0.8, 0.1, 0.25, 1.0]);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                push_rect(cmd, FULL, [0.1, 0.8, 0.5, 1.0]);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                vkCmdEndRenderPass(cmd);
            }
            "blit" => {
                // Offscreen render, then a scaled blit into the swapchain
                // image: the game's actual present shape.
                let orange = VkImageSubresourceRange {
                    aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                    baseMipLevel: 0,
                    levelCount: 1,
                    baseArrayLayer: 0,
                    layerCount: 1,
                };
                barrier_img(
                    cmd,
                    d.off_img,
                    Transition {
                        from: VK_IMAGE_LAYOUT_UNDEFINED,
                        to: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                        src_access: 0,
                        dst_access: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                        src_stage: VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        dst_stage: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                    },
                );
                begin_pass(cmd, false, d.off_fb, [0.2, 0.6, 0.2, 1.0]);
                vkCmdEndRenderPass(cmd);
                barrier_img(
                    cmd,
                    d.off_img,
                    Transition {
                        from: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                        to: VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                        src_access: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                        dst_access: VK_ACCESS_TRANSFER_READ_BIT,
                        src_stage: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                        dst_stage: VK_PIPELINE_STAGE_TRANSFER_BIT,
                    },
                );
                barrier_img(
                    cmd,
                    img,
                    Transition {
                        from: VK_IMAGE_LAYOUT_UNDEFINED,
                        to: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                        src_access: 0,
                        dst_access: VK_ACCESS_TRANSFER_WRITE_BIT,
                        src_stage: VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        dst_stage: VK_PIPELINE_STAGE_TRANSFER_BIT,
                    },
                );
                let region = VkImageBlit {
                    srcSubresource: VkImageSubresourceLayers {
                        aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                        mipLevel: 0,
                        baseArrayLayer: 0,
                        layerCount: 1,
                    },
                    srcOffsets: [
                        VkOffset3D { x: 0, y: 0, z: 0 },
                        VkOffset3D {
                            x: 320,
                            y: 240,
                            z: 1,
                        },
                    ],
                    dstSubresource: VkImageSubresourceLayers {
                        aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                        mipLevel: 0,
                        baseArrayLayer: 0,
                        layerCount: 1,
                    },
                    dstOffsets: [
                        VkOffset3D { x: 0, y: 0, z: 0 },
                        VkOffset3D {
                            x: (*vk).sc_extent.width as i32,
                            y: (*vk).sc_extent.height as i32,
                            z: 1,
                        },
                    ],
                };
                vkCmdBlitImage(
                    cmd,
                    d.off_img,
                    VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                    img,
                    VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                    1,
                    &region,
                    VK_FILTER_LINEAR,
                );
                let _ = orange;
                barrier_img(
                    cmd,
                    img,
                    Transition {
                        from: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                        to: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                        src_access: VK_ACCESS_TRANSFER_WRITE_BIT,
                        dst_access: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                        src_stage: VK_PIPELINE_STAGE_TRANSFER_BIT,
                        dst_stage: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                    },
                );
            }
            "upload" => {
                // Known-good clear first, then a buffer->image copy of a 2x2
                // patch into the top-left corner: the dump shows whether the
                // copy lands without any sampling involved.
                let range = VkImageSubresourceRange {
                    aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                    baseMipLevel: 0,
                    levelCount: 1,
                    baseArrayLayer: 0,
                    layerCount: 1,
                };
                barrier_img(
                    cmd,
                    img,
                    Transition {
                        from: VK_IMAGE_LAYOUT_UNDEFINED,
                        to: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                        src_access: 0,
                        dst_access: VK_ACCESS_TRANSFER_WRITE_BIT,
                        src_stage: VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        dst_stage: VK_PIPELINE_STAGE_TRANSFER_BIT,
                    },
                );
                let col = VkClearColorValue {
                    float32: [0.0, 0.6, 0.0, 1.0],
                };
                vkCmdClearColorImage(
                    cmd,
                    img,
                    VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                    &col,
                    1,
                    &range,
                );
                let copy = VkBufferImageCopy {
                    bufferOffset: 0,
                    bufferRowLength: 0,
                    bufferImageHeight: 0,
                    imageSubresource: VkImageSubresourceLayers {
                        aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                        mipLevel: 0,
                        baseArrayLayer: 0,
                        layerCount: 1,
                    },
                    imageOffset: VkOffset3D { x: 0, y: 0, z: 0 },
                    imageExtent: VkExtent3D {
                        width: 2,
                        height: 2,
                        depth: 1,
                    },
                };
                vkCmdCopyBufferToImage(
                    cmd,
                    d.vb,
                    img,
                    VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                    1,
                    &copy,
                );
                barrier_img(
                    cmd,
                    img,
                    Transition {
                        from: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                        to: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                        src_access: VK_ACCESS_TRANSFER_WRITE_BIT,
                        dst_access: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                        src_stage: VK_PIPELINE_STAGE_TRANSFER_BIT,
                        dst_stage: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                    },
                );
            }
            "uv" => {
                // Procedural checkerboard: geometry + fragment stage only.
                begin_pass(cmd, false, fb, BLACK);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_uv);
                vkCmdSetViewport(cmd, 0, 1, &vp);
                vkCmdSetScissor(cmd, 0, 1, &scissor);
                push_rect(cmd, FULL, [1.0, 1.0, 1.0, 1.0]);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                vkCmdEndRenderPass(cmd);
            }
            "texel" => {
                // Same texture, but fetched straight from the texel: no sampler.
                begin_pass(cmd, false, fb, BLACK);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_texel);
                vkCmdSetViewport(cmd, 0, 1, &vp);
                vkCmdSetScissor(cmd, 0, 1, &scissor);
                let sets = [d.set_tex];
                vkCmdBindDescriptorSets(
                    cmd,
                    VK_PIPELINE_BIND_POINT_GRAPHICS,
                    d.layout,
                    0,
                    1,
                    sets.as_ptr(),
                    0,
                    ptr::null(),
                );
                push_rect(cmd, FULL, [1.0, 1.0, 1.0, 1.0]);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                vkCmdEndRenderPass(cmd);
            }
            "tex" => {
                begin_pass(cmd, false, fb, BLACK);
                vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, d.p_tex);
                vkCmdSetViewport(cmd, 0, 1, &vp);
                vkCmdSetScissor(cmd, 0, 1, &scissor);
                let sets = [d.set_tex];
                vkCmdBindDescriptorSets(
                    cmd,
                    VK_PIPELINE_BIND_POINT_GRAPHICS,
                    d.layout,
                    0,
                    1,
                    sets.as_ptr(),
                    0,
                    ptr::null(),
                );
                push_rect(cmd, FULL, [1.0, 1.0, 1.0, 1.0]);
                vkCmdDraw(cmd, 4, 1, 0, 0);
                vkCmdEndRenderPass(cmd);
            }
            other => logf!("unknown scene '{other}'"),
        }

        if last {
            barrier_img(
                cmd,
                img,
                Transition {
                    from: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                    to: VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                    src_access: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                    dst_access: VK_ACCESS_TRANSFER_READ_BIT,
                    src_stage: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                    dst_stage: VK_PIPELINE_STAGE_TRANSFER_BIT,
                },
            );
            let copy = VkBufferImageCopy {
                bufferOffset: 0,
                bufferRowLength: 0,
                bufferImageHeight: 0,
                imageSubresource: VkImageSubresourceLayers {
                    aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                    mipLevel: 0,
                    baseArrayLayer: 0,
                    layerCount: 1,
                },
                imageOffset: VkOffset3D { x: 0, y: 0, z: 0 },
                imageExtent: VkExtent3D {
                    width: (*vk).sc_extent.width,
                    height: (*vk).sc_extent.height,
                    depth: 1,
                },
            };
            vkCmdCopyImageToBuffer(
                cmd,
                img,
                VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                d.readback,
                1,
                &copy,
            );
            barrier_img(
                cmd,
                img,
                Transition {
                    from: VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                    to: VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
                    src_access: VK_ACCESS_TRANSFER_READ_BIT,
                    dst_access: 0,
                    src_stage: VK_PIPELINE_STAGE_TRANSFER_BIT,
                    dst_stage: VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                },
            );
        } else {
            barrier_img(
                cmd,
                img,
                Transition {
                    from: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                    to: VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
                    src_access: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                    dst_access: 0,
                    src_stage: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                    dst_stage: VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                },
            );
        }
    }
}

unsafe fn present_scene(scene: &str, last: bool) -> i32 {
    unsafe {
        let vk = &raw mut VK;
        let sci = VkSemaphoreCreateInfo {
            sType: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
        };
        let mut acquire_sem: VkSemaphore = 0;
        let mut render_sem: VkSemaphore = 0;
        vkCreateSemaphore((*vk).dev, &sci, ptr::null(), &mut acquire_sem);
        vkCreateSemaphore((*vk).dev, &sci, ptr::null(), &mut render_sem);

        let mut idx: u32 = 0;
        let res = vkAcquireNextImageKHR(
            (*vk).dev,
            (*vk).swapchain,
            3_000_000_000,
            acquire_sem,
            0,
            &mut idx,
        );
        {
            static ACQ_LOG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = ACQ_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 12 || res != VK_SUCCESS {
                logf!(
                    "acquire #{n} sc={:?} idx={idx} -> {res} (0=ok 2=timeout {})",
                    (*vk).swapchain,
                    VK_SUBOPTIMAL_KHR
                );
            }
        }
        if res == VK_TIMEOUT {
            logf!(
                "acquire TIMEOUT (3s): swapchain {:?} slot ring never released",
                (*vk).swapchain
            );
            return 1;
        }
        if res != VK_SUCCESS && res != VK_SUBOPTIMAL_KHR {
            return die_vk("vkAcquireNextImageKHR", res);
        }

        let cai = VkCommandBufferAllocateInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            pNext: ptr::null(),
            commandPool: (*vk).pool,
            level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
            commandBufferCount: 1,
        };
        let mut cmd: VkCommandBuffer = ptr::null_mut();
        vkAllocateCommandBuffers((*vk).dev, &cai, &mut cmd);
        let begin = VkCommandBufferBeginInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            pNext: ptr::null(),
            flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
            pInheritanceInfo: ptr::null(),
        };
        vkBeginCommandBuffer(cmd, &begin);
        record_scene(cmd, scene, idx, last);
        vkEndCommandBuffer(cmd);

        let wait_stage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
        let submit = VkSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
            pNext: ptr::null(),
            waitSemaphoreCount: 1,
            pWaitSemaphores: &acquire_sem,
            pWaitDstStageMask: &wait_stage,
            commandBufferCount: 1,
            pCommandBuffers: &cmd,
            signalSemaphoreCount: 1,
            pSignalSemaphores: &render_sem,
        };
        let res = vkQueueSubmit((*vk).queue, 1, &submit, 0);
        if res != VK_SUCCESS {
            return die_vk("vkQueueSubmit", res);
        }
        let pi = VkPresentInfoKHR {
            sType: VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
            pNext: ptr::null(),
            waitSemaphoreCount: 1,
            pWaitSemaphores: &render_sem,
            swapchainCount: 1,
            pSwapchains: &(*vk).swapchain,
            pImageIndices: &idx,
            pResults: ptr::null_mut(),
        };
        let res = vkQueuePresentKHR((*vk).queue, &pi);
        if res != VK_SUCCESS && res != VK_SUBOPTIMAL_KHR {
            return die_vk("vkQueuePresentKHR", res);
        }
        // DXVK keeps frames in flight; GNDRAW_INFLIGHT=1 does the same so a
        // recreate can land while the device is still busy (the last frame of
        // a scene always waits, so the readback stays valid).
        if !inflight() || !last {
            vkQueueWaitIdle((*vk).queue);
        }
        vkFreeCommandBuffers((*vk).dev, (*vk).pool, 1, &cmd);
    }
    0
}

/// Frames in flight across presents (GNDRAW_INFLIGHT=1).
static INFLIGHT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn inflight() -> bool {
    *INFLIGHT.get_or_init(|| std::env::var_os("GNDRAW_INFLIGHT").is_some())
}

/// Probes and prints the verdict for a scene; also drops a PPM next to the
/// game frame dumps when GAMENATIVE_DUMP_DIR is set.
unsafe fn readback(scene: &str) -> i32 {
    unsafe {
        let d = draw();
        let vk = &raw mut VK;
        let w = (*vk).sc_extent.width;
        let h = (*vk).sc_extent.height;
        let mut base: *mut c_void = ptr::null_mut();
        if vkMapMemory((*vk).dev, d.readback_mem, 0, 0, 0, &mut base) != VK_SUCCESS {
            logf!("scene {scene}: vkMapMemory failed");
            return 1;
        }
        let px = base as *const u8;
        // The image bytes follow the swapchain fourcc: R8G8B8A8 is RGBA in
        // memory, B8G8R8A8 is BGRA.
        let bgra = (*vk).sc_format != VK_FORMAT_R8G8B8A8_UNORM;
        let at = |x: u32, y: u32| -> [u8; 3] {
            let o = ((y * w + x) * 4) as usize;
            let (r, g, b) = if bgra {
                (px.add(o + 2).read(), px.add(o + 1).read(), px.add(o).read())
            } else {
                (px.add(o).read(), px.add(o + 1).read(), px.add(o + 2).read())
            };
            [r, g, b]
        };
        let mean = |x0: u32, y0: u32, x1: u32, y1: u32| -> [i32; 3] {
            let mut acc = [0i64; 3];
            let mut n = 0i64;
            let mut y = y0;
            while y < y1 {
                let mut x = x0;
                while x < x1 {
                    let c = at(x, y);
                    acc[0] += c[0] as i64;
                    acc[1] += c[1] as i64;
                    acc[2] += c[2] as i64;
                    n += 1;
                    x += 4;
                }
                y += 4;
            }
            [
                (acc[0] / n.max(1)) as i32,
                (acc[1] / n.max(1)) as i32,
                (acc[2] / n.max(1)) as i32,
            ]
        };
        let l = mean(w / 8, h / 4, 3 * w / 8, 3 * h / 4);
        let r = mean(5 * w / 8, h / 4, 7 * w / 8, 3 * h / 4);
        let q = [
            mean(w / 16, h / 16, w / 2 - w / 16, h / 2 - h / 16),
            mean(w / 2 + w / 16, h / 16, w - w / 16, h / 2 - h / 16),
            mean(w / 16, h / 2 + h / 16, w / 2 - w / 16, h - h / 16),
            mean(w / 2 + w / 16, h / 2 + h / 16, w - w / 16, h - h / 16),
        ];
        let near = |a: &[i32; 3], b: [i32; 3], tol: i32| -> bool {
            (a[0] - b[0]).abs() <= tol && (a[1] - b[1]).abs() <= tol && (a[2] - b[2]).abs() <= tol
        };
        let dom = |a: &[i32; 3], i: usize| -> bool {
            a[i] > a[(i + 1) % 3] + 60 && a[i] > a[(i + 2) % 3] + 60
        };
        let blackish = |a: &[i32; 3]| -> bool { a[0] < 40 && a[1] < 40 && a[2] < 40 };
        let whiteish = |a: &[i32; 3]| -> bool { a[0] > 200 && a[1] > 200 && a[2] > 200 };

        let pass = match scene {
            "clear" => near(&l, [26, 102, 255], 24) && near(&r, [26, 102, 255], 24),
            "upload" => near(&r, [0, 153, 0], 24),
            "cull" => l[0] > 100 && l[1] < 100 && r[0] < 25 && r[1] < 25 && r[2] < 25,
            // Either orientation is a valid render; the printed quadrants say
            // which one it was (read them, don't infer).
            "yflip" => q[0][2] > 100 || q[2][2] > 100,
            "resize" => near(&l, [204, 51, 51], 24) && near(&r, [204, 51, 51], 24),
            "rpclear" => near(&l, [230, 26, 153], 24) && near(&r, [230, 26, 153], 24),
            "quad" => near(&l, [204, 51, 51], 24) && near(&r, [204, 51, 51], 24),
            "blend" => near(&l, [127, 76, 0], 30) && near(&r, [0, 153, 0], 30),
            "load" => whiteish(&l) && near(&r, [255, 0, 0], 30),
            "vtx" => dom(&q[2], 0) && dom(&q[3], 1) && dom(&q[0], 2) && whiteish(&q[1]),
            "depth" => near(&l, [204, 26, 64], 30) && near(&r, [204, 26, 64], 30),
            "blit" => near(&l, [51, 153, 51], 30) && near(&r, [51, 153, 51], 30),
            "bigtex" | "bigtex_bgra" => near(&l, [200, 30, 30], 30) && near(&r, [30, 200, 30], 30),
            "bigtex_srgb" => l[0] > 100 && l[1] < 60 && l[2] < 60 && r[1] > 100,
            "uv" | "texel" | "tex" => {
                let white = |a: &[i32; 3]| whiteish(a);
                let black = |a: &[i32; 3]| blackish(a);
                (white(&q[0]) && black(&q[1]) && black(&q[2]) && white(&q[3]))
                    || (black(&q[0]) && white(&q[1]) && white(&q[2]) && black(&q[3]))
            }
            other => {
                logf!("scene {other}: no expectation");
                false
            }
        };
        logf!(
            "scene {scene}: left={l:?} right={r:?} quadrants={q:?} -> {}",
            if pass { "PASS" } else { "FAIL" }
        );

        // PPM next to the game frame dumps, when the launcher provided a dir.
        if let Some(dir) = std::env::var_os("GAMENATIVE_DUMP_DIR").filter(|v| !v.is_empty()) {
            let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
            for y in 0..h {
                for x in 0..w {
                    out.extend_from_slice(&at(x, y));
                }
            }
            let path = std::path::Path::new(&dir).join(format!("gndraw-{scene}.ppm"));
            if std::fs::write(&path, &out).is_ok() {
                logf!("scene {scene}: wrote {}", path.display());
            }
        }
        vkUnmapMemory((*vk).dev, d.readback_mem);
    }
    0
}

/// Recreate the swapchain at a new extent, the way a D3D->Vulkan layer does
/// when the surface reports SUBOPTIMAL (DXVK: 960x600 -> 1280x960). Old
/// per-image state is replaced in place; the depth/offscreen targets are
/// extent-independent enough for this check.
unsafe fn recreate_swapchain(w: u32, h: u32) -> i32 {
    unsafe {
        let vk = &raw mut VK;
        let d = draw();
        // Mirror DXVK: no vkDeviceWaitIdle, frames still in flight, the old
        // swapchain handed to the create and destroyed afterwards.
        let old_swapchain = (*vk).swapchain;
        let min_images = 3u32;
        let sci = VkSwapchainCreateInfoKHR {
            sType: VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
            pNext: ptr::null(),
            flags: 0,
            surface: (*vk).surface,
            minImageCount: min_images,
            imageFormat: (*vk).sc_format,
            imageColorSpace: 0,
            imageExtent: VkExtent2D {
                width: w,
                height: h,
            },
            imageArrayLayers: 1,
            imageUsage: VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT
                | VK_IMAGE_USAGE_TRANSFER_DST_BIT
                | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
            imageSharingMode: VK_SHARING_MODE_EXCLUSIVE,
            queueFamilyIndexCount: 0,
            pQueueFamilyIndices: ptr::null(),
            preTransform: VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
            compositeAlpha: VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
            presentMode: VK_PRESENT_MODE_FIFO_KHR,
            clipped: VK_TRUE,
            oldSwapchain: old_swapchain,
        };
        let res = vkCreateSwapchainKHR((*vk).dev, &sci, ptr::null(), &mut (*vk).swapchain);
        logf!("resize: vkCreateSwapchainKHR {w}x{h} (old={old_swapchain:?}) -> {res}");
        if res != VK_SUCCESS {
            return 1;
        }
        if old_swapchain != 0 {
            vkDestroySwapchainKHR((*vk).dev, old_swapchain, ptr::null());
            logf!("resize: old swapchain {old_swapchain:?} destroyed");
        }
        (*vk).sc_extent = VkExtent2D {
            width: w,
            height: h,
        };
        let mut n: u32 = 8;
        let res = vkGetSwapchainImagesKHR(
            (*vk).dev,
            (*vk).swapchain,
            &mut n,
            (*vk).images.as_mut_ptr(),
        );
        if res != VK_SUCCESS {
            logf!("resize: vkGetSwapchainImagesKHR -> {res}");
            return 1;
        }
        (*vk).n_images = n;
        logf!("resize: {n} images at {w}x{h}");
        let mut caps = std::mem::zeroed::<VkSurfaceCapabilitiesKHR>();
        let cres = vkGetPhysicalDeviceSurfaceCapabilitiesKHR((*vk).pd, (*vk).surface, &mut caps);
        logf!(
            "resize: caps res={cres} currentExtent={}x{} minImages={} maxImages={}",
            caps.currentExtent.width,
            caps.currentExtent.height,
            caps.minImageCount,
            caps.maxImageCount
        );
        for i in 0..n as usize {
            let vci = VkImageViewCreateInfo {
                sType: VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                image: (*vk).images[i],
                viewType: VK_IMAGE_VIEW_TYPE_2D,
                format: (*vk).sc_format,
                components: VkComponentMapping {
                    r: VK_COMPONENT_SWIZZLE_IDENTITY,
                    g: VK_COMPONENT_SWIZZLE_IDENTITY,
                    b: VK_COMPONENT_SWIZZLE_IDENTITY,
                    a: VK_COMPONENT_SWIZZLE_IDENTITY,
                },
                subresourceRange: VkImageSubresourceRange {
                    aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                    baseMipLevel: 0,
                    levelCount: 1,
                    baseArrayLayer: 0,
                    layerCount: 1,
                },
            };
            let mut view: VkImageView = 0;
            let res = vkCreateImageView((*vk).dev, &vci, ptr::null(), &mut view);
            if res != VK_SUCCESS {
                logf!("resize: vkCreateImageView({i}) -> {res}");
                return 1;
            }
            let fci = VkFramebufferCreateInfo {
                sType: VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                renderPass: d.rp_clear,
                attachmentCount: 1,
                pAttachments: &view,
                width: w,
                height: h,
                layers: 1,
            };
            let mut fb: VkFramebuffer = 0;
            let res = vkCreateFramebuffer((*vk).dev, &fci, ptr::null(), &mut fb);
            if res != VK_SUCCESS {
                logf!("resize: vkCreateFramebuffer({i}) -> {res}");
                return 1;
            }
            // Install into the draw state (the old handles are leaked on
            // purpose: this is a diagnostic client).
            let dp: *mut Draw = &raw const *d as *mut Draw;
            (*dp).fbs[i] = fb;
        }
    }
    0
}

fn run_scenes(list: &str) -> i32 {
    logf!("draw scenes: {list}");
    if wayland_init() != 0 {
        return 1;
    }
    if vulkan_init() != 0 {
        return 1;
    }
    if swapchain_init() != 0 {
        return 1;
    }
    if unsafe { draw_init() } != 0 {
        return 1;
    }
    for scene in list.split(',') {
        let name = scene.trim();
        if name.is_empty() {
            continue;
        }
        if name == "resize" {
            // DXVK's order: frames in flight, then the recreate, then frames.
            logf!("scene resize: 4 frames in flight, then recreate at 960x600");
            for _ in 0..4 {
                unsafe {
                    if !WL.running {
                        break;
                    }
                }
                if unsafe { present_scene(name, false) } != 0 {
                    logf!("scene resize: present failed before recreate");
                    return 1;
                }
                unsafe {
                    let wl = &raw const WL;
                    while wlproto::wl_display_dispatch_pending((*wl).dpy) > 0 {}
                    wlproto::wl_display_flush((*wl).dpy);
                }
            }
            if unsafe { recreate_swapchain(960, 600) } != 0 {
                logf!("scene resize: recreate failed");
                return 1;
            }
        }
        logf!("scene {name}: {NUM_SCENE_FRAMES} frames");
        for f in 0..NUM_SCENE_FRAMES {
            unsafe {
                if !WL.running {
                    break;
                }
            }
            let last = f == NUM_SCENE_FRAMES - 1;
            if unsafe { present_scene(name, last) } != 0 {
                return 1;
            }
            unsafe {
                let wl = &raw const WL;
                while wlproto::wl_display_dispatch_pending((*wl).dpy) > 0 {}
                wlproto::wl_display_flush((*wl).dpy);
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        unsafe {
            readback(name);
        }
    }
    logf!("draw scenes done");
    0
}

// ------------------------------------------- mip-mapped texture scenes

struct Tex {
    set: VkDescriptorSet,
    _image: VkImage,
    _view: VkImageView,
    _sampler: VkSampler,
    _mem: VkDeviceMemory,
    _staging: VkBuffer,
    _staging_mem: VkDeviceMemory,
}

static mut BIGTEX: Option<Tex> = None;
static mut BIGTEX_KEY: VkFormat = -1;

/// Build (once per format) the mip-mapped texture the bigtex scenes read.
unsafe fn bigtex(format: VkFormat) -> Option<&'static Tex> {
    unsafe {
        if BIGTEX_KEY != format {
            BIGTEX = build_tex(1024, 1024, 8, format, true);
            BIGTEX_KEY = format;
        }
        let p: *const Option<Tex> = &raw const BIGTEX;
        (*p).as_ref()
    }
}

/// Upload a mip-mapped texture the way a D3D->Vulkan translation layer does:
/// one staging buffer, one vkCmdCopyBufferToImage per mip level, into an
/// OPTIMAL image (plus LINEAR/format variants). Level 0 is filled with
/// (200,30,30) and level 1 with (30,200,30); the scene reads one level per
/// half of the screen, so a per-level upload failure shows up directly.
unsafe fn build_tex(w: u32, h: u32, mips: u32, format: VkFormat, optimal: bool) -> Option<Tex> {
    unsafe {
        let vk = &raw mut VK;
        let d = draw();
        let mut mprops = std::mem::zeroed::<VkPhysicalDeviceMemoryProperties>();
        vkGetPhysicalDeviceMemoryProperties((*vk).pd, &mut mprops);

        let host_alloc = |size: u64,
                          usage: VkBufferUsageFlags|
         -> Option<(VkBuffer, VkDeviceMemory)> {
            let bci = VkBufferCreateInfo {
                sType: VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                size,
                usage,
                sharingMode: VK_SHARING_MODE_EXCLUSIVE,
                queueFamilyIndexCount: 0,
                pQueueFamilyIndices: ptr::null(),
            };
            let mut buf: VkBuffer = 0;
            if vkCreateBuffer((*vk).dev, &bci, ptr::null(), &mut buf) != VK_SUCCESS {
                return None;
            }
            let mut reqs = std::mem::zeroed::<VkMemoryRequirements>();
            vkGetBufferMemoryRequirements((*vk).dev, buf, &mut reqs);
            let want = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
            let mut mt = u32::MAX;
            for i in 0..mprops.memoryTypeCount {
                if reqs.memoryTypeBits & (1 << i) != 0
                    && mprops.memoryTypes[i as usize].propertyFlags & want == want
                {
                    mt = i;
                    break;
                }
            }
            if mt == u32::MAX {
                return None;
            }
            let ai = VkMemoryAllocateInfo {
                sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                pNext: ptr::null(),
                allocationSize: reqs.size,
                memoryTypeIndex: mt,
            };
            let mut mem: VkDeviceMemory = 0;
            if vkAllocateMemory((*vk).dev, &ai, ptr::null(), &mut mem) != VK_SUCCESS {
                return None;
            }
            if vkBindBufferMemory((*vk).dev, buf, mem, 0) != VK_SUCCESS {
                return None;
            }
            Some((buf, mem))
        };

        // Staging: one solid colour per level.
        let mut offsets = [0u64; 16];
        let mut sizes = [0u64; 16];
        let mut total = 0u64;
        for (i, (off, sz)) in offsets
            .iter_mut()
            .zip(sizes.iter_mut())
            .enumerate()
            .take(mips as usize)
        {
            *off = total;
            let lw = (w >> i).max(1);
            let lh = (h >> i).max(1);
            *sz = (lw * lh * 4) as u64;
            total += *sz;
        }
        let (staging, staging_mem) = host_alloc(total, VK_BUFFER_USAGE_TRANSFER_SRC_BIT)?;
        let mut map: *mut c_void = ptr::null_mut();
        if vkMapMemory((*vk).dev, staging_mem, 0, total, 0, &mut map) != VK_SUCCESS {
            return None;
        }
        for (i, (off, sz)) in offsets
            .iter()
            .zip(sizes.iter())
            .enumerate()
            .take(mips as usize)
        {
            let colour: [u8; 4] = if i == 0 {
                [200, 30, 30, 255]
            } else if i == 1 {
                [30, 200, 30, 255]
            } else {
                [0, 0, 0, 255]
            };
            let p = (map as *mut u8).add(*off as usize);
            for px in 0..(*sz / 4) as usize {
                ptr::copy_nonoverlapping(colour.as_ptr(), p.add(px * 4), 4);
            }
        }
        vkUnmapMemory((*vk).dev, staging_mem);

        let ici = VkImageCreateInfo {
            sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            imageType: VK_IMAGE_TYPE_2D,
            format,
            extent: VkExtent3D {
                width: w,
                height: h,
                depth: 1,
            },
            mipLevels: mips,
            arrayLayers: 1,
            samples: VK_SAMPLE_COUNT_1_BIT,
            tiling: if optimal {
                VK_IMAGE_TILING_OPTIMAL
            } else {
                VK_IMAGE_TILING_LINEAR
            },
            usage: VK_IMAGE_USAGE_SAMPLED_BIT
                | VK_IMAGE_USAGE_TRANSFER_DST_BIT
                | VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
            sharingMode: VK_SHARING_MODE_EXCLUSIVE,
            queueFamilyIndexCount: 0,
            pQueueFamilyIndices: ptr::null(),
            initialLayout: VK_IMAGE_LAYOUT_UNDEFINED,
        };
        let mut image: VkImage = 0;
        let res = vkCreateImage((*vk).dev, &ici, ptr::null(), &mut image);
        if res != VK_SUCCESS {
            logf!(
                "bigtex: create fmt={format} {w}x{h} mips={mips} tiling_optimal={optimal} -> {res}"
            );
            return None;
        }
        let mut reqs = std::mem::zeroed::<VkMemoryRequirements>();
        vkGetImageMemoryRequirements((*vk).dev, image, &mut reqs);
        // An OPTIMAL image wants device memory that is not host-visible; take
        // the first such type, else whatever the driver offers.
        let mut mt = u32::MAX;
        for i in 0..mprops.memoryTypeCount {
            let f = mprops.memoryTypes[i as usize].propertyFlags;
            if reqs.memoryTypeBits & (1 << i) != 0
                && f & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT != 0
                && f & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT == 0
            {
                mt = i;
                break;
            }
        }
        if mt == u32::MAX {
            for i in 0..mprops.memoryTypeCount {
                if reqs.memoryTypeBits & (1 << i) != 0 {
                    mt = i;
                    break;
                }
            }
        }
        let ai = VkMemoryAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            pNext: ptr::null(),
            allocationSize: reqs.size,
            memoryTypeIndex: mt,
        };
        let mut mem: VkDeviceMemory = 0;
        let ares = vkAllocateMemory((*vk).dev, &ai, ptr::null(), &mut mem);
        let bres = if ares == VK_SUCCESS {
            vkBindImageMemory((*vk).dev, image, mem, 0)
        } else {
            -1
        };
        logf!(
            "bigtex: create=0 alloc={ares} bind={bres} memtype={mt} reqs={}",
            reqs.size
        );
        if bres != VK_SUCCESS {
            return None;
        }

        let vci = VkImageViewCreateInfo {
            sType: VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            image,
            viewType: VK_IMAGE_VIEW_TYPE_2D,
            format,
            components: VkComponentMapping {
                r: VK_COMPONENT_SWIZZLE_IDENTITY,
                g: VK_COMPONENT_SWIZZLE_IDENTITY,
                b: VK_COMPONENT_SWIZZLE_IDENTITY,
                a: VK_COMPONENT_SWIZZLE_IDENTITY,
            },
            subresourceRange: VkImageSubresourceRange {
                aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                baseMipLevel: 0,
                levelCount: mips,
                baseArrayLayer: 0,
                layerCount: 1,
            },
        };
        let mut view: VkImageView = 0;
        if vkCreateImageView((*vk).dev, &vci, ptr::null(), &mut view) != VK_SUCCESS {
            return None;
        }
        let sci = VkSamplerCreateInfo {
            sType: VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            magFilter: VK_FILTER_LINEAR,
            minFilter: VK_FILTER_LINEAR,
            mipmapMode: VK_SAMPLER_MIPMAP_MODE_NEAREST,
            addressModeU: VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
            addressModeV: VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
            addressModeW: VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
            mipLodBias: 0.0,
            anisotropyEnable: VK_FALSE,
            maxAnisotropy: 1.0,
            compareEnable: VK_FALSE,
            compareOp: VK_COMPARE_OP_NEVER,
            minLod: 0.0,
            maxLod: mips as f32,
            borderColor: 0,
            unnormalizedCoordinates: VK_FALSE,
        };
        let mut sampler: VkSampler = 0;
        if vkCreateSampler((*vk).dev, &sci, ptr::null(), &mut sampler) != VK_SUCCESS {
            return None;
        }
        let dsai = VkDescriptorSetAllocateInfo {
            sType: VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
            pNext: ptr::null(),
            descriptorPool: d.build_pool,
            descriptorSetCount: 1,
            pSetLayouts: &d.set_layout,
        };
        let mut set: VkDescriptorSet = 0;
        if vkAllocateDescriptorSets((*vk).dev, &dsai, &mut set) != VK_SUCCESS {
            return None;
        }
        let dii = VkDescriptorImageInfo {
            sampler,
            imageView: view,
            imageLayout: VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
        };
        let wds = VkWriteDescriptorSet {
            sType: VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
            pNext: ptr::null(),
            dstSet: set,
            dstBinding: 0,
            dstArrayElement: 0,
            descriptorCount: 1,
            descriptorType: VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
            pImageInfo: &dii,
            pBufferInfo: ptr::null(),
            pTexelBufferView: ptr::null(),
        };
        vkUpdateDescriptorSets((*vk).dev, 1, &wds, 0, ptr::null());

        // Upload: one copy per mip level.
        let cai = VkCommandBufferAllocateInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            pNext: ptr::null(),
            commandPool: (*vk).pool,
            level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
            commandBufferCount: 1,
        };
        let mut cmd: VkCommandBuffer = ptr::null_mut();
        vkAllocateCommandBuffers((*vk).dev, &cai, &mut cmd);
        let begin = VkCommandBufferBeginInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            pNext: ptr::null(),
            flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
            pInheritanceInfo: ptr::null(),
        };
        vkBeginCommandBuffer(cmd, &begin);
        let range = VkImageSubresourceRange {
            aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
            baseMipLevel: 0,
            levelCount: mips,
            baseArrayLayer: 0,
            layerCount: 1,
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
            &VkImageMemoryBarrier {
                sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                pNext: ptr::null(),
                srcAccessMask: 0,
                dstAccessMask: VK_ACCESS_TRANSFER_WRITE_BIT,
                oldLayout: VK_IMAGE_LAYOUT_UNDEFINED,
                newLayout: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                image,
                subresourceRange: range,
            },
        );
        for (i, off) in offsets.iter().take(mips as usize).enumerate() {
            let lw = (w >> i).max(1);
            let lh = (h >> i).max(1);
            let copy = VkBufferImageCopy {
                bufferOffset: *off,
                bufferRowLength: 0,
                bufferImageHeight: 0,
                imageSubresource: VkImageSubresourceLayers {
                    aspectMask: VK_IMAGE_ASPECT_COLOR_BIT,
                    mipLevel: i as u32,
                    baseArrayLayer: 0,
                    layerCount: 1,
                },
                imageOffset: VkOffset3D { x: 0, y: 0, z: 0 },
                imageExtent: VkExtent3D {
                    width: lw,
                    height: lh,
                    depth: 1,
                },
            };
            vkCmdCopyBufferToImage(
                cmd,
                staging,
                image,
                VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                1,
                &copy,
            );
        }
        vkCmdPipelineBarrier(
            cmd,
            VK_PIPELINE_STAGE_TRANSFER_BIT,
            VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            1,
            &VkImageMemoryBarrier {
                sType: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                pNext: ptr::null(),
                srcAccessMask: VK_ACCESS_TRANSFER_WRITE_BIT,
                dstAccessMask: VK_ACCESS_SHADER_READ_BIT,
                oldLayout: VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                newLayout: VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                srcQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                dstQueueFamilyIndex: VK_QUEUE_FAMILY_IGNORED,
                image,
                subresourceRange: range,
            },
        );
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
        let sres = vkQueueSubmit((*vk).queue, 1, &submit, 0);
        vkQueueWaitIdle((*vk).queue);
        vkFreeCommandBuffers((*vk).dev, (*vk).pool, 1, &cmd);
        logf!("bigtex: upload submit={sres} mips={mips} bytes={total}");

        Some(Tex {
            set,
            _image: image,
            _view: view,
            _sampler: sampler,
            _mem: mem,
            _staging: staging,
            _staging_mem: staging_mem,
        })
    }
}

/// DXVK's presentation pattern against the shim's swapchain: acquire signalling
/// a *binary* semaphore, a submit2 that **waits** on that semaphore and signals
/// a timeline, a blocking wait on the timeline, then present. The scene suite
/// acquires and submits with v1+fences, and the standalone probe has no
/// swapchain, so the shim's acquire -> submit2 semaphore handoff is untested.
unsafe fn sem_chain_frames(frames: u32) -> i32 {
    const ST_TYPE_CREATE: i32 = 1000207002;
    const ST_WAIT_INFO: i32 = 1000207004;
    const SEM_TYPE_TIMELINE: i32 = 1;
    #[repr(C)]
    struct TypeCi {
        sType: i32,
        pNext: *const c_void,
        semaphoreType: i32,
        initialValue: u64,
    }
    #[repr(C)]
    struct WaitInfo {
        sType: i32,
        pNext: *const c_void,
        flags: u32,
        semaphoreCount: u32,
        pSemaphores: *const VkSemaphore,
        pValues: *const u64,
    }
    type WaitFn = unsafe extern "C" fn(VkDevice, *const WaitInfo, u64) -> VkResult;

    let vk = &raw const VK;
    let dev = (*vk).dev;
    let queue = (*vk).queue;

    let mut bin: VkSemaphore = 0;
    let bci = VkSemaphoreCreateInfo {
        sType: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
        pNext: ptr::null(),
        flags: 0,
    };
    vkCreateSemaphore(dev, &bci, ptr::null(), &mut bin);
    let tci = TypeCi {
        sType: ST_TYPE_CREATE,
        pNext: ptr::null(),
        semaphoreType: SEM_TYPE_TIMELINE,
        initialValue: 0,
    };
    let tcci = VkSemaphoreCreateInfo {
        sType: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
        pNext: &tci as *const _ as *const c_void,
        flags: 0,
    };
    let mut tl: VkSemaphore = 0;
    vkCreateSemaphore(dev, &tcci, ptr::null(), &mut tl);
    logf!("semaphores: acquire(binary)={bin} timeline={tl}");
    if bin == 0 || tl == 0 {
        return 1;
    }
    let wait_fn: WaitFn = std::mem::transmute(vkGetDeviceProcAddr(
        dev,
        c"vkWaitSemaphoresKHR".as_ptr() as *const c_char,
    ));

    let cpci = VkCommandPoolCreateInfo {
        sType: VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        pNext: ptr::null(),
        flags: VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
        queueFamilyIndex: (*vk).qf,
    };
    let mut cp: VkCommandPool = 0;
    vkCreateCommandPool(dev, &cpci, ptr::null(), &mut cp);
    let cai = VkCommandBufferAllocateInfo {
        sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        pNext: ptr::null(),
        commandPool: cp,
        level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        commandBufferCount: 1,
    };
    let mut cb: VkCommandBuffer = ptr::null_mut();
    vkAllocateCommandBuffers(dev, &cai, &mut cb);

    for i in 0..frames {
        let mut idx: u32 = 0;
        let r = vkAcquireNextImageKHR(dev, (*vk).swapchain, u64::MAX, bin, 0, &mut idx);
        if r != VK_SUCCESS && r != VK_SUBOPTIMAL_KHR {
            logf!("frame {i}: acquire -> {r}");
            return 1;
        }
        let begin = VkCommandBufferBeginInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            pNext: ptr::null(),
            flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
            pInheritanceInfo: ptr::null(),
        };
        vkBeginCommandBuffer(cb, &begin);
        vkEndCommandBuffer(cb);
        let wait = VkSemaphoreSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO,
            pNext: ptr::null(),
            semaphore: bin,
            value: 0,
            stageMask: VK_PIPELINE_STAGE_ALL_COMMANDS_BIT as VkPipelineStageFlags2,
            deviceIndex: 0,
        };
        let sig = VkSemaphoreSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO,
            pNext: ptr::null(),
            semaphore: tl,
            value: (i + 1) as u64,
            stageMask: VK_PIPELINE_STAGE_ALL_COMMANDS_BIT as VkPipelineStageFlags2,
            deviceIndex: 0,
        };
        let cbi = VkCommandBufferSubmitInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO,
            pNext: ptr::null(),
            commandBuffer: cb,
            deviceMask: 0,
        };
        let s2 = VkSubmitInfo2 {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO_2,
            pNext: ptr::null(),
            flags: 0,
            waitSemaphoreInfoCount: 1,
            pWaitSemaphoreInfos: &wait as *const _ as *const c_void,
            commandBufferInfoCount: 1,
            pCommandBufferInfos: &cbi,
            signalSemaphoreInfoCount: 1,
            pSignalSemaphoreInfos: &sig as *const _ as *const c_void,
        };
        let r = vkQueueSubmit2(queue, 1, &s2, 0);
        logf!(
            "frame {i}: acquire idx={idx} then submit2(wait bin, signal tl@{}) -> {r}",
            i + 1
        );
        let vals = [(i + 1) as u64];
        let sems = [tl];
        let wi = WaitInfo {
            sType: ST_WAIT_INFO,
            pNext: ptr::null(),
            flags: 0,
            semaphoreCount: 1,
            pSemaphores: sems.as_ptr(),
            pValues: vals.as_ptr(),
        };
        logf!("frame {i}: BLOCKING wait on tl@{} ...", i + 1);
        let t0 = std::time::Instant::now();
        let r = wait_fn(dev, &wi, u64::MAX);
        logf!(
            "frame {i}: wait -> {r} after {}ms",
            t0.elapsed().as_millis()
        );
        if r != VK_SUCCESS {
            return 1;
        }
        let pi = VkPresentInfoKHR {
            sType: VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
            pNext: ptr::null(),
            waitSemaphoreCount: 0,
            pWaitSemaphores: ptr::null(),
            swapchainCount: 1,
            pSwapchains: &(*vk).swapchain,
            pImageIndices: &idx,
            pResults: ptr::null_mut(),
        };
        let r = vkQueuePresentKHR(queue, &pi);
        logf!("frame {i}: present -> {r}");
        unsafe {
            let wl = &raw const WL;
            while wlproto::wl_display_dispatch_pending((*wl).dpy) > 0 {}
            wlproto::wl_display_flush((*wl).dpy);
        }
    }
    logf!("sem chain: {frames} frames done");
    0
}

fn run() -> i32 {
    logf!("starting");
    if let Some(scenes) = std::env::var_os("GNDRAW_SCENES").filter(|v| !v.is_empty()) {
        let list = scenes.to_string_lossy();
        let list = if list == "all" {
            "clear,rpclear,quad,blend,load,vtx,uv,texel,tex,upload,bigtex,bigtex_bgra,bigtex_srgb,depth,blit,resize,cull"
        } else {
            list.as_ref()
        };
        return run_scenes(list);
    }
    if wayland_init() != 0 {
        return 1;
    }
    if vulkan_init() != 0 {
        return 1;
    }
    if swapchain_init() != 0 {
        return 1;
    }
    if std::env::var_os("GNDRAW_SEMCHAIN").is_some() {
        return unsafe { sem_chain_frames(400) };
    }

    const COLORS: [[f32; 3]; 4] = [
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [1.0, 1.0, 1.0],
    ];
    for f in 0..NUM_FRAMES {
        unsafe {
            if !WL.running {
                break;
            }
        }
        let c = COLORS[f % 4];
        if present_frame(c[0], c[1], c[2]) != 0 {
            return 1;
        }
        logf!("frame {f} presented ({:.0} {:.0} {:.0})", c[0], c[1], c[2]);
        unsafe {
            let wl = &raw const WL;
            while wlproto::wl_display_dispatch_pending((*wl).dpy) > 0 {}
            wlproto::wl_display_flush((*wl).dpy);
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }

    logf!("done - all frames presented");
    unsafe {
        let vk = &raw mut VK;
        vkDeviceWaitIdle((*vk).dev);
        vkDestroySwapchainKHR((*vk).dev, (*vk).swapchain, ptr::null());
        vkDestroySurfaceKHR((*vk).inst, (*vk).surface, ptr::null());
    }
    0
}

fn main() {
    std::process::exit(run());
}
