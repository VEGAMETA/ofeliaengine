//! Reports whether DLSS is available on this machine and why not: where the Streamline
//! DLLs were found, their signature and versions, and each GPU's support.
//!
//! `cargo run -p engine_dlss --example dlss_probe` (set `ENGINE_STREAMLINE_DIR` to the
//! folder with `sl.interposer.dll`, `sl.common.dll`, `sl.dlss.dll` and `nvngx_dlss.dll`,
//! or put them in `plugins/nvidia/` next to the executable).

#![allow(clippy::print_stdout)]

use engine_dlss::{Dlss, Streamline, StreamlineDesc};
use engine_rhi::{AdapterPreference, Device, Instance, InstanceDesc};
use engine_upscale::Quality;

fn main() {
    let streamline = match Streamline::load(&StreamlineDesc::default()) {
        Ok(s) => s,
        Err(e) => {
            println!("DLSS unavailable: {e}");
            return;
        }
    };
    println!("Streamline loaded from {}", streamline.dir().display());
    println!(
        "instance extensions: {:?}\ndevice extensions: {:?}",
        streamline.instance_extensions(),
        streamline.device_extensions()
    );
    let instance = match Instance::with_extensions(
        &InstanceDesc {
            validation: false,
            surface: false,
            ..InstanceDesc::default()
        },
        &streamline.instance_extensions(),
    ) {
        Ok(i) => i,
        Err(e) => {
            println!("no Vulkan instance: {e}");
            return;
        }
    };
    let adapters = instance.adapters().unwrap_or_default();
    for adapter in &adapters {
        match streamline.check_adapter(&instance, adapter) {
            Ok(()) => println!("{}: DLSS supported", adapter.name()),
            Err(e) => println!("{}: DLSS unsupported: {e}", adapter.name()),
        }
    }
    let preference = AdapterPreference::from_env().unwrap_or(AdapterPreference::HighPerformance);
    let Ok(adapter) = instance.select_adapter(&preference) else {
        println!("no suitable adapter");
        return;
    };
    if let Err(e) = streamline.check_adapter(&instance, &adapter) {
        println!("selected {}: DLSS unsupported: {e}", adapter.name());
        return;
    }
    let device = match Device::with_extensions(&instance, &adapter, &streamline.device_extensions())
    {
        Ok(d) => d,
        Err(e) => {
            println!("no device: {e}");
            return;
        }
    };
    if let Err(e) = streamline.attach(&device) {
        println!("attach failed: {e}");
        return;
    }
    let versions = streamline.versions();
    println!(
        "selected {}: DLSS ready (sl.dlss {:?}, nvngx_dlss {:?})",
        adapter.name(),
        versions.streamline,
        versions.ngx
    );
    if let Ok(dlss) = Dlss::new(&streamline, 2) {
        for output in [(1920, 1080), (2560, 1440), (3840, 2160)] {
            for quality in Quality::ALL {
                match dlss.render_sizes(quality, output) {
                    Ok(sizes) => println!(
                        "  {output:?} {quality:?}: render {:?} (accepts {:?} to {:?})",
                        sizes.optimal, sizes.min, sizes.max
                    ),
                    Err(e) => println!("  {output:?} {quality:?}: {e}"),
                }
            }
        }
    }
    println!("log: {:?}", Streamline::log());
    drop(streamline);
    drop(device);
}
