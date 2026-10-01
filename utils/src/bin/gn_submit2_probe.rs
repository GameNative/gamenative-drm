//! gn-submit2-probe — reproduce the guest's freeze without the game or Gamescope.
//!
//! The game's last act before going silent is a `vkWaitSemaphoresKHR` on two
//! semaphores: a timeline value it has already asked `vkQueueSubmit2` to signal,
//! plus a second one at value 0. That wait never returns. The scene suite only
//! ever submits with `vkQueueSubmit`, so nothing covered this path.
//!
//! This probe is that path in isolation: a deep queue of submit2 calls, each
//! signalling a timeline, then the same two-semaphore blocking wait.

#![allow(non_snake_case, non_camel_case_types)]

use gn_utils::vk::*;
use std::ffi::{c_char, c_void};
use std::ptr;

macro_rules! logf {
    ($fmt:literal $(, $($arg:tt)*)?) => {
        eprintln!("gn-submit2-probe: {}", format_args!($fmt $(, $($arg)*)?))
    };
}

const VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO: i32 = 1000207002;
const VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO: i32 = 1000207004;
const VK_SEMAPHORE_TYPE_TIMELINE: i32 = 1;

#[repr(C)]
struct VkSemaphoreTypeCreateInfo {
    sType: i32,
    pNext: *const c_void,
    semaphoreType: i32,
    initialValue: u64,
}

#[repr(C)]
struct VkSemaphoreWaitInfo {
    sType: i32,
    pNext: *const c_void,
    flags: u32,
    semaphoreCount: u32,
    pSemaphores: *const VkSemaphore,
    pValues: *const u64,
}

type PFN_vkWaitSemaphores =
    unsafe extern "C" fn(VkDevice, *const VkSemaphoreWaitInfo, u64) -> VkResult;
type PFN_vkGetSemaphoreCounterValue =
    unsafe extern "C" fn(VkDevice, VkSemaphore, *mut u64) -> VkResult;

const DEPTH: usize = 512;
/// Real work per submission: an empty command buffer completes instantly, so the
/// first version of this probe passed trivially. 4 MB a frame gives the queue
/// actual transfers while keeping the total finite.
const COPY_BYTES: usize = 4 << 20;

fn run() -> i32 {
    unsafe {
        let app = VkApplicationInfo {
            sType: VK_STRUCTURE_TYPE_APPLICATION_INFO,
            pNext: ptr::null(),
            pApplicationName: c"gn-submit2-probe".as_ptr(),
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
        if vkCreateInstance(&ici, ptr::null(), &mut inst) != VK_SUCCESS {
            logf!("vkCreateInstance failed");
            return 1;
        }
        let mut n: u32 = 0;
        vkEnumeratePhysicalDevices(inst, &mut n, ptr::null_mut());
        let mut pds = vec![ptr::null_mut::<c_void>(); n as usize];
        vkEnumeratePhysicalDevices(inst, &mut n, pds.as_mut_ptr());
        let pd = pds[0];

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
            enabledExtensionCount: 0,
            ppEnabledExtensionNames: ptr::null(),
            pEnabledFeatures: ptr::null(),
        };
        let mut dev: VkDevice = ptr::null_mut();
        if vkCreateDevice(pd, &dci, ptr::null(), &mut dev) != VK_SUCCESS {
            logf!("vkCreateDevice failed");
            return 1;
        }
        let mut q: VkQueue = ptr::null_mut();
        vkGetDeviceQueue(dev, 0, 0, &mut q);

        // Timeline semaphores, as DXVK creates them.
        let make_timeline = |dev: VkDevice| -> VkSemaphore {
            let tci = VkSemaphoreTypeCreateInfo {
                sType: VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO,
                pNext: ptr::null(),
                semaphoreType: VK_SEMAPHORE_TYPE_TIMELINE,
                initialValue: 0,
            };
            let csi = VkSemaphoreCreateInfo {
                sType: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
                pNext: &tci as *const _ as *const c_void,
                flags: 0,
            };
            let mut s: VkSemaphore = 0;
            let r = vkCreateSemaphore(dev, &csi, ptr::null(), &mut s);
            logf!("timeline semaphore -> {r} handle={s}");
            s
        };
        let t1 = make_timeline(dev);
        let t2 = make_timeline(dev);
        if t1 == 0 || t2 == 0 {
            return 1;
        }

        let cpci = VkCommandPoolCreateInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
            pNext: ptr::null(),
            flags: VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
            queueFamilyIndex: 0,
        };
        let mut cp = 0u64;
        vkCreateCommandPool(dev, &cpci, ptr::null(), &mut cp);
        // Real GPU work: two buffers and a copy per submission.
        let mut mprops = std::mem::zeroed::<VkPhysicalDeviceMemoryProperties>();
        vkGetPhysicalDeviceMemoryProperties(pd, &mut mprops);
        let pick = |want: u32| -> u32 {
            for i in 0..mprops.memoryTypeCount {
                if mprops.memoryTypes[i as usize].propertyFlags & want == want {
                    return i;
                }
            }
            0
        };
        let src_type =
            pick(VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
        let dst_type = pick(VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
        logf!("memory types: src={src_type} dst={dst_type}");
        let mk_buf = |usage: u32| -> VkBuffer {
            let bci = VkBufferCreateInfo {
                sType: VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,
                size: COPY_BYTES as u64,
                usage,
                sharingMode: VK_SHARING_MODE_EXCLUSIVE,
                queueFamilyIndexCount: 0,
                pQueueFamilyIndices: ptr::null(),
            };
            let mut b: VkBuffer = 0;
            vkCreateBuffer(dev, &bci, ptr::null(), &mut b);
            b
        };
        let src_buf = mk_buf(VK_BUFFER_USAGE_TRANSFER_SRC_BIT);
        let dst_buf = mk_buf(VK_BUFFER_USAGE_TRANSFER_DST_BIT);
        for (b, mtype) in [(src_buf, src_type), (dst_buf, dst_type)] {
            let mut req = std::mem::zeroed::<VkMemoryRequirements>();
            vkGetBufferMemoryRequirements(dev, b, &mut req);
            let mai = VkMemoryAllocateInfo {
                sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                pNext: ptr::null(),
                allocationSize: req.size,
                memoryTypeIndex: mtype,
            };
            let mut mem: VkDeviceMemory = 0;
            let ra = vkAllocateMemory(dev, &mai, ptr::null(), &mut mem);
            let rb = vkBindBufferMemory(dev, b, mem, 0);
            logf!(
                "buffer {b}: mem={mem} alloc={ra} bind={rb} size={}",
                req.size
            );
        }

        let cai = VkCommandBufferAllocateInfo {
            sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            pNext: ptr::null(),
            commandPool: cp,
            level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
            commandBufferCount: DEPTH as u32,
        };
        let mut cbs: Vec<VkCommandBuffer> = vec![ptr::null_mut(); DEPTH];
        vkAllocateCommandBuffers(dev, &cai, cbs.as_mut_ptr());
        let copy = VkBufferCopy {
            srcOffset: 0,
            dstOffset: 0,
            size: COPY_BYTES as u64,
        };
        for cb in cbs.iter() {
            let begin = VkCommandBufferBeginInfo {
                sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                pNext: ptr::null(),
                flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
                pInheritanceInfo: ptr::null(),
            };
            vkBeginCommandBuffer(*cb, &begin);
            vkCmdCopyBuffer(*cb, src_buf, dst_buf, 1, &copy);
            vkEndCommandBuffer(*cb);
        }

        let wait_fn: PFN_vkWaitSemaphores = std::mem::transmute(vkGetDeviceProcAddr(
            dev,
            c"vkWaitSemaphoresKHR".as_ptr() as *const c_char,
        ));
        let counter_fn: PFN_vkGetSemaphoreCounterValue = std::mem::transmute(vkGetDeviceProcAddr(
            dev,
            c"vkGetSemaphoreCounterValueKHR".as_ptr() as *const c_char,
        ));
        logf!(
            "resolved wait={} counter={}",
            wait_fn as usize,
            counter_fn as usize
        );

        // Deep queue: DEPTH submits, cycling the command buffers the way the
        // guest does, signalling the timeline one value per submit, no waits.
        for (i, cb) in cbs.iter().enumerate() {
            let ci = VkCommandBufferSubmitInfo {
                sType: VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO,
                pNext: ptr::null(),
                commandBuffer: *cb,
                deviceMask: 0,
            };
            let sig = VkSemaphoreSubmitInfo {
                sType: VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO,
                pNext: ptr::null(),
                semaphore: t1,
                value: (i + 1) as u64,
                stageMask: VK_PIPELINE_STAGE_ALL_COMMANDS_BIT as VkPipelineStageFlags2,
                deviceIndex: 0,
            };
            let s2 = VkSubmitInfo2 {
                sType: VK_STRUCTURE_TYPE_SUBMIT_INFO_2,
                pNext: ptr::null(),
                flags: 0,
                waitSemaphoreInfoCount: 0,
                pWaitSemaphoreInfos: ptr::null(),
                commandBufferInfoCount: 1,
                pCommandBufferInfos: &ci,
                signalSemaphoreInfoCount: 1,
                pSignalSemaphoreInfos: &sig as *const _ as *const c_void,
            };
            let r = vkQueueSubmit2(q, 1, &s2, 0);
            if r != VK_SUCCESS {
                logf!("submit2 #{i} -> {r} (stopping)");
                break;
            }
        }
        logf!("{DEPTH} submits queued, no waits");

        let mut v: u64 = 0;
        let r = counter_fn(dev, t1, &mut v);
        logf!("counter after queueing: r={r} value={v} (want {DEPTH})");

        // The guest's exact shape: the timeline value it just signalled, plus a
        // second semaphore at value 0, with an infinite timeout.
        let sems = [t1, t2];
        let vals = [DEPTH as u64, 0u64];
        let wi = VkSemaphoreWaitInfo {
            sType: VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO,
            pNext: ptr::null(),
            flags: 0,
            semaphoreCount: 2,
            pSemaphores: sems.as_ptr(),
            pValues: vals.as_ptr(),
        };
        logf!("BLOCKING wait on [t1@{DEPTH}, t2@0] ... (hang here = reproduced)");
        let t0 = std::time::Instant::now();
        let r = wait_fn(dev, &wi, u64::MAX);
        logf!("wait returned r={r} after {}ms", t0.elapsed().as_millis());
        let r = counter_fn(dev, t1, &mut v);
        logf!("counter after wait: r={r} value={v}");
        logf!("done");
    }
    0
}

fn main() {
    std::process::exit(run());
}
