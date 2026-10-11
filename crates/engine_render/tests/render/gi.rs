//! Global illumination from a volume of probes (ADR 0088): converged on the GPU against
//! scenes whose answers are known, then sampled at points:
//!
//! - a white furnace: a uniform sky and nothing in the way, so the irradiance is π times
//!   the sky's radiance at every point and normal;
//! - a closed black box in that sky: dark inside, the sky's irradiance outside, at points
//!   near its walls on both sides (no light through the walls);
//! - a white room whose ceiling glows red: the floor's irradiance is red;
//! - a sunlit floor: what faces it above gets its reflected light;
//! - a double-sided glowing box seen from inside: its back faces glow too;
//! - once converged, the volume follows a change (ADR 0089): a sun turned off fades out of
//!   it at the rate its hysteresis sets, first around the camera when the ray budget is
//!   small.
//!
//! Under the validation layer with synchronization validation.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_core::math::{Mat4, Quat, Vec3};
use engine_render::gi::{GiSettings, GiSky, ProbeGi, ProbeGrid};
use engine_render::gpu_scene::{GpuScene, SceneInstance};
use engine_render::graph::{BufferState, ImportedBuffer, RenderGraph};
use engine_render::material::{MaterialDesc, MaterialId, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, MeshId, cube};
use engine_render::rays::{RayScene, RaySettings};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render gi test"),
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

/// A box from `min` to `max` (the unit cube scaled and moved).
fn slab(min: Vec3, max: Vec3) -> Mat4 {
    Mat4::from_scale_rotation_translation(max - min, Quat::IDENTITY, (min + max) * 0.5)
}

/// What a scene puts in the GPU scene: boxes and their materials.
type Boxes = Vec<(Mat4, MaterialId)>;

/// Frames of updates after the convergence, before the sampling.
#[derive(Debug, Clone, Copy)]
struct After {
    /// The sun in them.
    sun: Option<(Vec3, Vec3)>,
    /// How many.
    frames: u32,
    /// The ray budget (`GiSettings::rays_per_frame`).
    rays_per_frame: u32,
    /// Its share around the camera (`GiSettings::local_share`).
    local_share: f32,
    /// The camera.
    eye: Vec3,
}

/// Converges a volume over `grid` in a scene of `boxes` under `sky`, then returns the
/// irradiance (rgb) and coverage (a) at each of `points` (position, normal), after the
/// frames of updates `after` describes, if any.
fn converge_and_sample(
    after: Option<After>,
    grid: ProbeGrid,
    sky: Vec3,
    sun: Option<(Vec3, Vec3)>,
    materials_of: impl FnOnce(&mut MaterialTable) -> Boxes,
    points: &[(Vec3, Vec3)],
) -> Option<Vec<[f32; 4]>> {
    let (instance, device) = gpu()?;
    println!("adapter: {}", device.adapter().summary());
    if device.ray_query().is_none() {
        assert!(
            !std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1"),
            "both reference GPUs have ray queries"
        );
        println!("skipping: no ray queries on this GPU");
        return None;
    }
    let d = device.raw();
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut scene = GpuScene::new(&device, &bindless, 64, 1).unwrap();
    let mut rays = RayScene::new(&device, &bindless, RaySettings::default(), 1).unwrap();
    let settings = GiSettings {
        rays: 256,
        bounces: 3,
        samples: 6,
        updates_per_frame: 9,
        rays_per_frame: after.map_or(0, |a| a.rays_per_frame),
        local_share: after.map_or(0.0, |a| a.local_share),
        ..GiSettings::default()
    };
    let mut gi = ProbeGi::new(&device, &mut allocator, &bindless, grid, settings).unwrap();
    let mesh: MeshId = geometry.add_mesh(&cube().as_data()).unwrap();
    for (world, material) in materials_of(&mut materials) {
        scene.insert(SceneInstance {
            mesh,
            material,
            world_from_object: world,
        });
    }

    let bytes = (points.len() * 16) as u64;
    let (readback, readback_memory) = allocator
        .create_buffer(
            &vk::BufferCreateInfo::default().size(bytes).usage(
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                    | vk::BufferUsageFlags::TRANSFER_DST,
            ),
            MemoryLocation::Readback,
        )
        .unwrap();
    // SAFETY: created with the device address usage.
    let readback_address = unsafe {
        d.get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(readback))
    };
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

    // Frames until the volume has converged (the first also builds the structures),
    // then one that samples it.
    let mut frame_number = 0u64;
    let mut frames_after = after.map_or(0, |a| a.frames);
    let results = loop {
        frame_number += 1;
        let converged = gi.is_converged();
        let (sun, sampling) = match (after, converged) {
            (Some(a), true) if frames_after > 0 => {
                frames_after -= 1;
                (a.sun, false)
            }
            _ => (sun, converged),
        };
        ring.begin_slot(0);
        // SAFETY: one-shot recording; nothing is in flight.
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
            textures
                .begin_frame(&mut fg, &mut allocator, &bindless, frame_number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, &device, &mut allocator, &textures, frame_number)
                .unwrap();
            let geo = geometry
                .begin_frame(&mut fg, &device, &mut allocator, frame_number)
                .unwrap();
            let scene_frame = scene
                .add_passes(
                    &mut fg,
                    &device,
                    &mut allocator,
                    &mut ring,
                    &bindless,
                    &geo,
                    &mats,
                    frame_number,
                )
                .unwrap()
                .unwrap();
            let ray_frame = rays
                .add_passes(
                    &mut fg,
                    &device,
                    &mut allocator,
                    &mut ring,
                    &bindless,
                    &geo,
                    &scene_frame,
                    frame_number,
                )
                .unwrap()
                .unwrap();
            let gi_frame = gi.begin_frame(&mut fg);
            if sampling {
                let data: Vec<[f32; 4]> = points
                    .iter()
                    .flat_map(|(p, n)| [p.extend(1.0).to_array(), n.extend(0.0).to_array()])
                    .collect();
                let address = ring.push(&data).unwrap();
                let out = fg.import_buffer(
                    "gi samples",
                    ImportedBuffer {
                        buffer: readback,
                        size: bytes,
                        address: readback_address,
                        initial: BufferState::idle(),
                        final_state: Some(BufferState::host_read()),
                    },
                );
                gi_frame
                    .add_sample_pass(
                        &mut fg,
                        &mut ring,
                        &bindless,
                        address,
                        points.len() as u32,
                        out,
                    )
                    .unwrap();
            } else {
                gi_frame
                    .add_update_passes(
                        &mut fg,
                        &mut ring,
                        &bindless,
                        &ray_frame,
                        &scene_frame,
                        &geo,
                        &mats,
                        after.map_or(Vec3::ZERO, |a| a.eye),
                        sun,
                        GiSky::Flat(sky),
                    )
                    .unwrap();
            }
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
        if sampling {
            let ptr = readback_memory.mapped_ptr().unwrap();
            // SAFETY: the GPU is idle; the mapping holds the samples.
            let floats = unsafe {
                core::slice::from_raw_parts(ptr.as_ptr().cast::<[f32; 4]>(), points.len())
            };
            break floats.to_vec();
        }
        assert!(
            frame_number < 8 + u64::from(after.map_or(0, |a| a.frames)),
            "the volume never converged"
        );
    };
    println!("{gi:?}, {} bytes", gi.memory_bytes());
    for ((p, n), r) in points.iter().zip(&results) {
        println!(
            "  at {p} facing {n}: irradiance {:.4} {:.4} {:.4}, covered {:.3}",
            r[0], r[1], r[2], r[3]
        );
    }

    // SAFETY: the device is idle.
    unsafe {
        gi.destroy(&device, &mut allocator, &bindless);
        rays.destroy(&device, &mut allocator);
        scene.destroy(&mut allocator);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        allocator.destroy_buffer(readback, readback_memory);
        d.destroy_command_pool(pool, None);
    }
    drop(gi);
    drop(rays);
    drop(scene);
    drop(bindless);
    drop(allocator);
    drop(device);
    let report = instance.validation_report();
    println!(
        "validation (with synchronization validation): errors={} warnings={}",
        report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
    Some(results)
}

fn close(a: f32, b: f32, tolerance: f32) -> bool {
    (a - b).abs() <= tolerance * b.abs().max(1e-3)
}

#[test]
fn a_uniform_sky_gives_pi_times_its_radiance() {
    let sky = Vec3::new(0.5, 0.25, 1.0);
    let grid = ProbeGrid::covering(Vec3::splat(-2.0), Vec3::splat(2.0), 1.0, 1000);
    let points = [
        (Vec3::ZERO, Vec3::Y),
        (Vec3::new(1.3, -0.7, 0.2), Vec3::NEG_X),
        (Vec3::new(-1.6, 1.6, -1.6), Vec3::new(1.0, 1.0, -1.0)),
    ];
    // One small grey box far away: the ray scene needs an instance.
    let Some(results) = converge_and_sample(
        None,
        grid,
        sky,
        None,
        |m| {
            let grey = m.add(MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 0.5));
            vec![(slab(Vec3::splat(500.0), Vec3::splat(501.0)), grey)]
        },
        &points,
    ) else {
        return;
    };
    let expected = sky * core::f32::consts::PI;
    for r in &results {
        assert!(
            close(r[0], expected.x, 0.02)
                && close(r[1], expected.y, 0.02)
                && close(r[2], expected.z, 0.02),
            "{r:?} vs {expected}"
        );
        assert!((r[3] - 1.0).abs() < 1e-3, "inside the volume: {r:?}");
    }
}

#[test]
fn light_does_not_pass_through_the_walls_of_a_closed_box() {
    let sky = Vec3::splat(1.0);
    // A closed black box from (-1, -1, -1) to (1, 1, 1), walls 0.2 thick, in a volume
    // reaching 1.5 beyond it; probes 0.5 apart.
    let grid = ProbeGrid::covering(Vec3::splat(-2.5), Vec3::splat(2.5), 0.5, 2000);
    let points = [
        // Inside, near each wall, facing into the box.
        (Vec3::new(0.0, -0.75, 0.0), Vec3::Y),
        (Vec3::new(0.75, 0.0, 0.0), Vec3::NEG_X),
        (Vec3::new(0.0, 0.0, -0.75), Vec3::Z),
        // Outside, near each of those walls, facing away from the box.
        (Vec3::new(0.0, -1.25, 0.0), Vec3::NEG_Y),
        (Vec3::new(1.25, 0.0, 0.0), Vec3::X),
        (Vec3::new(0.0, 0.0, -1.25), Vec3::NEG_Z),
    ];
    let Some(results) = converge_and_sample(
        None,
        grid,
        sky,
        None,
        |m| {
            let black = m.add(MaterialDesc::solid([0.0, 0.0, 0.0], 0.0, 0.5));
            let (o, i) = (1.0, 0.8);
            [
                (Vec3::new(-o, -o, -o), Vec3::new(o, -i, o)),
                (Vec3::new(-o, i, -o), Vec3::new(o, o, o)),
                (Vec3::new(-o, -i, -o), Vec3::new(-i, i, o)),
                (Vec3::new(i, -i, -o), Vec3::new(o, i, o)),
                (Vec3::new(-i, -i, -o), Vec3::new(i, i, -i)),
                (Vec3::new(-i, -i, i), Vec3::new(i, i, o)),
            ]
            .into_iter()
            .map(|(min, max)| (slab(min, max), black))
            .collect()
        },
        &points,
    ) else {
        return;
    };
    let open_sky = core::f32::consts::PI;
    for r in &results[..3] {
        assert!(r[0] < 0.05 * open_sky, "inside the box: {r:?}");
    }
    // Outside a wall a surface facing away from it sees half the sky's sphere: π, less
    // what the box below or beside the point hides of the hemisphere's edge.
    for r in &results[3..] {
        assert!(
            r[0] > 0.6 * open_sky && r[0] < 1.05 * open_sky,
            "outside the box: {r:?}"
        );
    }
}

#[test]
fn a_glowing_red_ceiling_lights_the_floor_red() {
    // A closed white room 4 × 2 × 4 under a red glowing ceiling, in a black sky.
    let grid = ProbeGrid::covering(
        Vec3::new(-1.8, 0.2, -1.8),
        Vec3::new(1.8, 1.8, 1.8),
        0.6,
        2000,
    );
    let points = [
        (Vec3::new(0.0, 0.05, 0.0), Vec3::Y),
        (Vec3::new(1.5, 1.0, 0.0), Vec3::NEG_X),
    ];
    let Some(results) = converge_and_sample(
        None,
        grid,
        Vec3::ZERO,
        None,
        |m| {
            let white = m.add(MaterialDesc::solid([0.8, 0.8, 0.8], 0.0, 0.5));
            let glow = m.add(MaterialDesc {
                base_color: [0.0, 0.0, 0.0, 1.0],
                emissive: [4.0, 0.2, 0.2],
                ..MaterialDesc::default()
            });
            let walls = [
                (Vec3::new(-2.2, -0.2, -2.2), Vec3::new(2.2, 0.0, 2.2)),
                (Vec3::new(-2.2, -0.2, -2.2), Vec3::new(-2.0, 2.2, 2.2)),
                (Vec3::new(2.0, -0.2, -2.2), Vec3::new(2.2, 2.2, 2.2)),
                (Vec3::new(-2.2, -0.2, -2.2), Vec3::new(2.2, 2.2, -2.0)),
                (Vec3::new(-2.2, -0.2, 2.0), Vec3::new(2.2, 2.2, 2.2)),
            ];
            let mut boxes: Boxes = walls
                .into_iter()
                .map(|(a, b)| (slab(a, b), white))
                .collect();
            boxes.push((
                slab(Vec3::new(-2.2, 2.0, -2.2), Vec3::new(2.2, 2.2, 2.2)),
                glow,
            ));
            boxes
        },
        &points,
    ) else {
        return;
    };
    for r in &results {
        assert!(r[0] > 1.0, "lit by the ceiling: {r:?}");
        assert!(r[0] > 5.0 * r[1] && r[0] > 5.0 * r[2], "red: {r:?}");
    }
}

#[test]
fn a_sunlit_floor_lights_what_is_above_it() {
    // A grey floor under a sun straight above (irradiance 3) in a black sky: a surface
    // above it facing down sees the floor's radiance (albedo × 3 / π) over its whole
    // hemisphere, irradiance albedo × 3 (a little less: the floor is 40 m wide).
    let grid = ProbeGrid::covering(
        Vec3::new(-2.0, 0.3, -2.0),
        Vec3::new(2.0, 2.3, 2.0),
        0.5,
        2000,
    );
    let points = [
        (Vec3::new(0.0, 1.0, 0.0), Vec3::NEG_Y),
        (Vec3::new(0.0, 1.0, 0.0), Vec3::Y),
    ];
    let Some(results) = converge_and_sample(
        None,
        grid,
        Vec3::ZERO,
        Some((Vec3::Y, Vec3::splat(3.0))),
        |m| {
            let grey = m.add(MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 0.5));
            vec![(
                slab(Vec3::new(-20.0, -0.2, -20.0), Vec3::new(20.0, 0.0, 20.0)),
                grey,
            )]
        },
        &points,
    ) else {
        return;
    };
    let down = results[0];
    assert!(
        down[0] > 1.35 && down[0] < 1.55,
        "facing the floor: {down:?}"
    );
    // Facing up: the black sky (and nothing else).
    assert!(results[1][0] < 0.05, "facing the sky: {:?}", results[1]);
}

#[test]
fn a_double_sided_glowing_box_lights_its_inside() {
    // A box of a double-sided glowing material (black, emission `glow`) around the
    // volume: every ray from inside hits a back face, which a double-sided material shows,
    // so the irradiance is π × glow at every point and normal (a furnace). Single-sided,
    // the same box would hold its probes inside geometry, dark.
    let glow = Vec3::new(0.25, 0.5, 1.0);
    let grid = ProbeGrid::covering(Vec3::splat(-1.5), Vec3::splat(1.5), 0.75, 1000);
    let points = [
        (Vec3::ZERO, Vec3::Y),
        (Vec3::new(0.5, -0.4, 0.3), Vec3::NEG_X),
    ];
    let Some(results) = converge_and_sample(
        None,
        grid,
        Vec3::ZERO,
        None,
        |m| {
            let sheet = m.add(MaterialDesc {
                base_color: [0.0, 0.0, 0.0, 1.0],
                emissive: glow.to_array(),
                double_sided: true,
                ..MaterialDesc::default()
            });
            vec![(slab(Vec3::splat(-2.0), Vec3::splat(2.0)), sheet)]
        },
        &points,
    ) else {
        return;
    };
    let expected = glow * core::f32::consts::PI;
    for r in &results {
        assert!(
            close(r[0], expected.x, 0.02)
                && close(r[1], expected.y, 0.02)
                && close(r[2], expected.z, 0.02),
            "{r:?} vs {expected}"
        );
    }
}

#[test]
fn a_deep_trench_lets_in_only_the_sky_above_it() {
    // A black trench 4 wide, 4 deep and 40 long under a uniform sky: its floor's middle
    // sees the sky through the opening only, over a view factor of sin(atan(2 / 4)) =
    // 0.447 (an infinite slot: crossed strings), and a wall at mid-depth, facing across,
    // (1 - sin(atan(2 / 4))) / 2 = 0.276; the walls and the floor reflect nothing.
    let sky = Vec3::splat(1.0);
    let grid = ProbeGrid::covering(
        Vec3::new(-2.0, 0.0, -6.0),
        Vec3::new(2.0, 4.0, 6.0),
        0.5,
        4000,
    );
    let points = [
        (Vec3::new(0.0, 0.02, 0.0), Vec3::Y),
        (Vec3::new(1.98, 2.0, 0.0), Vec3::NEG_X),
    ];
    let Some(results) = converge_and_sample(
        None,
        grid,
        sky,
        None,
        |m| {
            let black = m.add(MaterialDesc::solid([0.0, 0.0, 0.0], 0.0, 0.5));
            [
                (Vec3::new(-2.5, -0.5, -20.0), Vec3::new(2.5, 0.0, 20.0)),
                (Vec3::new(-2.5, 0.0, -20.0), Vec3::new(-2.0, 4.0, 20.0)),
                (Vec3::new(2.0, 0.0, -20.0), Vec3::new(2.5, 4.0, 20.0)),
            ]
            .into_iter()
            .map(|(min, max)| (slab(min, max), black))
            .collect()
        },
        &points,
    ) else {
        return;
    };
    let pi = core::f32::consts::PI;
    let floor = 0.447 * pi;
    let wall = 0.276 * pi;
    assert!(
        close(results[0][0], floor, 0.1),
        "the floor: {:?} vs {floor}",
        results[0]
    );
    assert!(
        close(results[1][0], wall, 0.15),
        "the wall: {:?} vs {wall}",
        results[1]
    );
}

/// The sunlit floor of `a_sunlit_floor_lights_what_is_above_it` with probes over `grid`,
/// converged; then the sun is off for `frames` frames of updates within `rays_per_frame`
/// (256 rays a probe) and `local_share` around `eye`, and the irradiance facing the floor
/// sampled at `points`.
fn sun_turned_off_for(
    grid: ProbeGrid,
    frames: u32,
    rays_per_frame: u32,
    local_share: f32,
    eye: Vec3,
    points: &[(Vec3, Vec3)],
) -> Option<Vec<f32>> {
    let results = converge_and_sample(
        Some(After {
            sun: None,
            frames,
            rays_per_frame,
            local_share,
            eye,
        }),
        grid,
        Vec3::ZERO,
        Some((Vec3::Y, Vec3::splat(3.0))),
        |m| {
            let grey = m.add(MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 0.5));
            vec![(
                slab(Vec3::new(-20.0, -0.2, -20.0), Vec3::new(20.0, 0.0, 20.0)),
                grey,
            )]
        },
        points,
    )?;
    Some(results.iter().map(|r| r[0]).collect())
}

#[test]
fn a_light_turned_off_fades_out_at_the_hysteresis_rate() {
    // Converged, the floor's light reaches the surface above it at about 1.45 (see
    // `a_sunlit_floor_lights_what_is_above_it`). With every probe traced every frame, each
    // update keeps 0.9 of a probe's light: 0.9^7 = 0.48 of it after 7 frames, 0.9^40 =
    // 1.5 % after 40.
    let grid = ProbeGrid::covering(
        Vec3::new(-2.0, 0.3, -2.0),
        Vec3::new(2.0, 2.3, 2.0),
        0.5,
        2000,
    );
    let middle = [(Vec3::new(0.0, 1.0, 0.0), Vec3::NEG_Y)];
    let every_probe = grid.probe_count() * 256;
    let Some(after_7) = sun_turned_off_for(grid, 7, every_probe, 0.0, Vec3::ZERO, &middle) else {
        return;
    };
    let Some(after_40) = sun_turned_off_for(grid, 40, every_probe, 0.0, Vec3::ZERO, &middle) else {
        return;
    };
    let (after_7, after_40) = (after_7[0], after_40[0]);
    let converged = 1.45;
    assert!(
        after_7 > 0.35 * converged && after_7 < 0.62 * converged,
        "after 7 frames: {after_7}"
    );
    assert!(after_40 < 0.05 * converged, "after 40 frames: {after_40}");
}

#[test]
fn a_small_budget_follows_the_change_around_the_camera_first() {
    // A volume of 41 x 5 x 41 probes over a 20 m floor; 64 probes a frame, half of them in
    // the box around the camera (8 x 4 x 8 = 256 probes), the rest in turn over all 8,405.
    // After 64 frames each probe by the camera has been traced 8 times (0.9^8 = 0.43 of
    // its light left), each one at the far corner once at most.
    let grid = ProbeGrid::covering(
        Vec3::new(-10.0, 0.3, -10.0),
        Vec3::new(10.0, 2.3, 10.0),
        0.5,
        10_000,
    );
    let eye = Vec3::new(-8.0, 1.0, -8.0);
    let points = [
        (Vec3::new(-8.0, 1.0, -8.0), Vec3::NEG_Y),
        (Vec3::new(8.0, 1.0, 8.0), Vec3::NEG_Y),
    ];
    let Some(after) = sun_turned_off_for(grid, 64, 64 * 256, 0.5, eye, &points) else {
        return;
    };
    let (near, far) = (after[0], after[1]);
    let converged = 1.45;
    assert!(near < 0.6 * converged, "by the camera: {near}");
    assert!(far > 0.85 * converged, "far from it: {far}");
}

#[test]
fn a_sunlit_wall_lights_the_floor_in_front_of_it() {
    // `engine_brixelizer`'s scene of the same name (ADR 0090): a white wall 8 m wide and
    // 4 m tall facing the sun (irradiance 3 × 0.8 on it: radiance 0.9 × 2.4 / π ≈ 0.69) on
    // a grey floor, under a black sky. Integrated over the wall, one bounce gives a floor
    // point 0.4 m in front of it irradiance 0.958, and 4.9 m in front 0.170. The sunlit
    // floor lights the wall too (its radiance ≈ 0.82 then): about 1.14 and 0.20. Probe GI
    // gives 1.19 and 0.20 on both GPUs; Brixelizer GI 0.22 to 0.23 and 0.08.
    let grid = ProbeGrid::covering(
        Vec3::new(-4.0, 0.1, -1.8),
        Vec3::new(4.0, 3.9, 3.5),
        0.5,
        4_000,
    );
    let points = [
        (Vec3::new(0.0, 0.0, -1.5), Vec3::Y),
        (Vec3::new(0.0, 0.0, 3.0), Vec3::Y),
    ];
    let Some(results) = converge_and_sample(
        None,
        grid,
        Vec3::ZERO,
        Some((Vec3::new(0.0, 0.6, 0.8), Vec3::splat(3.0))),
        |m| {
            let grey = m.add(MaterialDesc::solid([0.5, 0.5, 0.5], 0.0, 0.5));
            let white = m.add(MaterialDesc::solid([0.9, 0.9, 0.9], 0.0, 0.5));
            vec![
                (
                    slab(Vec3::new(-20.0, -0.2, -20.0), Vec3::new(20.0, 0.0, 20.0)),
                    grey,
                ),
                (
                    slab(Vec3::new(-4.0, 0.0, -2.1), Vec3::new(4.0, 4.0, -1.9)),
                    white,
                ),
            ]
        },
        &points,
    ) else {
        return;
    };
    let (near, far) = (results[0], results[1]);
    println!("in front of the wall {near:?}, 4.9 m away {far:?}");
    assert!(
        near[0] > 1.0 && near[0] < 1.3,
        "in front of the wall: {near:?}"
    );
    assert!(far[0] > 0.16 && far[0] < 0.24, "4.9 m away: {far:?}");
}
