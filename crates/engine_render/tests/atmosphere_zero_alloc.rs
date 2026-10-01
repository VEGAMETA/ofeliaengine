//! CLAUDE.md §5: zero heap allocations per frame in steady state, for the atmosphere and
//! height fog (ADR 0069): a sun that moves every frame (the sky-view table rebuilt each
//! frame, the image-based light every few), the aerial perspective, the full-screen pass
//! and transparent surfaces under fog, declared and recorded.
//!
//! A counting global allocator observes the process while frames are declared and
//! recorded after a warm-up (driver allocations happen in C and are not counted). One
//! test per file, so nothing else allocates concurrently. Every frame is submitted and
//! waited for: with async compute (ADR 0065) the graph submits its earlier segments itself
//! during `execute`.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Vec3, look_at, vulkan_perspective};
use engine_render::atmosphere::{Atmosphere, HeightFog};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings, Temporal,
};
use engine_render::graph::{RenderGraph, TextureDesc};
use engine_render::material::{AlphaMode, MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance as VkInstance, InstanceDesc,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn steady_state_atmosphere_frames_do_not_allocate() {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let setup = || -> Result<(VkInstance, Device), engine_rhi::RhiError> {
        let instance = VkInstance::new(&InstanceDesc {
            app_name: String::from("engine_render atmosphere zero-alloc test"),
            validation: false,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: false,
        })?;
        let adapter = instance.select_adapter(&AdapterPreference::HighPerformance)?;
        let device = Device::new(&instance, &adapter)?;
        Ok((instance, device))
    };
    let (_instance, device) = match setup() {
        Ok(pair) => pair,
        Err(e) if !required => {
            println!("skipping: {e}");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let (color_format, depth_format) = (vk::Format::R16G16B16A16_SFLOAT, vk::Format::D32_SFLOAT);
    let renderer = ForwardRenderer::new(
        &device,
        &bindless,
        color_format,
        depth_format,
        ShadowSettings::default(),
    )
    .unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 2).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 2);
    let mut materials = MaterialTable::new(2);
    let opaque = materials.add(MaterialDesc::solid([0.6, 0.5, 0.4], 0.0, 0.5));
    let glass = materials.add(MaterialDesc {
        base_color: [0.4, 0.8, 1.0, 0.4],
        alpha: AlphaMode::Blend,
        ..MaterialDesc::default()
    });
    let draws: Vec<DrawItem> = (0..64)
        .map(|i| DrawItem {
            mesh,
            material: if i % 8 == 0 { glass } else { opaque },
            world_from_object: Mat4::from_translation(Vec3::new(
                (i % 8) as f32 * 2.0 - 8.0,
                0.0,
                -((i / 8) as f32) * 2.0,
            )),
        })
        .collect();
    let mut ring = UploadRing::new(&device, &mut allocator, 2, 4 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 2);
    let temporal = Temporal::new(&device, &mut allocator, 64).unwrap();
    let mut environment =
        Environment::new(&device, &mut allocator, &bindless, Sky::default()).unwrap();
    environment
        .set_atmosphere(
            &device,
            &mut allocator,
            &bindless,
            Some(Atmosphere::earth()),
        )
        .unwrap();
    environment.set_fog(Some(HeightFog::default()));
    // SAFETY: plain object creation.
    let (pool, cmd) = unsafe {
        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(device.graphics.family),
                None,
            )
            .unwrap();
        let cmd = d
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(1),
            )
            .unwrap()[0];
        (pool, cmd)
    };
    let eye = Vec3::new(0.0, 4.0, 12.0);
    let camera = Camera {
        clip_from_world: vulkan_perspective(1.0, 16.0 / 9.0, 0.1, 500.0)
            * look_at(eye, Vec3::new(0.0, 0.0, -4.0), Vec3::Y),
        position: eye,
    };
    let mut frame_number = 0u64;
    let mut frame = |i: usize| {
        // The sun climbs 0.05° a frame: a new sky-view table every frame, new image-based
        // light every other.
        let elevation = (10.0 + 0.05 * i as f32).to_radians();
        let light = DirectionalLight {
            direction: -Vec3::new(
                elevation.cos() * 0.6,
                elevation.sin(),
                elevation.cos() * 0.8,
            ),
            color: Vec3::splat(3.0),
            ambient: Vec3::ZERO,
        };
        frame_number += 1;
        let slot = (frame_number % 2) as usize;
        // SAFETY: the previous frame finished (waited below).
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .unwrap();
        }
        ring.begin_slot(slot);
        {
            // SAFETY: every earlier frame was waited for.
            let mut fg = unsafe { graph.begin_slot(slot, cmd) };
            textures
                .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                .unwrap();
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(color_format, 640, 360));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(depth_format, 640, 360));
            let passes = renderer
                .add_passes(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &geo,
                    &mats,
                    &draws,
                    &ForwardView {
                        camera,
                        light,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: Some(&temporal),
                        lights: &[],
                        environment: Some(&environment),
                    },
                )
                .unwrap();
            assert!(passes.atmosphere.is_some());
            std::hint::black_box(passes);
            fg.add_pass("present stand-in")
                .read(color, engine_render::graph::Access::TransferRead)
                .side_effects()
                .execute(|_| {});
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
        }
        // SAFETY: recorded above; submitted after the graph's own segments, then waited
        // for.
        unsafe {
            d.end_command_buffer(cmd).unwrap();
            let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            d.queue_submit2(
                device.graphics.raw,
                &[vk::SubmitInfo2::default().command_buffer_infos(&infos)],
                vk::Fence::null(),
            )
            .unwrap();
            device.wait_idle().unwrap();
        }
    };

    for i in 0..30 {
        frame(i);
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for i in 30..130 {
        frame(i);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!("100 frames: {allocations} heap allocations");
    assert_eq!(allocations, 0);

    device.wait_idle().unwrap();
    let mut temporal = temporal;
    // SAFETY: nothing is in use.
    unsafe {
        environment.destroy(&mut allocator, &bindless);
        temporal.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        d.destroy_command_pool(pool, None);
    }
}
