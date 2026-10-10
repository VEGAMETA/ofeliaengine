//! Vulkan's layers outlive the thread that first loaded them (`modules.rs`).
//!
//! The validation layer of the Vulkan SDK 1.4.350 links mimalloc 3.3.2, which takes the
//! thread that loads the layer for the process's main thread, identified by the address of
//! its thread environment block (TEB). When that thread exits, mimalloc tears the main
//! thread's heap down (its `tld` becomes null), and when Windows places a new thread's TEB
//! at the same address, that thread's first allocation in the layer dereferences the
//! torn-down heap: an access violation in `_mi_subproc`. The golden tests hit it about
//! once in eight runs: every test runs on a new thread.
//!
//! This test does it on purpose: an instance with validation is created on a short-lived
//! thread, which exits; then new threads are started until one gets its TEB address, and
//! that thread calls into the layer. The engine creates instances on a thread that lives as
//! long as the process, so the layer never sees its loading thread exit.
//!
//! One test per file: it depends on the layer being loaded for the first time.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_rhi::{AdapterPreference, Device, Instance, InstanceDesc};

#[cfg(windows)]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
}

/// What mimalloc identifies a thread by: the address of its TEB (`NtCurrentTeb`).
#[cfg(all(windows, target_arch = "x86_64"))]
fn thread_id() -> usize {
    let teb: usize;
    // SAFETY: on x64 Windows `gs:[0x30]` holds the TEB's own address.
    unsafe {
        core::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags))
    };
    teb
}

#[cfg(windows)]
fn loaded(name: &str) -> bool {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated name; the handle is only compared with null.
    !unsafe { GetModuleHandleW(wide.as_ptr()) }.is_null()
}

fn desc() -> InstanceDesc {
    InstanceDesc {
        app_name: String::from("engine_rhi instance lifetime"),
        validation: true,
        debug_utils: true,
        surface: false,
        headless_surface: false,
        sync_validation: false,
    }
}

/// Calls into every layer of `instance` in a way that allocates in each (the validation
/// layer copies the name into a `std::string`), and creates and drops a device.
fn use_layers(instance: &Instance) {
    // SAFETY: a valid instance and a NUL-terminated name.
    let f = unsafe {
        instance
            .entry()
            .get_instance_proc_addr(instance.raw().handle(), c"vkCreateDevice".as_ptr())
    };
    assert!(f.is_some());
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let adapter = instance.select_adapter(&preference).unwrap();
    drop(Device::new(instance, &adapter).unwrap());
}

#[test]
fn layers_outlive_the_thread_that_loaded_them() {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    // The first instance of the process, on a thread that exits right after.
    let (instance, first_thread) = std::thread::spawn(|| (Instance::new(&desc()), thread_id()))
        .join()
        .unwrap();
    let instance = match instance {
        Ok(instance) => instance,
        Err(e) if !required => {
            println!("skipping (no Vulkan): {e}");
            return;
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    };
    let validation = instance.validation_report().enabled;
    println!("validation: {validation}; first instance created on thread {first_thread:#x}");

    // New threads until Windows reuses the first thread's TEB (within a few thousand here).
    let mut reused = false;
    for attempt in 0..4096 {
        let instance = instance.clone();
        let hit = std::thread::spawn(move || {
            if thread_id() != first_thread {
                return false;
            }
            use_layers(&instance);
            true
        })
        .join()
        .unwrap();
        if hit {
            println!("TEB {first_thread:#x} reused after {attempt} threads; the layers held");
            reused = true;
            break;
        }
    }
    if !reused {
        println!("TEB {first_thread:#x} was not reused; nothing to check");
    }
    // A thread that was never seen, and the instance's own thread, still work.
    std::thread::spawn({
        let instance = instance.clone();
        move || use_layers(&instance)
    })
    .join()
    .unwrap();
    use_layers(&instance);

    // The loader and the layers stay loaded after the last instance (`modules.rs`).
    drop(instance);
    #[cfg(windows)]
    {
        assert!(loaded("vulkan-1.dll"), "the loader was unloaded");
        if validation {
            assert!(
                loaded("VkLayer_khronos_validation.dll"),
                "the validation layer was unloaded"
            );
        }
    }
}
