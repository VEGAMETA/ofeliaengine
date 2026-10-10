//! The upload ring (core review 2 §3.6): a frame that uses more than three quarters of a
//! slot, or asks for more than it has, makes the ring grow for the next frames; each slot
//! takes its larger buffer when it begins again, so data already handed out stays where
//! it is. Under the validation layer.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_render::upload::UploadRing;
use engine_rhi::{AdapterPreference, Allocator, Device, Instance, InstanceDesc};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render upload ring test"),
            validation: true,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: false,
        })?;
        let preference =
            AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
        let adapter = instance.select_adapter(&preference)?;
        let device = Device::new(&instance, &adapter)?;
        Ok((instance, device))
    };
    match attempt() {
        Ok(pair) => Some(pair),
        Err(e) if !required => {
            println!("skipping: {e}");
            None
        }
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn the_ring_grows_ahead_and_slots_take_their_new_buffers_when_they_begin() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    let mut allocator = Allocator::new(&device);
    let mut ring = UploadRing::new(&device, &mut allocator, 2, 4096).unwrap();
    // A quiet frame: nothing to grow, nothing allocated.
    ring.begin_slot(0);
    assert!(ring.push(&[0u8; 1024]).is_some());
    assert!(!ring.grow(&device, &mut allocator).unwrap());
    // Slot 1 uses more than three quarters: the ring grows (twice the demand, at least a
    // MiB), but slot 1 keeps its buffer until it begins again.
    ring.begin_slot(1);
    let first = ring.push(&[1u8; 3200]).unwrap();
    assert!(ring.grow(&device, &mut allocator).unwrap());
    assert_eq!(ring.stats().capacity, 1 << 20);
    assert_eq!(ring.available(), 4096 - 3200);
    let second = ring.push(&[2u8; 512]).unwrap();
    assert_eq!(second, first + 3200, "the frame goes on in the same buffer");
    // Each slot takes the larger buffer as it begins.
    ring.begin_slot(0);
    assert_eq!(ring.available(), 1 << 20);
    ring.begin_slot(1);
    assert_eq!(ring.available(), 1 << 20);
    // A frame that asks for more than a slot holds is refused once and recorded; the
    // ring grows for the next frames.
    let large = vec![3u8; 3 << 20];
    assert!(ring.push(&large).is_none());
    assert_eq!(ring.stats().failed, 1);
    assert!(ring.grow(&device, &mut allocator).unwrap());
    assert!(ring.stats().capacity >= 6 << 20);
    ring.begin_slot(0);
    assert!(ring.push(&large).is_some());
    assert_eq!(ring.stats().grown, 2);
    assert!(!ring.grow(&device, &mut allocator).unwrap());
    // SAFETY: nothing was submitted.
    unsafe { ring.destroy(&mut allocator) };
    drop(allocator);
    let report = instance.validation_report();
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
