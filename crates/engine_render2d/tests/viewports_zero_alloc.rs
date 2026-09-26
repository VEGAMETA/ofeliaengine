//! CLAUDE.md §5: zero heap allocations per frame in steady state for viewports: due
//! viewports (a render tick, a child shown in its parent, split screen, pixel-perfect
//! scaling) rendered through graph scopes, their contents drawn by the 2D renderer, and
//! the composite into the output.
//!
//! A counting global allocator observes the process after a warm-up (texture creation
//! happens on the first frames; driver allocations happen in C and are not counted). One
//! test per file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Vec2, Vec4};
use engine_render::graph::{Access, LoadOp, RenderGraph, TextureDesc};
use engine_render::post::Tonemap;
use engine_render::upload::UploadRing;
use engine_render::viewport::{
    Placement, Scaling, UpdateMode, ViewportDesc, ViewportSize, Viewports,
};
use engine_render2d::{Canvas, Quad, QuadMode, Renderer2d, View2d};
use engine_rhi::ash::vk;
use engine_rhi::{AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc};
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
fn steady_state_frames_do_not_allocate() {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let setup = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("viewports zero-alloc test"),
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
    let tonemap = Tonemap::new(&device, &bindless, format).unwrap();
    let mut ring = UploadRing::new(&device, &mut allocator, 2, 1 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 2);
    let mut viewports = Viewports::new(&device, 2);
    let world = viewports.create(ViewportDesc {
        name: String::from("world"),
        format,
        placement: Some(Placement {
            rect: [0.0, 0.0, 0.5, 1.0],
            order: 0,
        }),
        ..ViewportDesc::default()
    });
    let minimap = viewports.create(ViewportDesc {
        name: String::from("minimap"),
        format,
        size: ViewportSize::Fixed {
            width: 64,
            height: 64,
        },
        parent: Some(world),
        placement: None,
        update: UpdateMode::Every { n: 2, offset: 0 },
        ..ViewportDesc::default()
    });
    viewports.create(ViewportDesc {
        name: String::from("pixel art"),
        format,
        size: ViewportSize::Fixed {
            width: 32,
            height: 18,
        },
        placement: Some(Placement {
            rect: [0.5, 0.0, 0.5, 1.0],
            order: 1,
        }),
        scaling: Scaling::PixelPerfect,
        ..ViewportDesc::default()
    });
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
    let mut canvas = Canvas::new();

    let mut frame = |frame_number: u64| {
        canvas.clear();
        if let Some(index) = viewports.texture_index(minimap) {
            canvas.draw(&Quad::sprite(index, Vec2::splat(40.0), Vec2::splat(64.0)));
        }
        for i in 0..100 {
            canvas.draw(&Quad::circle(Vec2::new(i as f32, 20.0), 3.0).color(Vec4::ONE));
        }
        canvas.prepare();
        // SAFETY: nothing recorded by this test is ever submitted.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .unwrap();
        }
        let slot = (frame_number % 2) as usize;
        ring.begin_slot(slot);
        {
            // SAFETY: nothing submitted, so no GPU work uses any slot.
            let mut fg = unsafe { graph.begin_slot(slot, cmd) };
            let (canvas_ref, renderer_ref, bindless_ref) = (&canvas, &renderer, &bindless);
            let ring_ref = &mut ring;
            viewports
                .render(
                    &mut fg,
                    &device,
                    &mut allocator,
                    &bindless,
                    (320, 180),
                    frame_number,
                    |fg, vp| {
                        let clear = match vp.load {
                            LoadOp::Clear(c) => Some(c),
                            _ => None,
                        };
                        renderer_ref
                            .add_pass(
                                fg,
                                ring_ref,
                                bindless_ref,
                                canvas_ref,
                                &View2d::pixels(vp.width, vp.height),
                                vp.color,
                                clear,
                            )
                            .unwrap();
                    },
                )
                .unwrap();
            let output = fg.create_texture("output", TextureDesc::new_2d(format, 320, 180));
            viewports.composite(
                &mut fg,
                &tonemap,
                &bindless,
                output,
                (320, 180),
                frame_number,
                [0.0; 4],
            );
            fg.add_pass("consume")
                .read(output, Access::TransferRead)
                .side_effects()
                .execute(|_| {});
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
        }
        // SAFETY: recording command buffer.
        unsafe { d.end_command_buffer(cmd).unwrap() };
    };

    for frame_number in 1..=20 {
        frame(frame_number);
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for frame_number in 21..=120 {
        frame(frame_number);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!("100 frames of 3 viewports: {allocations} heap allocations");
    assert_eq!(allocations, 0);

    device.wait_idle().unwrap();
    // SAFETY: nothing is in use.
    unsafe {
        viewports.destroy_all(&mut allocator, &bindless);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        renderer.destroy(&mut allocator);
        d.destroy_command_pool(pool, None);
    }
}
