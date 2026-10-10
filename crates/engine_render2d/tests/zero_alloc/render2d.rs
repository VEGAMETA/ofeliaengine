//! CLAUDE.md §5: zero heap allocations per frame in steady state, for recording a canvas
//! (with y-sorting, layers, clips and every blend mode), sorting and batching it, writing
//! the instances into the upload ring and recording the 2D pass through the render graph.
//!
//! A counting global allocator observes the process after a warm-up (driver allocations
//! happen in C and are not counted). One test per file, so nothing else allocates
//! concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Vec2, Vec4};
use engine_render::graph::{RenderGraph, TextureDesc};
use engine_render::upload::UploadRing;
use engine_render2d::{Blend2d, Camera2d, Canvas, ClipRect, Order, Quad, QuadMode, Renderer2d};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};
use std::sync::atomic::Ordering;

use crate::ALLOCATIONS;

#[test]
fn steady_state_frames_do_not_allocate() {
    let _alone = crate::alone();
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let setup = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render2d zero-alloc test"),
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
    let format = vk::Format::R8G8B8A8_UNORM;
    let mut renderer = Renderer2d::new(
        &device,
        &mut allocator,
        &bindless,
        format,
        QuadMode::Indexed,
    )
    .unwrap();
    let mut ring = UploadRing::new(&device, &mut allocator, 2, 4 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 2);
    let mut canvas = Canvas::new();
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

    let mut frame = |frame_index: usize| {
        let slot = frame_index % 2;
        let t = frame_index as f32 * 0.1;
        canvas.clear();
        for i in 0..5000 {
            let x = (i % 100) as f32;
            let y = (i / 100) as f32 + (t + x).sin();
            canvas.draw(
                &Quad::sprite((i % 7) as u32, Vec2::new(x, y), Vec2::ONE)
                    .order(Order::y_sorted(0, y)),
            );
        }
        canvas.set_clip(Some(ClipRect {
            x: 10,
            y: 10,
            width: 100,
            height: 50,
        }));
        for i in 0..500 {
            let p = Vec2::new(i as f32, t);
            canvas.draw(&Quad::circle(p, 2.0).color(Vec4::ONE).order(Order::layer(1)));
            canvas.draw(
                &Quad::line(p, p + Vec2::ONE, 0.5)
                    .blend(Blend2d::Additive)
                    .order(Order::layer(1)),
            );
            canvas.draw(
                &Quad::rounded_rect(p, Vec2::splat(3.0), 1.0)
                    .outline(0.25)
                    .blend(Blend2d::Multiply)
                    .order(Order::layer(-1)),
            );
        }
        canvas.set_clip(None);
        canvas.prepare();

        // SAFETY: nothing recorded by this test is ever submitted.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .unwrap();
        }
        ring.begin_slot(slot);
        {
            // SAFETY: nothing submitted, so no GPU work uses any slot.
            let mut fg = unsafe { graph.begin_slot(slot, cmd) };
            let target = fg.create_texture("target", TextureDesc::new_2d(format, 320, 180));
            let view = Camera2d {
                center: Vec2::new(50.0, 25.0),
                pixels_per_unit: 3.0,
                rotation: t,
            }
            .view(320, 180);
            renderer
                .add_pass(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &canvas,
                    &view,
                    target,
                    Some([0.0; 4]),
                )
                .unwrap();
            fg.add_pass("consume")
                .read(target, engine_render::graph::Access::TransferRead)
                .side_effects()
                .execute(|_| {});
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
        }
        // SAFETY: recording command buffer.
        unsafe { d.end_command_buffer(cmd).unwrap() };
    };

    for i in 0..20 {
        frame(i);
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for i in 20..120 {
        frame(i);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!(
        "100 frames of 6500 quads: {allocations} heap allocations; last canvas {:?}",
        canvas.stats()
    );
    assert_eq!(allocations, 0);
    assert!(!canvas.stats().presorted);

    device.wait_idle().unwrap();
    // SAFETY: nothing is in use.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        renderer.destroy(&mut allocator);
        d.destroy_command_pool(pool, None);
    }
}
