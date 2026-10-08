//! Minimal Vulkan FFI — only what the ICD shim and the smoke-test clients
//! use, transliterated from vulkan_core.h / vk_layer.h / vulkan_wayland.h.
//! Handles are opaque pointers (dispatchable) or u64 (non-dispatchable);
//! no libvulkan at link time for the shim (everything resolves via GIPA).

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::os::raw::{c_char, c_void};

// ------------------------------------------------------------------ handles
pub type VkInstance = *mut c_void;
pub type VkPhysicalDevice = *mut c_void;
pub type VkDevice = *mut c_void;
pub type VkQueue = *mut c_void;
pub type VkCommandBuffer = *mut c_void;
pub type VkImage = u64;
pub type VkDeviceMemory = u64;
pub type VkFence = u64;
pub type VkSemaphore = u64;
pub type VkSurfaceKHR = u64;
pub type VkSwapchainKHR = u64;
pub type VkCommandPool = u64;

// Graphics-pipeline state the shim inspects to count culling pipelines.
pub type VkPipeline = u64;
pub type VkPipelineCache = u64;
pub type VkCullModeFlags = u32;
pub const VK_CULL_MODE_NONE: VkCullModeFlags = 0;
pub const VK_CULL_MODE_BACK_BIT: VkCullModeFlags = 0x2;
pub const VK_CULL_MODE_FRONT_BIT: VkCullModeFlags = 0x1;
pub type VkFrontFace = i32;
pub const VK_FRONT_FACE_COUNTER_CLOCKWISE: VkFrontFace = 0;
pub const VK_FRONT_FACE_CLOCKWISE: VkFrontFace = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPipelineRasterizationStateCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: u32,
    pub depthClampEnable: VkBool32,
    pub rasterizerDiscardEnable: VkBool32,
    pub polygonMode: i32,
    pub cullMode: VkCullModeFlags,
    pub frontFace: VkFrontFace,
    pub depthBiasEnable: VkBool32,
    pub depthBiasConstantFactor: f32,
    pub depthBiasClamp: f32,
    pub depthBiasSlopeFactor: f32,
    pub lineWidth: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkGraphicsPipelineCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: u32,
    pub stageCount: u32,
    pub pStages: *const c_void,
    pub pVertexInputState: *const c_void,
    pub pInputAssemblyState: *const c_void,
    pub pTessellationState: *const c_void,
    pub pViewportState: *const c_void,
    pub pRasterizationState: *const VkPipelineRasterizationStateCreateInfo,
    pub pMultisampleState: *const c_void,
    pub pDepthStencilState: *const c_void,
    pub pColorBlendState: *const c_void,
    pub pDynamicState: *const c_void,
    pub layout: u64,
    pub renderPass: u64,
    pub subpass: u32,
    pub basePipelineHandle: VkPipeline,
    pub basePipelineIndex: i32,
}

pub type PFN_vkCreateGraphicsPipelines = unsafe extern "C" fn(
    VkDevice,
    VkPipelineCache,
    u32,
    *const VkGraphicsPipelineCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkPipeline,
) -> VkResult;

pub const VK_NULL_HANDLE: *mut c_void = std::ptr::null_mut();

pub type VkBool32 = u32;
pub const VK_TRUE: VkBool32 = 1;
pub const VK_FALSE: VkBool32 = 0;

pub type VkResult = i32;
pub const VK_SUCCESS: VkResult = 0;
pub const VK_NOT_READY: VkResult = 1;
pub const VK_TIMEOUT: VkResult = 2;
pub const VK_SUBOPTIMAL_KHR: VkResult = 1000001003;
pub const VK_INCOMPLETE: VkResult = 5;
pub const VK_ERROR_OUT_OF_HOST_MEMORY: VkResult = -1;
pub const VK_ERROR_INITIALIZATION_FAILED: VkResult = -3;
pub const VK_ERROR_FEATURE_NOT_PRESENT: VkResult = -8;
pub const VK_ERROR_FORMAT_NOT_SUPPORTED: VkResult = -11;
pub const VK_ERROR_SURFACE_LOST_KHR: VkResult = -1000000000;
pub const VK_ERROR_OUT_OF_DATE_KHR: VkResult = -1000001004;

pub const VK_API_VERSION_1_3: u32 = 1 << 22 | 3 << 12;

// ------------------------------------------------------------ enum values
pub type VkStructureType = i32;
pub const VK_STRUCTURE_TYPE_APPLICATION_INFO: VkStructureType = 0;
pub const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: VkStructureType = 1;
pub const VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO: VkStructureType = 2;
pub const VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO: VkStructureType = 3;
pub const VK_STRUCTURE_TYPE_SUBMIT_INFO: VkStructureType = 4;
pub const VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO: VkStructureType = 5;
pub const VK_STRUCTURE_TYPE_FENCE_CREATE_INFO: VkStructureType = 8;
pub const VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO: VkStructureType = 9;
pub const VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO: VkStructureType = 14;
pub const VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO: VkStructureType = 39;
pub const VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO: VkStructureType = 42;
pub const VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO: VkStructureType = 40;
pub const VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER: VkStructureType = 45;
pub const VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO: VkStructureType = 47;
pub const VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO: VkStructureType = 48;
pub const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2: VkStructureType = 1000059000;
pub const VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2: VkStructureType = 1000059002;
pub const VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR: VkStructureType = 1000001000;
pub const VK_STRUCTURE_TYPE_PRESENT_INFO_KHR: VkStructureType = 1000001001;
pub const VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR: VkStructureType = 1000006000;
pub const VK_STRUCTURE_TYPE_XLIB_SURFACE_CREATE_INFO_KHR: VkStructureType = 1000004000;
pub const VK_STRUCTURE_TYPE_ACQUIRE_NEXT_IMAGE_INFO_KHR: VkStructureType = 1000060010;
pub const VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO: VkStructureType = 1000072001;
pub const VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO: VkStructureType = 1000072002;
pub const VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR: VkStructureType = 1000074000;
pub const VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR: VkStructureType = 1000074002;
pub const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO: VkStructureType =
    1000071000;
pub const VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO: VkStructureType = 1000127001;
pub const VK_STRUCTURE_TYPE_DEVICE_QUEUE_INFO_2: VkStructureType = 1000145003;
pub const VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT: VkStructureType = 1000158000;
pub const VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT: VkStructureType =
    1000158004;
pub const VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT: VkStructureType =
    1000158003;
pub const VK_STRUCTURE_TYPE_SUBMIT_INFO_2: VkStructureType = 1000314004;
pub const VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO: VkStructureType = 1000314006;
// Physical-device image-format query (the modifier-support instrument):
// VkPhysicalDeviceImageFormatInfo2 / VkImageFormatProperties2 and the DRM
// format-modifier pNext. Read-only diagnostics — nothing in the create path
// consults these; they exist so a session can record the driver's answer.
pub const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2: VkStructureType = 1000059004;
pub const VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2: VkStructureType = 1000059003;
pub const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT: VkStructureType =
    1000158002;
// The driver's answer for the modifier of an image created with
// VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT: `vkGetImageDrmFormatModifierPropertiesEXT`.
// Valid usage (VUID-vkGetImageDrmFormatModifierPropertiesEXT-image-02272) requires
// the image to have been created with that tiling — which is exactly why the slot
// image uses the DRM tiling form rather than plain VK_IMAGE_TILING_OPTIMAL.
pub const VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_PROPERTIES_EXT: VkStructureType = 1000158005;

pub const VK_LAYER_LINK_INFO: i32 = 0; // VkLayerFunction

pub type VkFormat = i32;
pub const VK_FORMAT_R8G8B8A8_UNORM: VkFormat = 37;
pub const VK_FORMAT_B8G8R8A8_UNORM: VkFormat = 44;

pub type VkColorSpaceKHR = i32;
pub const VK_COLOR_SPACE_SRGB_NONLINEAR_KHR: VkColorSpaceKHR = 0;

pub type VkImageType = i32;
pub const VK_IMAGE_TYPE_2D: VkImageType = 1;

pub type VkImageTiling = i32;
pub const VK_IMAGE_TILING_OPTIMAL: VkImageTiling = 0;
pub const VK_IMAGE_TILING_LINEAR: VkImageTiling = 1;
pub const VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT: VkImageTiling = 1000158000;

pub type VkImageUsageFlags = u32;
pub const VK_IMAGE_USAGE_TRANSFER_SRC_BIT: VkImageUsageFlags = 0x1;
pub const VK_IMAGE_USAGE_TRANSFER_DST_BIT: VkImageUsageFlags = 0x2;
pub const VK_IMAGE_USAGE_SAMPLED_BIT: VkImageUsageFlags = 0x4;
pub const VK_IMAGE_USAGE_STORAGE_BIT: VkImageUsageFlags = 0x8;
pub const VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT: VkImageUsageFlags = 0x10;

pub type VkSharingMode = i32;
pub const VK_SHARING_MODE_EXCLUSIVE: VkSharingMode = 0;

pub type VkImageLayout = i32;
pub const VK_IMAGE_LAYOUT_UNDEFINED: VkImageLayout = 0;
pub const VK_IMAGE_LAYOUT_GENERAL: VkImageLayout = 1;
pub const VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL: VkImageLayout = 7;
pub const VK_IMAGE_LAYOUT_PRESENT_SRC_KHR: VkImageLayout = 1000001002;

pub type VkSampleCountFlagBits = u32;
pub const VK_SAMPLE_COUNT_1_BIT: VkSampleCountFlagBits = 1;

pub type VkExternalMemoryHandleTypeFlags = u32;
pub const VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT: VkExternalMemoryHandleTypeFlags = 0x200;

pub type VkSurfaceTransformFlagBitsKHR = u32;
pub const VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR: VkSurfaceTransformFlagBitsKHR = 1;

pub type VkCompositeAlphaFlagBitsKHR = u32;
pub const VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR: VkCompositeAlphaFlagBitsKHR = 0x1;
pub const VK_COMPOSITE_ALPHA_PRE_MULTIPLIED_BIT_KHR: VkCompositeAlphaFlagBitsKHR = 0x2;
pub const VK_COMPOSITE_ALPHA_POST_MULTIPLIED_BIT_KHR: VkCompositeAlphaFlagBitsKHR = 0x4;
pub const VK_COMPOSITE_ALPHA_INHERIT_BIT_KHR: VkCompositeAlphaFlagBitsKHR = 0x8;

pub type VkPresentModeKHR = i32;
pub const VK_PRESENT_MODE_IMMEDIATE_KHR: VkPresentModeKHR = 0;
pub const VK_PRESENT_MODE_MAILBOX_KHR: VkPresentModeKHR = 1;
pub const VK_PRESENT_MODE_FIFO_KHR: VkPresentModeKHR = 2;
pub const VK_PRESENT_MODE_FIFO_RELAXED_KHR: VkPresentModeKHR = 3;

pub type VkPipelineStageFlags = u32;
pub const VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT: VkPipelineStageFlags = 0x1;
pub const VK_PIPELINE_STAGE_TRANSFER_BIT: VkPipelineStageFlags = 0x1000;
pub const VK_PIPELINE_STAGE_ALL_COMMANDS_BIT: VkPipelineStageFlags = 0x10000;

pub type VkAccessFlags = u32;
pub const VK_ACCESS_TRANSFER_WRITE_BIT: VkAccessFlags = 0x1000;

pub type VkImageAspectFlags = u32;
pub const VK_IMAGE_ASPECT_COLOR_BIT: VkImageAspectFlags = 0x1;

pub type VkCommandBufferLevel = i32;
pub const VK_COMMAND_BUFFER_LEVEL_PRIMARY: VkCommandBufferLevel = 0;

pub type VkCommandBufferUsageFlags = u32;
pub const VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT: VkCommandBufferUsageFlags = 0x1;

pub type VkCommandPoolCreateFlags = u32;
pub const VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT: VkCommandPoolCreateFlags = 0x2;

pub type VkQueueFlags = u32;
pub const VK_QUEUE_GRAPHICS_BIT: VkQueueFlags = 0x1;

pub type VkFormatFeatureFlags = u32;
pub const VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT: VkFormatFeatureFlags = 0x80;

pub const VK_QUEUE_FAMILY_IGNORED: u32 = u32::MAX;

pub type VkDeviceQueueCreateFlags = u32;
pub type VkDeviceCreateFlags = u32;
pub type VkInstanceCreateFlags = u32;
pub type VkFenceCreateFlags = u32;
pub type VkSemaphoreCreateFlags = u32;
pub type VkCommandPoolResetFlags = u32;
pub type VkDependencyFlags = u32;
pub type VkMemoryMapFlags = u32;
pub type VkImageCreateFlags = u32;
pub type VkCommandBufferResetFlags = u32;
pub type VkFenceImportFlags = u32;
pub type VkSemaphoreImportFlags = u32;
pub type VkDeviceSize = u64;
pub type VkMemoryPropertyFlags = u32;
pub type VkMemoryHeapFlags = u32;
pub const VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT: VkMemoryPropertyFlags = 0x1;

pub const VK_KHR_SURFACE_EXTENSION_NAME: &str = "VK_KHR_surface";
pub const VK_KHR_WAYLAND_SURFACE_EXTENSION_NAME: &str = "VK_KHR_wayland_surface";
pub const VK_KHR_SWAPCHAIN_EXTENSION_NAME: &str = "VK_KHR_swapchain";
pub const VK_KHR_EXTERNAL_MEMORY_EXTENSION_NAME: &str = "VK_KHR_external_memory";
pub const VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME: &str = "VK_KHR_external_memory_fd";
pub const VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME: &str = "VK_EXT_external_memory_dma_buf";
pub const VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME: &str =
    "VK_EXT_image_drm_format_modifier";

// --------------------------------------------------------------- structs
#[repr(C)]
pub struct VkBaseOutStructure {
    pub sType: VkStructureType,
    pub pNext: *mut VkBaseOutStructure,
}

#[repr(C)]
pub struct VkApplicationInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub pApplicationName: *const c_char,
    pub applicationVersion: u32,
    pub pEngineName: *const c_char,
    pub engineVersion: u32,
    pub apiVersion: u32,
}

#[repr(C)]
pub struct VkInstanceCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: VkInstanceCreateFlags,
    pub pApplicationInfo: *const VkApplicationInfo,
    pub enabledLayerCount: u32,
    pub ppEnabledLayerNames: *const *const c_char,
    pub enabledExtensionCount: u32,
    pub ppEnabledExtensionNames: *const *const c_char,
}

pub const VK_MAX_EXTENSION_NAME_SIZE: usize = 256;
pub const VK_MAX_PHYSICAL_DEVICE_NAME_SIZE: usize = 256;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkExtensionProperties {
    pub extensionName: [c_char; VK_MAX_EXTENSION_NAME_SIZE],
    pub specVersion: u32,
}

// vk_layer.h chain links (VkLayerInstanceLink / VkLayerDeviceLink).
pub type PfnVkVoidFunction = Option<unsafe extern "C" fn()>;

#[repr(C)]
pub struct VkLayerInstanceLink {
    pub pNext: *mut VkBaseOutStructure,
    pub pfnNextGetInstanceProcAddr: PfnVkVoidFunction,
    pub pfnNextGetPhysicalDeviceProcAddr: PfnVkVoidFunction,
}

#[repr(C)]
pub struct VkLayerDeviceLink {
    pub pNext: *mut VkBaseOutStructure,
    pub pfnNextGetInstanceProcAddr: PfnVkVoidFunction,
    pub pfnNextGetDeviceProcAddr: PfnVkVoidFunction,
}

#[repr(C)]
pub union VkLayerFunction_u {
    pub pLayerInfo: *mut VkLayerInstanceLink,
    pub pLayerDeviceInfo: *mut VkLayerDeviceLink,
}

#[repr(C)]
pub struct VkLayerInstanceCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *mut VkBaseOutStructure,
    pub function: i32,
    pub u: VkLayerFunction_u,
}

#[repr(C)]
pub struct VkLayerDeviceCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *mut VkBaseOutStructure,
    pub function: i32,
    pub u: VkLayerFunction_u,
}

#[repr(C)]
pub struct VkAllocationCallbacks {
    pub pUserData: *mut c_void,
    pub pfnAllocation: *mut c_void,
    pub pfnReallocation: *mut c_void,
    pub pfnFree: *mut c_void,
    pub pfnInternalAllocation: *mut c_void,
    pub pfnInternalFree: *mut c_void,
}

#[repr(C)]
pub struct VkDeviceQueueCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: VkDeviceQueueCreateFlags,
    pub queueFamilyIndex: u32,
    pub queueCount: u32,
    pub pQueuePriorities: *const f32,
}

#[repr(C)]
pub struct VkDeviceCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: VkDeviceCreateFlags,
    pub queueCreateInfoCount: u32,
    pub pQueueCreateInfos: *const VkDeviceQueueCreateInfo,
    pub enabledLayerCount: u32,
    pub ppEnabledLayerNames: *const *const c_char,
    pub enabledExtensionCount: u32,
    pub ppEnabledExtensionNames: *const *const c_char,
    pub pEnabledFeatures: *const c_void,
}

#[repr(C)]
pub struct VkDeviceQueueInfo2 {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: VkDeviceQueueCreateFlags,
    pub queueFamilyIndex: u32,
    pub queueIndex: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkExtent2D {
    pub width: u32,
    pub height: u32,
}

/// `VkPhysicalDeviceSurfaceInfo2KHR`: what a caller asks surface capabilities
/// *for* in the `2` variant of the query. VKD3D-Proton (D3D12) goes through this
/// form where DXVK (D3D11) uses the plain one.
#[repr(C)]
pub struct VkPhysicalDeviceSurfaceInfo2KHR {
    pub sType: u32,
    pub pNext: *const c_void,
    pub surface: VkSurfaceKHR,
}

/// `VkSurfaceCapabilities2KHR`: the `2` variant's out-structure. Any further
/// structures a caller chains are left untouched: this implementation fills only
/// what it supports, which is the contract.
#[repr(C)]
pub struct VkSurfaceCapabilities2KHR {
    pub sType: u32,
    pub pNext: *mut c_void,
    pub surfaceCapabilities: VkSurfaceCapabilitiesKHR,
}

#[repr(C)]
pub struct VkSurfaceCapabilitiesKHR {
    pub minImageCount: u32,
    pub maxImageCount: u32,
    pub currentExtent: VkExtent2D,
    pub minImageExtent: VkExtent2D,
    pub maxImageExtent: VkExtent2D,
    pub maxImageArrayLayers: u32,
    pub supportedTransforms: VkSurfaceTransformFlagBitsKHR,
    pub currentTransform: VkSurfaceTransformFlagBitsKHR,
    pub supportedCompositeAlpha: VkCompositeAlphaFlagBitsKHR,
    pub supportedUsageFlags: VkImageUsageFlags,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkSurfaceFormatKHR {
    pub format: VkFormat,
    pub colorSpace: VkColorSpaceKHR,
}

#[repr(C)]
pub struct VkWaylandSurfaceCreateInfoKHR {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: u32,
    pub display: *mut c_void, // struct wl_display *
    pub surface: *mut c_void, // struct wl_surface *
}

#[repr(C)]
pub struct VkXlibSurfaceCreateInfoKHR {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: u32,
    pub dpy: *mut c_void, // Display *
    pub window: usize,    // XID Window (unsigned long)
}

#[repr(C)]
pub struct VkSubresourceLayout {
    pub offset: VkDeviceSize,
    pub size: VkDeviceSize,
    pub rowPitch: VkDeviceSize,
    pub arrayPitch: VkDeviceSize,
    pub depthPitch: VkDeviceSize,
}

#[repr(C)]
pub struct VkImageDrmFormatModifierExplicitCreateInfoEXT {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub drmFormatModifier: u64,
    pub drmFormatModifierPlaneCount: u32,
    pub pPlaneLayouts: *const VkSubresourceLayout,
}

#[repr(C)]
pub struct VkImageDrmFormatModifierListCreateInfoEXT {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub drmFormatModifierCount: u32,
    pub pDrmFormatModifiers: *const u64,
}

#[repr(C)]
pub struct VkExternalMemoryImageCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub handleTypes: VkExternalMemoryHandleTypeFlags,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkExtent3D {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
}

#[repr(C)]
pub struct VkImageCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: VkImageCreateFlags,
    pub imageType: VkImageType,
    pub format: VkFormat,
    pub extent: VkExtent3D,
    pub mipLevels: u32,
    pub arrayLayers: u32,
    pub samples: VkSampleCountFlagBits,
    pub tiling: VkImageTiling,
    pub usage: VkImageUsageFlags,
    pub sharingMode: VkSharingMode,
    pub queueFamilyIndexCount: u32,
    pub pQueueFamilyIndices: *const u32,
    pub initialLayout: VkImageLayout,
}

#[repr(C)]
pub struct VkMemoryRequirements {
    pub size: VkDeviceSize,
    pub alignment: VkDeviceSize,
    pub memoryTypeBits: u32,
}

#[repr(C)]
pub struct VkMemoryDedicatedAllocateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub image: VkImage,
    pub buffer: u64,
}

#[repr(C)]
pub struct VkImportMemoryFdInfoKHR {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub handleType: VkExternalMemoryHandleTypeFlags,
    pub fd: i32,
}

#[repr(C)]
pub struct VkExportMemoryAllocateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub handleTypes: VkExternalMemoryHandleTypeFlags,
}

#[repr(C)]
pub struct VkMemoryGetFdInfoKHR {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub memory: VkDeviceMemory,
    pub handleType: VkExternalMemoryHandleTypeFlags,
}

#[repr(C)]
pub struct VkMemoryAllocateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub allocationSize: VkDeviceSize,
    pub memoryTypeIndex: u32,
}

#[repr(C)]
pub struct VkImageSubresource {
    pub aspectMask: VkImageAspectFlags,
    pub mipLevel: u32,
    pub arrayLayer: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageSubresourceRange {
    pub aspectMask: VkImageAspectFlags,
    pub baseMipLevel: u32,
    pub levelCount: u32,
    pub baseArrayLayer: u32,
    pub layerCount: u32,
}

#[repr(C)]
pub struct VkFenceCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: VkFenceCreateFlags,
}

#[repr(C)]
pub struct VkSemaphoreCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: VkSemaphoreCreateFlags,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkSemaphoreWaitInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: u32,
    pub semaphoreCount: u32,
    pub pSemaphores: *const VkSemaphore,
    pub pValues: *const u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkSemaphoreSubmitInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub semaphore: VkSemaphore,
    pub value: u64,
    pub stageMask: u64,
    pub deviceIndex: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkSubmitInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub waitSemaphoreCount: u32,
    pub pWaitSemaphores: *const VkSemaphore,
    pub pWaitDstStageMask: *const VkPipelineStageFlags,
    pub commandBufferCount: u32,
    pub pCommandBuffers: *const VkCommandBuffer,
    pub signalSemaphoreCount: u32,
    pub pSignalSemaphores: *const VkSemaphore,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkCommandBufferSubmitInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub commandBuffer: VkCommandBuffer,
    pub deviceMask: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkSubmitInfo2 {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: u32,
    pub waitSemaphoreInfoCount: u32,
    pub pWaitSemaphoreInfos: *const c_void,
    pub commandBufferInfoCount: u32,
    pub pCommandBufferInfos: *const VkCommandBufferSubmitInfo,
    pub signalSemaphoreInfoCount: u32,
    pub pSignalSemaphoreInfos: *const c_void,
}

#[repr(C)]
pub struct VkPresentInfoKHR {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub waitSemaphoreCount: u32,
    pub pWaitSemaphores: *const VkSemaphore,
    pub swapchainCount: u32,
    pub pSwapchains: *const VkSwapchainKHR,
    pub pImageIndices: *const u32,
    pub pResults: *mut VkResult,
}

#[repr(C)]
pub struct VkAcquireNextImageInfoKHR {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub swapchain: VkSwapchainKHR,
    pub timeout: u64,
    pub semaphore: VkSemaphore,
    pub fence: VkFence,
    pub deviceMask: u32,
}

#[repr(C)]
pub struct VkSwapchainCreateInfoKHR {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: u32,
    pub surface: VkSurfaceKHR,
    pub minImageCount: u32,
    pub imageFormat: VkFormat,
    pub imageColorSpace: VkColorSpaceKHR,
    pub imageExtent: VkExtent2D,
    pub imageArrayLayers: u32,
    pub imageUsage: VkImageUsageFlags,
    pub imageSharingMode: VkSharingMode,
    pub queueFamilyIndexCount: u32,
    pub pQueueFamilyIndices: *const u32,
    pub preTransform: VkSurfaceTransformFlagBitsKHR,
    pub compositeAlpha: VkCompositeAlphaFlagBitsKHR,
    pub presentMode: VkPresentModeKHR,
    pub clipped: VkBool32,
    pub oldSwapchain: VkSwapchainKHR,
}

#[repr(C)]
pub struct VkCommandBufferAllocateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub commandPool: VkCommandPool,
    pub level: VkCommandBufferLevel,
    pub commandBufferCount: u32,
}

#[repr(C)]
pub struct VkCommandBufferBeginInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: VkCommandBufferUsageFlags,
    pub pInheritanceInfo: *const c_void,
}

#[repr(C)]
pub struct VkCommandPoolCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: VkCommandPoolCreateFlags,
    pub queueFamilyIndex: u32,
}

#[repr(C)]
pub struct VkClearColorValue {
    pub float32: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkOffset3D {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

#[repr(C)]
pub struct VkImageMemoryBarrier {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub srcAccessMask: VkAccessFlags,
    pub dstAccessMask: VkAccessFlags,
    pub oldLayout: VkImageLayout,
    pub newLayout: VkImageLayout,
    pub srcQueueFamilyIndex: u32,
    pub dstQueueFamilyIndex: u32,
    pub image: VkImage,
    pub subresourceRange: VkImageSubresourceRange,
}

// Layout must match the real 824-byte header struct (limits is 8-aligned,
// so there are 4 pad bytes after pipelineCacheUUID): the driver writes the
// whole struct even though only deviceName is read.
#[repr(C, align(8))]
pub struct VkPhysicalDeviceProperties {
    pub apiVersion: u32,
    pub driverVersion: u32,
    pub vendorID: u32,
    pub deviceID: u32,
    pub deviceType: i32,
    pub deviceName: [c_char; VK_MAX_PHYSICAL_DEVICE_NAME_SIZE],
    pub pipelineCacheUUID: [u8; 16],
    pub _pad0: [u8; 4],
    pub limits: [u8; 504],          // VkPhysicalDeviceLimits (unused fields)
    pub sparseProperties: [u8; 20], // VkPhysicalDeviceSparseProperties
    pub _pad1: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkQueueFamilyProperties {
    pub queueFlags: VkQueueFlags,
    pub queueCount: u32,
    pub timestampValidBits: u32,
    pub minImageTransferGranularity: VkExtent3D,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkMemoryType {
    pub propertyFlags: VkMemoryPropertyFlags,
    pub heapIndex: u32,
}

pub const VK_MAX_MEMORY_TYPES: usize = 32;
pub const VK_MAX_MEMORY_HEAPS: usize = 16;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkMemoryHeap {
    pub size: VkDeviceSize,
    pub flags: VkMemoryHeapFlags,
}

#[repr(C)]
pub struct VkPhysicalDeviceMemoryProperties {
    pub memoryTypeCount: u32,
    pub memoryTypes: [VkMemoryType; VK_MAX_MEMORY_TYPES],
    pub memoryHeapCount: u32,
    pub memoryHeaps: [VkMemoryHeap; VK_MAX_MEMORY_HEAPS],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkDrmFormatModifierPropertiesEXT {
    pub drmFormatModifier: u64,
    pub drmFormatModifierPlaneCount: u32,
    pub drmFormatModifierTilingFeatures: VkFormatFeatureFlags,
}

#[repr(C)]
pub struct VkDrmFormatModifierPropertiesListEXT {
    pub sType: VkStructureType,
    pub pNext: *mut c_void,
    pub drmFormatModifierCount: u32,
    pub pDrmFormatModifierProperties: *mut VkDrmFormatModifierPropertiesEXT,
}

#[repr(C)]
pub struct VkFormatProperties2 {
    pub sType: VkStructureType,
    pub pNext: *mut c_void,
    pub formatProperties: [u8; 12], // VkFormatProperties (unused fields)
}

// ---- physical-device image-format query (modifier-support instrument) ----
// Only ever populated/read by the read-only `modq` probe; the image-create
// path does not consult any of these.

/// `VkPhysicalDeviceImageFormatInfo2` pNext entry naming a DRM format
/// modifier the caller wants the driver to consider.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPhysicalDeviceImageDrmFormatModifierInfoEXT {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub drmFormatModifier: u64,
    pub sharingMode: VkSharingMode,
    pub queueFamilyIndexCount: u32,
    pub pQueueFamilyIndices: *const u32,
}

/// Input to `vkGetPhysicalDeviceImageFormatProperties2`. `type_` is the
/// Vulkan `type` field (`type` is a Rust keyword; the C layout is unchanged).
#[repr(C)]
pub struct VkPhysicalDeviceImageFormatInfo2 {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub format: VkFormat,
    pub type_: VkImageType,
    pub tiling: VkImageTiling,
    pub usage: VkImageUsageFlags,
    pub flags: VkImageCreateFlags,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageFormatProperties {
    pub maxExtent: VkExtent3D,
    pub maxMipLevels: u32,
    pub maxArrayLayers: u32,
    pub sampleCounts: VkSampleCountFlagBits,
    pub maxResourceSize: VkDeviceSize,
}

#[repr(C)]
pub struct VkImageFormatProperties2 {
    pub sType: VkStructureType,
    pub pNext: *mut c_void,
    pub imageFormatProperties: VkImageFormatProperties,
}

// ----------------------------------------------------- function pointers
pub type PFN_vkVoidFunction = PfnVkVoidFunction;
pub type GipaFn = unsafe extern "C" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction;

pub type PFN_vkCreateInstance = unsafe extern "C" fn(
    *const VkInstanceCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkInstance,
) -> VkResult;
pub type PFN_vkDestroyInstance = unsafe extern "C" fn(VkInstance, *const VkAllocationCallbacks);
pub type PFN_vkEnumeratePhysicalDevices =
    unsafe extern "C" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult;
pub type PFN_vkEnumerateInstanceExtensionProperties =
    unsafe extern "C" fn(*const c_char, *mut u32, *mut VkExtensionProperties) -> VkResult;
pub type PFN_vkEnumerateInstanceVersion = unsafe extern "C" fn(*mut u32) -> VkResult;
pub type PFN_vkEnumerateDeviceExtensionProperties = unsafe extern "C" fn(
    VkPhysicalDevice,
    *const c_char,
    *mut u32,
    *mut VkExtensionProperties,
) -> VkResult;
pub type PFN_vkCreateDevice = unsafe extern "C" fn(
    VkPhysicalDevice,
    *const VkDeviceCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkDevice,
) -> VkResult;
pub type PFN_vkGetDeviceProcAddr =
    unsafe extern "C" fn(VkDevice, *const c_char) -> PFN_vkVoidFunction;

pub type PFN_vkGetDeviceQueue = unsafe extern "C" fn(VkDevice, u32, u32, *mut VkQueue);
pub type PFN_vkGetDeviceQueue2 =
    unsafe extern "C" fn(VkDevice, *const VkDeviceQueueInfo2, *mut VkQueue);
pub type PFN_vkCreateImage = unsafe extern "C" fn(
    VkDevice,
    *const VkImageCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkImage,
) -> VkResult;
pub type PFN_vkDestroyImage = unsafe extern "C" fn(VkDevice, VkImage, *const VkAllocationCallbacks);
pub type PFN_vkGetImageMemoryRequirements =
    unsafe extern "C" fn(VkDevice, VkImage, *mut VkMemoryRequirements);
pub type PFN_vkGetImageSubresourceLayout =
    unsafe extern "C" fn(VkDevice, VkImage, *const VkImageSubresource, *mut VkSubresourceLayout);
pub type PFN_vkAllocateMemory = unsafe extern "C" fn(
    VkDevice,
    *const VkMemoryAllocateInfo,
    *const VkAllocationCallbacks,
    *mut VkDeviceMemory,
) -> VkResult;
pub type PFN_vkFreeMemory =
    unsafe extern "C" fn(VkDevice, VkDeviceMemory, *const VkAllocationCallbacks);
pub type PFN_vkBindImageMemory =
    unsafe extern "C" fn(VkDevice, VkImage, VkDeviceMemory, VkDeviceSize) -> VkResult;
pub type PFN_vkCreateFence = unsafe extern "C" fn(
    VkDevice,
    *const VkFenceCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkFence,
) -> VkResult;
pub type PFN_vkDestroyFence = unsafe extern "C" fn(VkDevice, VkFence, *const VkAllocationCallbacks);
pub type PFN_vkWaitForFences =
    unsafe extern "C" fn(VkDevice, u32, *const VkFence, VkBool32, u64) -> VkResult;
pub type PFN_vkResetFences = unsafe extern "C" fn(VkDevice, u32, *const VkFence) -> VkResult;
pub type PFN_vkCreateSemaphore = unsafe extern "C" fn(
    VkDevice,
    *const VkSemaphoreCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkSemaphore,
) -> VkResult;
pub type PFN_vkDestroySemaphore =
    unsafe extern "C" fn(VkDevice, VkSemaphore, *const VkAllocationCallbacks);
pub type PFN_vkAllocateCommandBuffers = unsafe extern "C" fn(
    VkDevice,
    *const VkCommandBufferAllocateInfo,
    *mut VkCommandBuffer,
) -> VkResult;
pub type PFN_vkFreeCommandBuffers =
    unsafe extern "C" fn(VkDevice, VkCommandPool, u32, *const VkCommandBuffer);
pub type PFN_vkQueueSubmit =
    unsafe extern "C" fn(VkQueue, u32, *const VkSubmitInfo, VkFence) -> VkResult;
pub type PFN_vkQueueSubmit2 =
    unsafe extern "C" fn(VkQueue, u32, *const VkSubmitInfo2, VkFence) -> VkResult;
pub type PFN_vkQueueWaitIdle = unsafe extern "C" fn(VkQueue) -> VkResult;
pub type PFN_vkDeviceWaitIdle = unsafe extern "C" fn(VkDevice) -> VkResult;
pub type PFN_vkDestroyDevice = unsafe extern "C" fn(VkDevice, *const VkAllocationCallbacks);

// Physical-device queries used only by the read-only image-path instrument.
pub type PFN_vkGetPhysicalDeviceImageFormatProperties2 = unsafe extern "C" fn(
    VkPhysicalDevice,
    *const VkPhysicalDeviceImageFormatInfo2,
    *mut VkImageFormatProperties2,
) -> VkResult;
pub type PFN_vkGetPhysicalDeviceMemoryProperties =
    unsafe extern "C" fn(VkPhysicalDevice, *mut VkPhysicalDeviceMemoryProperties);

// `vkGetPhysicalDeviceFormatProperties2` enumerates a format's DRM format
// modifiers through a `VkDrmFormatModifierPropertiesListEXT` out-chain. Used to
// learn which modifiers the driver advertises for a swapchain format, so the
// slot image can offer the driver its own set (the driver then picks its
// preferred layout) instead of a modifier the shim names.
pub type PFN_vkGetPhysicalDeviceFormatProperties2 =
    unsafe extern "C" fn(VkPhysicalDevice, VkFormat, *mut VkFormatProperties2);

// ---- driver read-back (used by the bounded `x11 view` line) --------------
//
// Copy a slot image into a host-visible staging buffer through the driver
// itself, so the driver's own view of the image can be compared with the
// shim's linear mmap.
pub type VkBuffer = u64;
pub type VkBufferUsageFlags = u32;
pub type VkBufferCreateFlags = u32;
pub const VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO: VkStructureType = 12;
pub const VK_BUFFER_USAGE_TRANSFER_SRC_BIT: VkBufferUsageFlags = 0x1;
pub const VK_BUFFER_USAGE_TRANSFER_DST_BIT: VkBufferUsageFlags = 0x2;
pub const VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL: VkImageLayout = 6;
pub const VK_ACCESS_TRANSFER_READ_BIT: VkAccessFlags = 0x800;
pub const VK_ACCESS_MEMORY_READ_BIT: VkAccessFlags = 0x8000;
pub const VK_ACCESS_MEMORY_WRITE_BIT: VkAccessFlags = 0x10000;
pub const VK_PIPELINE_STAGE_HOST_BIT: VkPipelineStageFlags = 0x4000;
pub const VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT: u32 = 0x2;
pub const VK_MEMORY_PROPERTY_HOST_COHERENT_BIT: u32 = 0x4;
pub const VK_WHOLE_SIZE: VkDeviceSize = !0u64;

#[repr(C)]
pub struct VkBufferCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: VkBufferCreateFlags,
    pub size: VkDeviceSize,
    pub usage: VkBufferUsageFlags,
    pub sharingMode: VkSharingMode,
    pub queueFamilyIndexCount: u32,
    pub pQueueFamilyIndices: *const u32,
}

#[repr(C)]
pub struct VkBufferImageCopy {
    pub bufferOffset: VkDeviceSize,
    pub bufferRowLength: u32,
    pub bufferImageHeight: u32,
    /// Four fields (`VkImageSubresourceLayers`), not the three-field
    /// `VkImageSubresource`: the copy target names a layer *count* as well as a
    /// base layer, and typing it short makes `layerCount` read whatever follows
    /// the struct.
    pub imageSubresource: VkImageSubresourceLayers,
    pub imageOffset: VkOffset3D,
    pub imageExtent: VkExtent3D,
}

pub type PFN_vkCreateBuffer = unsafe extern "C" fn(
    VkDevice,
    *const VkBufferCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkBuffer,
) -> VkResult;
pub type PFN_vkDestroyBuffer =
    unsafe extern "C" fn(VkDevice, VkBuffer, *const VkAllocationCallbacks);
pub type PFN_vkGetBufferMemoryRequirements =
    unsafe extern "C" fn(VkDevice, VkBuffer, *mut VkMemoryRequirements);
pub type PFN_vkBindBufferMemory =
    unsafe extern "C" fn(VkDevice, VkBuffer, VkDeviceMemory, VkDeviceSize) -> VkResult;
pub type PFN_vkCreateCommandPool = unsafe extern "C" fn(
    VkDevice,
    *const VkCommandPoolCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkCommandPool,
) -> VkResult;
pub type PFN_vkDestroyCommandPool =
    unsafe extern "C" fn(VkDevice, VkCommandPool, *const VkAllocationCallbacks);
pub type PFN_vkBeginCommandBuffer =
    unsafe extern "C" fn(VkCommandBuffer, *const VkCommandBufferBeginInfo) -> VkResult;
pub type PFN_vkEndCommandBuffer = unsafe extern "C" fn(VkCommandBuffer) -> VkResult;
pub type PFN_vkCmdPipelineBarrier = unsafe extern "C" fn(
    VkCommandBuffer,
    VkPipelineStageFlags,
    VkPipelineStageFlags,
    u32,
    u32,
    *const c_void,
    u32,
    *const c_void,
    u32,
    *const VkImageMemoryBarrier,
);
pub type PFN_vkCmdCopyImageToBuffer = unsafe extern "C" fn(
    VkCommandBuffer,
    VkImage,
    VkImageLayout,
    VkBuffer,
    u32,
    *const VkBufferImageCopy,
);
pub type PFN_vkMapMemory = unsafe extern "C" fn(
    VkDevice,
    VkDeviceMemory,
    VkDeviceSize,
    VkDeviceSize,
    VkMemoryMapFlags,
    *mut *mut c_void,
) -> VkResult;
pub type PFN_vkUnmapMemory = unsafe extern "C" fn(VkDevice, VkDeviceMemory);

pub const VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE: VkStructureType = 6;

#[repr(C)]
pub struct VkMappedMemoryRange {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub memory: VkDeviceMemory,
    pub offset: VkDeviceSize,
    pub size: VkDeviceSize,
}
pub type PFN_vkInvalidateMappedMemoryRanges =
    unsafe extern "C" fn(VkDevice, u32, *const VkMappedMemoryRange) -> VkResult;

// ---------------------------------------- guest-command trace region structs
// Read only by the bounded one-shot `x11 cmd` trace, to name the image and the
// transfer extent a recorded copy/blit/clear targets — never by a present path.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageSubresourceLayers {
    pub aspectMask: VkImageAspectFlags,
    pub mipLevel: u32,
    pub baseArrayLayer: u32,
    pub layerCount: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageCopy {
    pub srcSubresource: VkImageSubresourceLayers,
    pub srcOffset: VkOffset3D,
    pub dstSubresource: VkImageSubresourceLayers,
    pub dstOffset: VkOffset3D,
    pub extent: VkExtent3D,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageBlit {
    pub srcSubresource: VkImageSubresourceLayers,
    pub srcOffsets: [VkOffset3D; 2],
    pub dstSubresource: VkImageSubresourceLayers,
    pub dstOffsets: [VkOffset3D; 2],
}

/// v2 form of [`VkImageCopy`]: the same region preceded by `sType`/`pNext`.
/// [`VkCopyImageInfo2`]`::pRegions` points at an array of these, never at
/// [`VkImageCopy`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageCopy2 {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub srcSubresource: VkImageSubresourceLayers,
    pub srcOffset: VkOffset3D,
    pub dstSubresource: VkImageSubresourceLayers,
    pub dstOffset: VkOffset3D,
    pub extent: VkExtent3D,
}

/// v2 form of [`VkImageBlit`]: the same source and destination regions preceded
/// by `sType`/`pNext`. [`VkBlitImageInfo2`]`::pRegions` points at an array of
/// these, never at [`VkImageBlit`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageBlit2 {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub srcSubresource: VkImageSubresourceLayers,
    pub srcOffsets: [VkOffset3D; 2],
    pub dstSubresource: VkImageSubresourceLayers,
    pub dstOffsets: [VkOffset3D; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkCopyImageInfo2 {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub srcImage: VkImage,
    pub srcImageLayout: VkImageLayout,
    pub dstImage: VkImage,
    pub dstImageLayout: VkImageLayout,
    pub regionCount: u32,
    pub pRegions: *const VkImageCopy2,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkBlitImageInfo2 {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub srcImage: VkImage,
    pub srcImageLayout: VkImageLayout,
    pub dstImage: VkImage,
    pub dstImageLayout: VkImageLayout,
    pub regionCount: u32,
    pub pRegions: *const VkImageBlit2,
    pub filter: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkCopyBufferToImageInfo2 {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub srcBuffer: VkBuffer,
    pub dstImage: VkImage,
    pub dstImageLayout: VkImageLayout,
    pub regionCount: u32,
    pub pRegions: *const VkBufferImageCopy,
}

pub type VkFilter = i32;

pub type PFN_vkCmdClearColorImage = unsafe extern "C" fn(
    VkCommandBuffer,
    VkImage,
    VkImageLayout,
    *const VkClearColorValue,
    u32,
    *const VkImageSubresourceRange,
);
pub type PFN_vkCmdCopyImage = unsafe extern "C" fn(
    VkCommandBuffer,
    VkImage,
    VkImageLayout,
    VkImage,
    VkImageLayout,
    u32,
    *const VkImageCopy,
);
pub type PFN_vkCmdCopyImage2 = unsafe extern "C" fn(VkCommandBuffer, *const VkCopyImageInfo2);
pub type PFN_vkCmdBlitImage = unsafe extern "C" fn(
    VkCommandBuffer,
    VkImage,
    VkImageLayout,
    VkImage,
    VkImageLayout,
    u32,
    *const VkImageBlit,
    VkFilter,
);
pub type PFN_vkCmdBlitImage2 = unsafe extern "C" fn(VkCommandBuffer, *const VkBlitImageInfo2);
pub type PFN_vkCmdCopyBufferToImage = unsafe extern "C" fn(
    VkCommandBuffer,
    VkBuffer,
    VkImage,
    VkImageLayout,
    u32,
    *const VkBufferImageCopy,
);
pub type PFN_vkCmdCopyBufferToImage2 =
    unsafe extern "C" fn(VkCommandBuffer, *const VkCopyBufferToImageInfo2);

// ------------------------------- synchronization2 (vkCmdPipelineBarrier2)
// DXVK 2.7 uses VK_KHR_synchronization2: layout transitions of the swapchain
// image arrive as vkCmdPipelineBarrier2, not vkCmdPipelineBarrier.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageMemoryBarrier2 {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub srcStageMask: u64,
    pub srcAccessMask: u64,
    pub dstStageMask: u64,
    pub dstAccessMask: u64,
    pub oldLayout: VkImageLayout,
    pub newLayout: VkImageLayout,
    pub srcQueueFamilyIndex: u32,
    pub dstQueueFamilyIndex: u32,
    pub image: VkImage,
    pub subresourceRange: VkImageSubresourceRange,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkDependencyInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub dependencyFlags: u32,
    pub memoryBarrierCount: u32,
    pub pMemoryBarriers: *const c_void,
    pub bufferMemoryBarrierCount: u32,
    pub pBufferMemoryBarriers: *const c_void,
    pub imageMemoryBarrierCount: u32,
    pub pImageMemoryBarriers: *const VkImageMemoryBarrier2,
}
pub type PFN_vkCmdPipelineBarrier2 = unsafe extern "C" fn(VkCommandBuffer, *const VkDependencyInfo);

// ------------------------------- render-pass trace (vkCmdBeginRenderPass)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkOffset2D {
    pub x: i32,
    pub y: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkRect2D {
    pub offset: VkOffset2D,
    pub extent: VkExtent2D,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkRenderPassBeginInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub renderPass: u64,
    pub framebuffer: u64,
    pub renderArea: VkRect2D,
    pub clearValueCount: u32,
    pub pClearValues: *const VkClearValue,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkClearValue {
    pub float32: [f32; 4],
}
pub type PFN_vkCmdBeginRenderPass =
    unsafe extern "C" fn(VkCommandBuffer, *const VkRenderPassBeginInfo, i32);

// ------------------------------- view/framebuffer tracking (render-pass target)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkComponentMapping {
    pub r: i32,
    pub g: i32,
    pub b: i32,
    pub a: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageViewCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: u32,
    pub image: VkImage,
    pub viewType: i32,
    pub format: i32,
    pub components: VkComponentMapping,
    pub subresourceRange: VkImageSubresourceRange,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkFramebufferCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: u32,
    pub renderPass: u64,
    pub attachmentCount: u32,
    pub pAttachments: *const u64,
    pub width: u32,
    pub height: u32,
    pub layers: u32,
}
pub type VkImageView = u64;
pub type VkFramebuffer = u64;
pub type PFN_vkCreateImageView = unsafe extern "C" fn(
    VkDevice,
    *const VkImageViewCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkImageView,
) -> VkResult;
pub type PFN_vkCreateFramebuffer = unsafe extern "C" fn(
    VkDevice,
    *const VkFramebufferCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkFramebuffer,
) -> VkResult;

// ------------------------------- dynamic rendering (vkCmdBeginRendering)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkRenderingAttachmentInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub imageView: VkImageView,
    pub imageLayout: VkImageLayout,
    pub resolveMode: i32,
    pub resolveImageView: VkImageView,
    pub resolveImageLayout: VkImageLayout,
    pub loadOp: i32,
    pub storeOp: i32,
    pub clearValue: VkClearValue,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkRenderingInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: u32,
    pub renderArea: VkRect2D,
    pub layerCount: u32,
    pub viewMask: u32,
    pub colorAttachmentCount: u32,
    pub pColorAttachments: *const VkRenderingAttachmentInfo,
    pub pDepthAttachment: *const VkRenderingAttachmentInfo,
    pub pStencilAttachment: *const VkRenderingAttachmentInfo,
}
pub type PFN_vkCmdBeginRendering = unsafe extern "C" fn(VkCommandBuffer, *const VkRenderingInfo);

pub type VkSubpassContents = i32;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkSubpassBeginInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub contents: VkSubpassContents,
}
pub type PFN_vkCmdBeginRenderPass2 =
    unsafe extern "C" fn(VkCommandBuffer, *const VkRenderPassBeginInfo, *const VkSubpassBeginInfo);

// ------------------------------- dynamic state (viewport / scissor) trace
// Intercepted only to report the values the guest sets: a viewport whose
// extent is zero, or whose rect lies outside the swapchain image, clips every
// draw while leaving a loadOp clear intact, which is exactly the
// clears-land-draws-do-not shape being diagnosed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkViewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub minDepth: f32,
    pub maxDepth: f32,
}
pub type PFN_vkCmdSetViewport = unsafe extern "C" fn(VkCommandBuffer, u32, u32, *const VkViewport);
pub type PFN_vkCmdSetViewportWithCount =
    unsafe extern "C" fn(VkCommandBuffer, u32, *const VkViewport);
pub type PFN_vkCmdSetScissor = unsafe extern "C" fn(VkCommandBuffer, u32, u32, *const VkRect2D);
pub type PFN_vkCmdSetScissorWithCount = unsafe extern "C" fn(VkCommandBuffer, u32, *const VkRect2D);

// ------------------------------- render-pass load/store capture
// vkCmdBeginRenderPass names a VkRenderPass, and the attachment loadOp/storeOp
// live in that render pass's subpass description, not in the begin info; the
// shim records them at create time so the begin trace can name them.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkAttachmentDescription {
    pub flags: u32,
    pub format: i32,
    pub samples: u32,
    pub loadOp: i32,
    pub storeOp: i32,
    pub stencilLoadOp: i32,
    pub stencilStoreOp: i32,
    pub initialLayout: i32,
    pub finalLayout: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkAttachmentReference {
    pub attachment: u32,
    pub layout: i32,
}
#[repr(C)]
pub struct VkSubpassDescription {
    pub flags: u32,
    pub pipelineBindPoint: i32,
    pub inputAttachmentCount: u32,
    pub pInputAttachments: *const VkAttachmentReference,
    pub colorAttachmentCount: u32,
    pub pColorAttachments: *const VkAttachmentReference,
    pub pResolveAttachments: *const VkAttachmentReference,
    pub pDepthStencilAttachment: *const VkAttachmentReference,
    pub preserveAttachmentCount: u32,
    pub pPreserveAttachments: *const u32,
}
#[repr(C)]
pub struct VkRenderPassCreateInfo {
    pub sType: VkStructureType,
    pub pNext: *const c_void,
    pub flags: u32,
    pub attachmentCount: u32,
    pub pAttachments: *const VkAttachmentDescription,
    pub subpassCount: u32,
    pub pSubpasses: *const VkSubpassDescription,
    pub dependencyCount: u32,
    pub pDependencies: *const c_void,
}
pub type PFN_vkCreateRenderPass = unsafe extern "C" fn(
    VkDevice,
    *const VkRenderPassCreateInfo,
    *const VkAllocationCallbacks,
    *mut u64,
) -> VkResult;

// ------------------------------- sampler YCbCr conversion
// VK_KHR_sampler_ycbcr_conversion (core 1.1). vkCreateSamplerYcbcrConversion is a
// device-level command whose handle the shim wraps; the create-info is passed to
// the driver verbatim, so it is typed opaquely here.
pub type VkSamplerYcbcrConversion = u64;
pub type PFN_vkCreateSamplerYcbcrConversion = unsafe extern "C" fn(
    VkDevice,
    *const c_void,
    *const VkAllocationCallbacks,
    *mut VkSamplerYcbcrConversion,
) -> VkResult;
