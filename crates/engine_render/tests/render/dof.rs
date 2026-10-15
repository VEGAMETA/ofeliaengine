//! Depth of field (ADR 0095), under the validation layer with synchronization validation:
//!
//! - an image in focus comes through bit for bit;
//! - a bright point far behind the plane in focus spreads to a disk of the thin lens's
//!   radius, keeping its energy;
//! - a sharp object in front of a blurred background stays sharp, and the background does
//!   not take its color;
//! - a blurred object in front of a sharp background spreads over it as far as its
//!   radius, and leaves the background sharp beyond.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::half;
use engine_render::dof::{DepthOfField, DofCamera, DofSettings, Lens};
use engine_render::graph::{
    Access, BufferState, FrameGraph, ImportedBuffer, LoadOp, RenderGraph, StoreOp, Texture,
    TextureDesc,
};
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const WIDTH: u32 = 256;
const HEIGHT: u32 = 144;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;

const CAMERA: DofCamera = DofCamera {
    fov_y: 0.6,
    near: 0.1,
    far: 100.0,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render dof test"),
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

/// A scene: a background color and depth, then rectangles of color and of depth.
struct Scene {
    color: [f32; 3],
    distance: f32,
    colors: Vec<Rect>,
    distances: Vec<Rect>,
}

fn clear_rects(
    ctx_cmd: vk::CommandBuffer,
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
                ctx_cmd,
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

/// Draws `scene` into new graph textures: (color, depth).
fn draw<'g>(fg: &mut FrameGraph<'g>, scene: &Scene) -> (Texture, Texture) {
    let color = fg.create_texture("scene", TextureDesc::new_2d(HDR, WIDTH, HEIGHT));
    let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, WIDTH, HEIGHT));
    let [r, g, b] = scene.color;
    let colors = scene.colors.clone();
    fg.add_pass("draw color")
        .color(color, LoadOp::Clear([r, g, b, 1.0]), StoreOp::Store)
        .execute(move |ctx| clear_rects(ctx.cmd, ctx.device, &colors, false));
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
    (color, depth)
}

fn rect(rect: [u32; 4], value: [f32; 3]) -> Rect {
    Rect {
        rect,
        value: [value[0], value[1], value[2], 1.0],
    }
}

fn at(distance: f32, rect: [u32; 4]) -> Rect {
    Rect {
        rect,
        value: [distance, 0.0, 0.0, 0.0],
    }
}

#[test]
fn depth_of_field_blurs_by_the_thin_lens_and_keeps_focus_sharp() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let dof = DepthOfField::new(&device, &bindless).unwrap();
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

    // Runs one frame: the scene drawn, `blur` true for depth of field with `settings` (else
    // the scene itself); returns the pixels read back.
    let mut frame = |scene: &Scene, settings: Option<DofSettings>| -> Vec<[f32; 3]> {
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
            let (color, depth) = draw(&mut fg, scene);
            let result = match settings {
                Some(settings) => {
                    dof.add_passes(&mut fg, &bindless, color, depth, CAMERA, &settings)
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
    let focus = 2.0;
    let settings = DofSettings {
        focus_distance: focus,
        f_number: 0.7,
        sensor_height: 0.1,
        max_radius: 0.25,
        rings: 6,
    };
    let lens = Lens::new(&settings, CAMERA, HEIGHT);

    // 1. In focus: the image comes through bit for bit.
    let pattern = Scene {
        color: [0.2, 0.3, 0.4],
        distance: focus,
        colors: vec![
            rect([10, 10, 40, 30], [5.0, 0.5, 0.1]),
            rect([100, 50, 3, 70], [0.0, 2.0, 0.0]),
            rect([200, 100, 50, 40], [0.01, 0.02, 30.0]),
        ],
        distances: vec![],
    };
    let sharp = frame(&pattern, None);
    let kept = frame(&pattern, Some(settings));
    let differing = sharp.iter().zip(&kept).filter(|(a, b)| a != b).count();
    println!("in focus: {differing} pixels differ");
    assert_eq!(differing, 0);

    // 2. A bright point far behind the plane in focus: a disk of the lens's radius.
    let far = 30.0;
    let radius = 2.0 * lens.far_radius(lens.q(depth_of(far))); // in pixels of the image
    let (cx, cy) = (WIDTH / 2, HEIGHT / 2);
    let point = Scene {
        color: [0.0; 3],
        distance: far,
        colors: vec![rect([cx - 1, cy - 1, 2, 2], [1000.0; 3])],
        distances: vec![],
    };
    let disk = frame(&point, Some(settings));
    let energy: f32 = disk.iter().map(|p| p[0]).sum();
    let expected = 4.0 * 1000.0;
    let mean = expected / (core::f32::consts::PI * radius * radius);
    let (mut inner_low, mut outer_high) = (f32::MAX, 0.0f32);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let r = ((x as f32 + 0.5 - cx as f32).powi(2) + (y as f32 + 0.5 - cy as f32).powi(2))
                .sqrt();
            let v = pixel(&disk, x, y)[0];
            if r < 0.6 * radius {
                inner_low = inner_low.min(v);
            }
            if r > radius + 3.0 {
                outer_high = outer_high.max(v);
            }
        }
    }
    println!(
        "far point: radius {radius:.2} px, energy {energy:.1} (expected {expected}), inside \
         at least {inner_low:.2}, outside at most {outer_high:.3} (disk mean {mean:.2})"
    );
    if std::env::var("DOF_PROFILE").is_ok() {
        let mut bins = [(0.0f32, 0u32, 0.0f32); 32];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let r = ((x as f32 + 0.5 - cx as f32).powi(2)
                    + (y as f32 + 0.5 - cy as f32).powi(2))
                .sqrt();
                if (r as usize) < bins.len() {
                    let v = pixel(&disk, x, y)[0];
                    let b = &mut bins[r as usize];
                    b.0 += v;
                    b.1 += 1;
                    b.2 = b.2.max(v);
                }
            }
        }
        for (r, (sum, n, max)) in bins.iter().enumerate() {
            println!(
                "  r {r:2}: mean {:.3} max {max:.3}",
                sum / (*n).max(1) as f32
            );
        }
        for y in (cy - 20)..(cy + 20) {
            let row: String = (cx - 24..cx + 24)
                .map(|x| {
                    let v = pixel(&disk, x, y)[0] / mean;
                    match v {
                        v if v < 0.05 => ' ',
                        v if v < 0.3 => '.',
                        v if v < 0.7 => ':',
                        v if v < 1.3 => 'o',
                        _ => '#',
                    }
                })
                .collect();
            println!("  |{row}|");
        }
    }
    assert!(radius > 8.0, "the test needs a wide disk: {radius}");
    assert!((energy - expected).abs() < 0.15 * expected, "{energy}");
    assert!(inner_low > 0.4 * mean, "{inner_low} of {mean}");
    assert!(outer_high < 0.05 * mean, "{outer_high} of {mean}");

    // 3. A sharp object in front of a blurred background: it stays sharp, and the
    // background does not take its color.
    let stripes: Vec<Rect> = (0..16)
        .map(|k| rect([128 + 8 * k, 0, 4, HEIGHT], [0.0, 4.0, 0.0]))
        .collect();
    let mut colors = vec![rect([0, 0, 128, HEIGHT], [3.0, 0.0, 0.0])];
    colors.extend(stripes);
    let sharp_front = Scene {
        color: [0.0, 0.0, 2.0],
        distance: far,
        colors,
        distances: vec![at(focus, [0, 0, 128, HEIGHT])],
    };
    let out = frame(&sharp_front, Some(settings));
    let mut front_error = 0.0f32;
    let mut red_behind = 0.0f32;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let p = pixel(&out, x, y);
            if x < 126 {
                front_error = front_error
                    .max((p[0] - 3.0).abs())
                    .max(p[1].abs())
                    .max(p[2].abs());
            }
            if x > 130 {
                red_behind = red_behind.max(p[0]);
            }
        }
    }
    // The stripes blurred to their average.
    let middle = pixel(&out, 192, HEIGHT / 2);
    println!(
        "sharp in front: its worst error {front_error}, red behind it at most {red_behind}, the \
         stripes' middle {middle:?}"
    );
    assert!(front_error < 1e-3, "{front_error}");
    assert!(red_behind < 0.05, "{red_behind}");
    assert!(
        (middle[1] - 2.0).abs() < 0.4 && (middle[2] - 1.0).abs() < 0.2,
        "{middle:?}"
    );

    // 4. A blurred object in front of a sharp background spreads over it as far as its
    // radius; beyond, the background is sharp.
    let near = 0.7;
    let spread = 2.0 * lens.near_radius(lens.q(depth_of(near)));
    let background: Vec<Rect> = (0..16)
        .map(|k| rect([128 + 8 * k, 0, 4, HEIGHT], [0.0, 0.0, 1.0]))
        .collect();
    let mut colors = vec![rect([0, 0, 128, HEIGHT], [4.0, 4.0, 4.0])];
    colors.extend(background);
    let blurred_front = Scene {
        color: [0.0, 0.0, 0.0],
        distance: focus,
        colors,
        distances: vec![at(near, [0, 0, 128, HEIGHT])],
    };
    let reference = frame(&blurred_front, None);
    let out = frame(&blurred_front, Some(settings));
    let y = HEIGHT / 2;
    // Just past the edge, a good part of the white; well beyond its radius, the background
    // itself.
    let edge = pixel(&out, 128 + (0.25 * spread) as u32, y);
    let mut beyond_error = 0.0f32;
    for yy in 0..HEIGHT {
        for x in (128 + spread as u32 + 6)..WIDTH {
            let (a, b) = (pixel(&out, x, yy), pixel(&reference, x, yy));
            beyond_error =
                beyond_error.max((0..3).map(|k| (a[k] - b[k]).abs()).fold(0.0, f32::max));
        }
    }
    println!(
        "blurred in front: spread {spread:.2} px, a quarter past the edge {edge:?}, the \
         background beyond its worst error {beyond_error}"
    );
    assert!(spread > 8.0, "the test needs a wide spread: {spread}");
    assert!(edge[0] > 0.8, "{edge:?}");
    assert!(beyond_error < 1e-3, "{beyond_error}");

    // 5. The same in front of a blurred background (behind the plane in focus), and in
    // front of one barely in front of the plane (its near radius under a pixel: not near
    // field, or the background's own sharp samples outweigh the blurred object, ADR 0095).
    for (name, distance) in [("a blurred background", far), ("one barely in front", 1.8)] {
        let scene = Scene {
            distance,
            colors: blurred_front.colors.clone(),
            distances: blurred_front.distances.clone(),
            ..blurred_front
        };
        let out = frame(&scene, Some(settings));
        let edge = pixel(&out, 128 + (0.25 * spread) as u32, y);
        println!("blurred in front of {name}: a quarter past the edge {edge:?}");
        assert!(edge[0] > 0.8, "{name}: {edge:?}");
    }

    // SAFETY: the device is idle.
    unsafe {
        graph.destroy(&mut allocator, Some(&bindless));
        allocator.destroy_buffer(readback, readback_memory);
        d.destroy_command_pool(pool, None);
    }
    drop(dof);
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
