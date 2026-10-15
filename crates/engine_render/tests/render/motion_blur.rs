//! Motion blur (ADR 0096), under the validation layer with synchronization validation:
//!
//! - a still image comes through bit for bit;
//! - a bright square moving right streaks along its motion, as far as the shutter's share
//!   of it, and no higher than it. Its energy grows by up to a fifth: the filter keeps a
//!   moving object's core nearly opaque (McGuire's plausibility, not a physical
//!   integration);
//! - a still object in front of a moving background stays sharp;
//! - the camera's own motion (every pixel moving) blurs the whole image along it.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::half;
use engine_render::graph::{
    Access, BufferState, FrameGraph, ImportedBuffer, LoadOp, RenderGraph, StoreOp, Texture,
    TextureDesc,
};
use engine_render::motion_blur::{MotionBlur, MotionBlurCamera, MotionBlurSettings};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const WIDTH: u32 = 256;
const HEIGHT: u32 = 144;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const MOTION: vk::Format = vk::Format::R16G16_SFLOAT;

const CAMERA: MotionBlurCamera = MotionBlurCamera {
    near: 0.1,
    far: 100.0,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render motion blur test"),
            validation: true,
            debug_utils: true,
            surface: false,
            headless_surface: false,
            sync_validation: true,
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
            println!("skipping GPU test (set ENGINE_REQUIRE_GPU=1 to make this an error): {e}");
            None
        }
        Err(e) => panic!("GPU required but unavailable: {e}"),
    }
}

/// The depth buffer value of a distance (Vulkan's 0..1 perspective depth).
fn depth_of(distance: f32) -> f32 {
    CAMERA.far / (CAMERA.far - CAMERA.near) * (1.0 - CAMERA.near / distance)
}

/// A rectangle (x, y, width, height) of a value.
#[derive(Clone, Copy)]
struct Rect {
    rect: [u32; 4],
    value: [f32; 4],
}

/// A scene: background color, depth and motion (pixels per frame), then rectangles of
/// each.
#[derive(Clone)]
struct Scene {
    color: [f32; 3],
    distance: f32,
    motion: [f32; 2],
    colors: Vec<Rect>,
    distances: Vec<Rect>,
    motions: Vec<Rect>,
}

fn clear_rects(
    cmd: vk::CommandBuffer,
    device: &engine_rhi::ash::Device,
    rects: &[Rect],
    depth: bool,
) {
    for r in rects {
        let [x, y, w, h] = r.rect;
        let attachment = if depth {
            vk::ClearAttachment {
                aspect_mask: vk::ImageAspectFlags::DEPTH,
                color_attachment: 0,
                clear_value: vk::ClearValue {
                    depth_stencil: vk::ClearDepthStencilValue {
                        depth: r.value[0],
                        stencil: 0,
                    },
                },
            }
        } else {
            vk::ClearAttachment {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                color_attachment: 0,
                clear_value: vk::ClearValue {
                    color: vk::ClearColorValue { float32: r.value },
                },
            }
        };
        // SAFETY: recording inside the pass's rendering scope; the rectangle is inside the
        // target.
        unsafe {
            device.cmd_clear_attachments(
                cmd,
                &[attachment],
                &[vk::ClearRect {
                    rect: vk::Rect2D {
                        offset: vk::Offset2D {
                            x: x as i32,
                            y: y as i32,
                        },
                        extent: vk::Extent2D {
                            width: w,
                            height: h,
                        },
                    },
                    base_array_layer: 0,
                    layer_count: 1,
                }],
            );
        }
    }
}

/// Pixels per frame as texture coordinates.
fn uv(motion: [f32; 2]) -> [f32; 4] {
    [
        motion[0] / WIDTH as f32,
        motion[1] / HEIGHT as f32,
        0.0,
        0.0,
    ]
}

/// Draws `scene` into new graph textures: (color, motion, depth).
fn draw<'g>(fg: &mut FrameGraph<'g>, scene: &Scene) -> (Texture, Texture, Texture) {
    let color = fg.create_texture("scene", TextureDesc::new_2d(HDR, WIDTH, HEIGHT));
    let motion = fg.create_texture("motion", TextureDesc::new_2d(MOTION, WIDTH, HEIGHT));
    let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, WIDTH, HEIGHT));
    let [r, g, b] = scene.color;
    let colors = scene.colors.clone();
    fg.add_pass("draw color")
        .color(color, LoadOp::Clear([r, g, b, 1.0]), StoreOp::Store)
        .execute(move |ctx| clear_rects(ctx.cmd, ctx.device, &colors, false));
    let motions: Vec<Rect> = scene
        .motions
        .iter()
        .map(|r| Rect {
            rect: r.rect,
            value: uv([r.value[0], r.value[1]]),
        })
        .collect();
    fg.add_pass("draw motion")
        .color(motion, LoadOp::Clear(uv(scene.motion)), StoreOp::Store)
        .execute(move |ctx| clear_rects(ctx.cmd, ctx.device, &motions, false));
    let distances: Vec<Rect> = scene
        .distances
        .iter()
        .map(|r| Rect {
            rect: r.rect,
            value: [depth_of(r.value[0]), 0.0, 0.0, 0.0],
        })
        .collect();
    fg.add_pass("draw depth")
        .depth(
            depth,
            LoadOp::Clear([depth_of(scene.distance), 0.0, 0.0, 0.0]),
            StoreOp::Store,
        )
        .execute(move |ctx| clear_rects(ctx.cmd, ctx.device, &distances, true));
    (color, motion, depth)
}

fn rect(rect: [u32; 4], value: [f32; 3]) -> Rect {
    Rect {
        rect,
        value: [value[0], value[1], value[2], 1.0],
    }
}

fn moving(rect: [u32; 4], motion: [f32; 2]) -> Rect {
    Rect {
        rect,
        value: [motion[0], motion[1], 0.0, 0.0],
    }
}

fn at(distance: f32, rect: [u32; 4]) -> Rect {
    Rect {
        rect,
        value: [distance, 0.0, 0.0, 0.0],
    }
}

#[test]
fn motion_blur_streaks_what_moves_and_keeps_still_things_sharp() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let blur = MotionBlur::new(&device, &bindless).unwrap();
    let bytes = u64::from(WIDTH * HEIGHT * 8);
    let (readback, readback_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default()
                .size(bytes)
                .usage(vk::BufferUsageFlags::TRANSFER_DST),
            MemoryLocation::Readback,
        )
        .unwrap();
    let d = device.raw();
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

    // Runs one frame: the scene drawn, motion blurred with `settings` (else the scene
    // itself); returns the pixels read back.
    let mut frame = |scene: &Scene, settings: Option<MotionBlurSettings>| -> Vec<[f32; 3]> {
        // SAFETY: one-shot recording and submission, then wait.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .unwrap();
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .unwrap();
        }
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            let (color, motion, depth) = draw(&mut fg, scene);
            let result = match settings {
                Some(settings) => {
                    blur.add_passes(&mut fg, &bindless, color, motion, depth, CAMERA, &settings)
                }
                None => color,
            };
            let out = fg.import_buffer(
                "readback",
                ImportedBuffer {
                    buffer: readback,
                    size: bytes,
                    address: 0,
                    initial: BufferState::idle(),
                    final_state: Some(BufferState::host_read()),
                },
            );
            fg.add_pass("read back")
                .read(result, Access::TransferRead)
                .write(out, Access::TransferWrite)
                .execute(move |ctx| {
                    let region = vk::BufferImageCopy {
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        image_extent: vk::Extent3D {
                            width: WIDTH,
                            height: HEIGHT,
                            depth: 1,
                        },
                        ..Default::default()
                    };
                    // SAFETY: recording; layouts arranged by the graph.
                    unsafe {
                        ctx.device.cmd_copy_image_to_buffer(
                            ctx.cmd,
                            ctx.image(result),
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            ctx.buffer(out),
                            &[region],
                        );
                    }
                });
            fg.execute_with_bindless(&mut allocator, Some(&bindless))
                .unwrap();
        }
        // SAFETY: as above.
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
        let ptr = readback_memory.mapped_ptr().unwrap();
        // SAFETY: the GPU is idle; the mapping covers the image.
        let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), bytes as usize) };
        raw.chunks_exact(8)
            .map(|p| {
                let h = |k: usize| half::to_f32(u16::from_le_bytes([p[2 * k], p[2 * k + 1]]));
                [h(0), h(1), h(2)]
            })
            .collect()
    };
    let pixel = |image: &[[f32; 3]], x: u32, y: u32| image[(y * WIDTH + x) as usize];
    // A full shutter: the streak is as long as the frame's motion, half each way.
    let settings = MotionBlurSettings {
        shutter: 1.0,
        samples: 35,
        max_radius: 0.25,
    };

    // 1. Nothing moves: the image comes through bit for bit.
    let pattern = Scene {
        color: [0.2, 0.3, 0.4],
        distance: 5.0,
        motion: [0.0, 0.0],
        colors: vec![
            rect([10, 10, 40, 30], [5.0, 0.5, 0.1]),
            rect([100, 50, 3, 70], [0.0, 2.0, 0.0]),
        ],
        distances: vec![],
        motions: vec![],
    };
    let sharp = frame(&pattern, None);
    let kept = frame(&pattern, Some(settings));
    let differing = sharp.iter().zip(&kept).filter(|(a, b)| a != b).count();
    println!("still: {differing} pixels differ");
    assert_eq!(differing, 0);

    // 2. A bright square moving 24 pixels a frame to the right: a streak 12 pixels each way
    // past it, as tall as it.
    let (sx, sy, side) = (112u32, 64u32, 16u32);
    let square = Scene {
        color: [0.0; 3],
        distance: 5.0,
        motion: [0.0, 0.0],
        colors: vec![rect([sx, sy, side, side], [10.0; 3])],
        distances: vec![],
        motions: vec![moving([sx, sy, side, side], [24.0, 0.0])],
    };
    let out = frame(&square, Some(settings));
    let energy: f32 = out.iter().map(|p| p[0]).sum();
    let expected = 10.0 * (side * side) as f32;
    let row = sy + side / 2;
    let profile: Vec<f32> = (sx - 20..sx + side + 20)
        .map(|x| pixel(&out, x, row)[0])
        .collect();
    let above = (0..sy - 2)
        .flat_map(|y| (0..WIDTH).map(move |x| (x, y)))
        .map(|(x, y)| pixel(&out, x, y)[0])
        .fold(0.0f32, f32::max);
    let left_of = pixel(&out, sx - 6, row)[0];
    let right_of = pixel(&out, sx + side + 6, row)[0];
    let beyond = pixel(&out, sx + side + 16, row)[0].max(pixel(&out, sx - 16, row)[0]);
    println!(
        "moving square: energy {energy:.1} (expected {expected}), 6 px left of it \
         {left_of:.2}, 6 px right {right_of:.2}, 16 px past either side {beyond:.3}, above it \
         at most {above:.3}; its row: {profile:?}"
    );
    assert!(energy > expected && energy < 1.25 * expected, "{energy}");
    assert!(left_of > 1.0 && right_of > 1.0, "{left_of} {right_of}");
    assert!(beyond < 0.05, "{beyond}");
    assert!(above < 0.01, "{above}");

    // 3. A still object in front of a background moving 30 pixels a frame: it stays sharp.
    let still_front = Scene {
        color: [0.0, 0.0, 1.0],
        distance: 20.0,
        motion: [30.0, 0.0],
        colors: {
            let mut c: Vec<Rect> = (0..16)
                .map(|k| rect([16 * k, 0, 8, HEIGHT], [0.0, 3.0, 0.0]))
                .collect();
            c.push(rect([96, 40, 64, 64], [4.0, 0.0, 0.0]));
            c
        },
        distances: vec![at(2.0, [96, 40, 64, 64])],
        motions: vec![moving([96, 40, 64, 64], [0.0, 0.0])],
    };
    let reference = frame(&still_front, None);
    let out = frame(&still_front, Some(settings));
    let mut front_error = 0.0f32;
    for y in 42..102 {
        for x in 98..158 {
            let (a, b) = (pixel(&out, x, y), pixel(&reference, x, y));
            front_error = front_error.max((0..3).map(|k| (a[k] - b[k]).abs()).fold(0.0, f32::max));
        }
    }
    // The background's stripes blurred to their average far from the object.
    let background = pixel(&out, 30, 20);
    println!(
        "still in front of a moving background: its worst error {front_error}, the stripes \
         far from it {background:?}"
    );
    assert!(front_error < 0.05, "{front_error}");
    assert!((background[1] - 1.5).abs() < 0.3, "{background:?}");

    // 4. The camera's own motion, upwards: everything blurs along it, nothing across it.
    let turning = Scene {
        color: [0.0; 3],
        distance: 10.0,
        motion: [0.0, -20.0],
        colors: (0..9)
            .map(|k| rect([0, 16 * k, WIDTH, 8], [2.0, 2.0, 2.0]))
            .collect(),
        distances: vec![],
        motions: vec![],
    };
    let out = frame(&turning, Some(settings));
    let column: Vec<f32> = (40..72).map(|y| pixel(&out, 128, y)[0]).collect();
    let spread = column.iter().fold(0.0f32, |m, v| m.max(*v))
        - column.iter().fold(f32::MAX, |m, v| m.min(*v));
    let across = (0..WIDTH)
        .map(|x| (pixel(&out, x, 60)[0] - pixel(&out, 128, 60)[0]).abs())
        .fold(0.0f32, f32::max);
    println!(
        "camera motion: horizontal stripes 16 px apart under a 20 px streak, a column's \
         range {spread:.3} (from 2 sharp), across a row at most {across:.3}"
    );
    assert!(spread < 0.6, "{spread}");
    assert!(across < 0.15, "{across}");

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        allocator.destroy_buffer(readback, readback_memory);
        d.destroy_command_pool(pool, None);
    }
    drop(blur);
    drop(bindless);
    drop(allocator);
    drop(device);
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): errors={} warnings={}",
        report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
