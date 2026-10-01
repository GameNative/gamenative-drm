//! libvulkan_freedreno.so test double — same code as mock_vkdriver, built
//! under the payload fallback's SONAME so the shim's
//! `dlopen("libvulkan_freedreno.so")` finds it via LD_LIBRARY_PATH.

include!("../../mock-vkdriver/src/lib.rs");
