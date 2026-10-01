//! gn-device-probe — create a device through the ICD shim and report what the
//! guest actually gets. No surface, no Gamescope: verifies device-level
//! extension masking (the shim drops the pipeline-library extensions) without
//! needing a live session.

#![allow(non_snake_case)]

use gn_utils::vk::*;
use std::ffi::{c_char, c_void, CStr};
use std::ptr;

macro_rules! logf {
    ($fmt:literal $(, $($arg:tt)*)?) => {
        eprintln!("gn-device-probe: {}", format_args!($fmt $(, $($arg)*)?))
    };
}

fn run() -> i32 {
    unsafe {
        let app = VkApplicationInfo {
            sType: VK_STRUCTURE_TYPE_APPLICATION_INFO,
            pNext: ptr::null(),
            pApplicationName: c"gn-device-probe".as_ptr(),
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
        let mut inst: VkInstance = ptr::null_mut();
        let res = vkCreateInstance(&ici, ptr::null(), &mut inst);
        if res != VK_SUCCESS {
            logf!("vkCreateInstance -> {res}");
            return 1;
        }
        let mut n: u32 = 0;
        vkEnumeratePhysicalDevices(inst, &mut n, ptr::null_mut());
        if n == 0 {
            logf!("no physical devices");
            return 1;
        }
        let mut pds = vec![ptr::null_mut::<c_void>(); n as usize];
        vkEnumeratePhysicalDevices(inst, &mut n, pds.as_mut_ptr());
        let pd = pds[0];
        let mut props = std::mem::zeroed::<VkPhysicalDeviceProperties>();
        vkGetPhysicalDeviceProperties(pd, &mut props);
        logf!(
            "device: {}",
            CStr::from_ptr(props.deviceName.as_ptr()).to_string_lossy()
        );

        // The two pipeline-library extensions are what the shim masks by
        // default; the shim logs each one it removes.
        let exts: [*const c_char; 2] = [
            c"VK_EXT_graphics_pipeline_library".as_ptr(),
            c"VK_KHR_pipeline_library".as_ptr(),
        ];
        let prio = 1.0f32;
        let qci = VkDeviceQueueCreateInfo {
            sType: VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            queueFamilyIndex: 0,
            queueCount: 1,
            pQueuePriorities: &prio,
        };
        let dci = VkDeviceCreateInfo {
            sType: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
            queueCreateInfoCount: 1,
            pQueueCreateInfos: &qci,
            enabledLayerCount: 0,
            ppEnabledLayerNames: ptr::null(),
            enabledExtensionCount: exts.len() as u32,
            ppEnabledExtensionNames: exts.as_ptr(),
            pEnabledFeatures: ptr::null(),
        };
        let mut dev: VkDevice = ptr::null_mut();
        let res = vkCreateDevice(pd, &dci, ptr::null(), &mut dev);
        logf!(
            "device create requesting GPL pair -> {res} ({})",
            if res == VK_SUCCESS { "ok" } else { "failed" }
        );
        if res != VK_SUCCESS {
            return 1;
        }
        // Second device: entry points the shim resolves must be per device, so
        // a submit on *this* device's queue must not reuse the first one's
        // function (which is bound to the handle that resolved it).
        let mut dev2: VkDevice = ptr::null_mut();
        let res2 = vkCreateDevice(pd, &dci, ptr::null(), &mut dev2);
        logf!("second device create -> {res2}");
        let mut q1: VkQueue = ptr::null_mut();
        let mut q2: VkQueue = ptr::null_mut();
        vkGetDeviceQueue(dev, 0, 0, &mut q1);
        vkGetDeviceQueue(dev2, 0, 0, &mut q2);
        logf!("queues: first={q1:?} second={q2:?}");
        // A realistic submit: an empty command buffer plus a semaphore and a
        // fence, i.e. the shape the guest actually submits. Both devices are
        // exercised so a per-device resolution mistake shows up.
        let mut cps = [0u64; 2];
        let mut cbs: [VkCommandBuffer; 2] = [ptr::null_mut(); 2];
        for (i, dev_i) in [dev, dev2].iter().enumerate() {
            let cpci = VkCommandPoolCreateInfo {
                sType: VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                pNext: ptr::null(),
                flags: VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                queueFamilyIndex: 0,
            };
            let res = vkCreateCommandPool(*dev_i, &cpci, ptr::null(), &mut cps[i]);
            if res != VK_SUCCESS {
                logf!("device {i}: vkCreateCommandPool -> {res}");
                continue;
            }
            let cai = VkCommandBufferAllocateInfo {
                sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                pNext: ptr::null(),
                commandPool: cps[i],
                level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                commandBufferCount: 1,
            };
            let res = vkAllocateCommandBuffers(*dev_i, &cai, &mut cbs[i]);
            if res != VK_SUCCESS {
                logf!("device {i}: vkAllocateCommandBuffers -> {res}");
                continue;
            }
            let begin = VkCommandBufferBeginInfo {
                sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                pNext: ptr::null(),
                flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
                pInheritanceInfo: ptr::null(),
            };
            vkBeginCommandBuffer(cbs[i], &begin);
            vkEndCommandBuffer(cbs[i]);
            logf!("device {i}: cmd buffer {cbs:?}", cbs = cbs[i]);
        }
        let mut fence: VkFence = 0;
        let fci = VkFenceCreateInfo {
            sType: VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
        };
        vkCreateFence(dev2, &fci, ptr::null(), &mut fence);
        let csi = VkSemaphoreCreateInfo {
            sType: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
            pNext: ptr::null(),
            flags: 0,
        };
        let mut sem: VkSemaphore = 0;
        vkCreateSemaphore(dev2, &csi, ptr::null(), &mut sem);
        logf!("fence={fence} sem={sem}");

        let cb_infos = [
            VkCommandBufferSubmitInfo {
                sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO,
                pNext: ptr::null(),
                commandBuffer: cbs[0],
                deviceMask: 0,
            },
            VkCommandBufferSubmitInfo {
                sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO,
                pNext: ptr::null(),
                commandBuffer: cbs[1],
                deviceMask: 0,
            },
        ];
        let wait_info = VkSemaphoreSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO,
            pNext: ptr::null(),
            semaphore: sem,
            value: 0,
            stageMask: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT as VkPipelineStageFlags2,
            deviceIndex: 0,
        };
        let signal_info = VkSemaphoreSubmitInfo {
            sType: VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO,
            pNext: ptr::null(),
            semaphore: sem,
            value: 0,
            stageMask: VK_PIPELINE_STAGE_ALL_COMMANDS_BIT as VkPipelineStageFlags2,
            deviceIndex: 0,
        };
        let mut s2 = VkSubmitInfo2 {
            sType: VK_STRUCTURE_TYPE_SUBMIT_INFO_2,
            pNext: ptr::null(),
            flags: 0,
            waitSemaphoreInfoCount: 1,
            pWaitSemaphoreInfos: &wait_info as *const _ as *const c_void,
            commandBufferInfoCount: 1,
            pCommandBufferInfos: &cb_infos[0],
            signalSemaphoreInfoCount: 1,
            pSignalSemaphoreInfos: &signal_info as *const _ as *const c_void,
        };
        logf!("submitting its own command buffer on the first device's queue ...");
        let r1 = vkQueueSubmit2(q1, 1, &s2, 0);
        logf!("submit2 on first device's queue -> {r1}");
        s2.pCommandBufferInfos = &cb_infos[1];
        logf!("submitting its own command buffer on the second device's queue ...");
        let r2 = vkQueueSubmit2(q2, 1, &s2, fence);
        logf!("submit2 on second device's queue -> {r2}");
        logf!("done");
    }
    0
}

fn main() {
    std::process::exit(run());
}
