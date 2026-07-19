//! One-shot probe (perf audit, Fix D): does this GPU/driver support importing an arbitrary host
//! pointer as Vulkan memory (`VK_EXT_external_memory_host`) — the mechanism to DMA a rendered frame
//! straight into the guest scanout (memfd-backed guest RAM), skipping the CPU readback copy? Also
//! reports `minImportedHostPointerAlignment` (the guest scanout base + size must satisfy it) and the
//! dma-buf/fd export extensions. Prints, exits — validates nothing on the GPU by itself.

use ash::vk;
use std::ffi::CStr;

fn arr_to_cstr(a: &[i8]) -> &CStr {
    // SAFETY: Vulkan guarantees a NUL within the fixed-size name array.
    unsafe { CStr::from_ptr(a.as_ptr()) }
}
fn name(a: &[i8]) -> String {
    arr_to_cstr(a).to_string_lossy().into_owned()
}

fn main() {
    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => e,
        Err(e) => {
            eprintln!("probe: cannot load Vulkan loader: {e}");
            std::process::exit(1);
        }
    };
    let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 3, 0));
    let ci = vk::InstanceCreateInfo::default().application_info(&app);
    let instance = unsafe { entry.create_instance(&ci, None).expect("create_instance") };

    let physicals = unsafe { instance.enumerate_physical_devices().expect("enum devices") };
    for &pd in &physicals {
        let props = unsafe { instance.get_physical_device_properties(pd) };
        let mut driver = vk::PhysicalDeviceDriverProperties::default();
        let mut host = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default()
            .push_next(&mut driver)
            .push_next(&mut host);
        unsafe { instance.get_physical_device_properties2(pd, &mut props2) };

        let dev_exts = unsafe {
            instance
                .enumerate_device_extension_properties(pd)
                .unwrap_or_default()
        };
        let has = |want: &str| {
            dev_exts
                .iter()
                .any(|e| arr_to_cstr(&e.extension_name).to_string_lossy() == want)
        };
        let ext_host = has("VK_EXT_external_memory_host");
        let ext_fd = has("VK_KHR_external_memory_fd");
        let ext_dmabuf = has("VK_EXT_external_memory_dma_buf");

        println!(
            "device: {} | driver: {} ({:?}) | type {:?}",
            name(&props.device_name),
            name(&driver.driver_name),
            driver.driver_id,
            props.device_type,
        );
        println!(
            "  VK_EXT_external_memory_host = {ext_host}   (minImportedHostPointerAlignment = {} bytes)",
            host.min_imported_host_pointer_alignment
        );
        println!("  VK_KHR_external_memory_fd  = {ext_fd}");
        println!("  VK_EXT_external_memory_dma_buf = {ext_dmabuf}");

        // Which memory types are HOST_VISIBLE (candidates the imported guest RAM must be compatible with).
        let mp = unsafe { instance.get_physical_device_memory_properties(pd) };
        let host_visible: Vec<usize> = (0..mp.memory_type_count as usize)
            .filter(|&i| {
                mp.memory_types[i]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
            })
            .collect();
        println!(
            "  HOST_VISIBLE memory type indices: {host_visible:?} (import must land in one of these)"
        );
        println!(
            "  => Fix D verdict: {}",
            if ext_host {
                "GO — import guest scanout host pointer, cmd_copy_image_to_buffer straight into guest RAM"
            } else {
                "NO external_memory_host — fall back to dma-buf/fd import or keep the CPU copy"
            }
        );
    }
    unsafe { instance.destroy_instance(None) };
}
