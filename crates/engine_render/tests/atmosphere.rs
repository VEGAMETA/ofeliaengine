//! The atmosphere and height fog (ADR 0069) against numbers computed here on the CPU, in
//! double precision and with many more steps, under the validation layer with
//! synchronization validation.
//!
//! - **Transmittance table:** each texel is the transmittance of Bruneton's
//!   parameterization (ported from Hillaire's reference) integrated over 512 steps; straight
//!   up it has a closed form.
//! - **Multiple scattering table:** Hillaire's approximation (second-order light over 64
//!   directions, summed as L₂ / (1 − f_ms)) recomputed here with 5× the steps; the series
//!   converges (f_ms < 1) and adds a modest share of the light.
//! - **Sky-view table:** single scattering (multiple scattering off) along each texel's
//!   direction with 2,000 steps, Rayleigh and Cornette-Shanks phase functions, the planet's
//!   shadow and the light the ground reflects.
//! - **Aerial perspective froxels:** single scattering and transmittance from the camera
//!   along each froxel column's ray to the slice's depth.
//! - **The full-screen pass:** an emissive panel behind the aerial perspective is the
//!   panel × transmittance + light, read from the froxels as the hardware filters them.
//! - **Height fog:** a panel and the sky through the fog against the analytic transmittance
//!   and the in-scattering of the sky's average radiance and the sun.
//! - **The sun:** the directional light's color is the transmittance towards the sun:
//!   white at noon, red and dim at sunset (the table agrees with the CPU there).

#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::too_many_lines,
    clippy::too_many_arguments
)]

use engine_core::half;
use engine_core::math::{Mat4, Quat, Vec3, Vec4, look_at, vulkan_perspective};
use engine_render::atmosphere::{
    AERIAL_KM_PER_SLICE, AERIAL_SLICES, Atmosphere, AtmosphereTextures, HeightFog,
    MULTIPLE_SCATTERING_SIZE, SKY_VIEW_SIZE, TRANSMITTANCE_SIZE,
};
use engine_render::environment::{Environment, Sky};
use engine_render::forward::{
    Camera, DirectionalLight, DrawItem, ForwardRenderer, ForwardView, ShadowSettings,
};
use engine_render::graph::{
    Access, BufferState, FrameGraph, ImportedBuffer, RenderGraph, Texture, TextureDesc,
};
use engine_render::material::{MaterialDesc, MaterialTable};
use engine_render::mesh::{GeometryCapacity, GeometryPool, cube};
use engine_render::texture_pool::TexturePool;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{
    AdapterPreference, Allocator, Bindless, Device, Instance, InstanceDesc, MemoryLocation,
};

const SIZE: u32 = 128;
const HDR: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH: vk::Format = vk::Format::D32_SFLOAT;
const PI: f64 = core::f64::consts::PI;

fn gpu() -> Option<(Instance, Device)> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let attempt = || -> Result<(Instance, Device), engine_rhi::RhiError> {
        let instance = Instance::new(&InstanceDesc {
            app_name: String::from("engine_render atmosphere test"),
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

// ---------------------------------------------------------------------------------------
// The CPU reference, in double precision.

type V3 = [f64; 3];

fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn mul(a: V3, b: V3) -> V3 {
    [a[0] * b[0], a[1] * b[1], a[2] * b[2]]
}
fn scale(a: V3, s: f64) -> V3 {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn length(a: V3) -> f64 {
    dot(a, a).sqrt()
}
fn v3(v: Vec3) -> V3 {
    [f64::from(v.x), f64::from(v.y), f64::from(v.z)]
}
fn exp3(a: V3) -> V3 {
    [(-a[0]).exp(), (-a[1]).exp(), (-a[2]).exp()]
}

/// Rayleigh scattering, Mie scattering and extinction at `altitude` km.
fn medium(a: &Atmosphere, altitude: f64) -> (V3, V3, V3) {
    let h = altitude.max(0.0);
    let rayleigh = (-h / f64::from(a.rayleigh_scale_height)).exp();
    let mie = (-h / f64::from(a.mie_scale_height)).exp();
    let ozone = (1.0
        - (h - f64::from(a.ozone_peak_altitude)).abs() / f64::from(a.ozone_half_width))
    .max(0.0);
    let r = scale(v3(a.rayleigh_scattering), rayleigh);
    let m = scale(v3(a.mie_scattering), mie);
    let e = add(
        add(r, scale(v3(a.mie_scattering + a.mie_absorption), mie)),
        scale(v3(a.ozone_absorption), ozone),
    );
    (r, m, e)
}

/// Transmittance from radius `r` along cos zenith `mu` to the top (the CPU integration of
/// the library, 512 steps in double precision).
fn transmittance(a: &Atmosphere, r: f64, mu: f64) -> V3 {
    v3(a.transmittance(
        (r - f64::from(a.ground_radius)) as f32,
        mu.clamp(-1.0, 1.0) as f32,
    ))
}

/// Nearest intersection ahead of `o` along unit `d` with the sphere of radius `radius`.
fn ray_sphere(o: V3, d: V3, radius: f64) -> Option<f64> {
    let b = dot(d, o);
    let c = dot(o, o) - radius * radius;
    let delta = b * b - c;
    if delta < 0.0 {
        return None;
    }
    let s = delta.sqrt();
    let (t0, t1) = (-b - s, -b + s);
    if t0 < 0.0 && t1 < 0.0 {
        None
    } else if t0 < 0.0 {
        Some(t1.max(0.0))
    } else {
        Some(t0.max(0.0))
    }
}

fn rayleigh_phase(c: f64) -> f64 {
    3.0 / (16.0 * PI) * (1.0 + c * c)
}

fn mie_phase(g: f64, c: f64) -> f64 {
    let k = 3.0 / (8.0 * PI) * (1.0 - g * g) / (2.0 + g * g);
    k * (1.0 + c * c) / (1.0 + g * g - 2.0 * g * c).powf(1.5)
}

/// Single scattering (`phases`; else isotropic, as the multiple scattering table) from `o`
/// along `d` to the ground, the top or `limit`, and the transmittance; the ground's
/// reflection with `ground`. Also the integral of scattering × transmittance (f_ms's).
fn integrate(
    a: &Atmosphere,
    o: V3,
    d: V3,
    sun: V3,
    steps: usize,
    phases: bool,
    ground: bool,
    limit: f64,
) -> (V3, V3, V3) {
    let bottom = f64::from(a.ground_radius);
    let t_bottom = ray_sphere(o, d, bottom);
    let Some(t_top) = ray_sphere(o, d, f64::from(a.top_radius)) else {
        return ([0.0; 3], [1.0; 3], [0.0; 3]);
    };
    let t_max = t_bottom.map_or(t_top, |b| b.min(t_top)).min(limit);
    let dt = t_max / steps as f64;
    let c = dot(sun, d);
    let (mie_value, rayleigh_value) =
        (mie_phase(f64::from(a.mie_anisotropy), c), rayleigh_phase(c));
    let mut luminance = [0.0; 3];
    let mut multiple = [0.0; 3];
    let mut throughput = [1.0; 3];
    for i in 0..steps {
        let t = (i as f64 + 0.5) * dt;
        let p = add(o, scale(d, t));
        let radius = length(p);
        let (rayleigh, mie, extinction) = medium(a, radius - bottom);
        let up = scale(p, 1.0 / radius);
        let mu_sun = dot(sun, up);
        let to_sun = transmittance(a, radius, mu_sun);
        let shadow = if ray_sphere(p, sun, bottom + 0.01).is_some() {
            0.0
        } else {
            1.0
        };
        let scattering = add(rayleigh, mie);
        let phase_scattering = if phases {
            add(scale(mie, mie_value), scale(rayleigh, rayleigh_value))
        } else {
            scale(scattering, 1.0 / (4.0 * PI))
        };
        // Midpoint of the step: transmittance to it, then over half the step.
        let half = exp3(scale(extinction, dt * 0.5));
        let to_here = mul(throughput, half);
        luminance = add(
            luminance,
            scale(mul(to_here, mul(to_sun, phase_scattering)), shadow * dt),
        );
        multiple = add(multiple, scale(mul(to_here, scattering), dt));
        throughput = mul(throughput, exp3(scale(extinction, dt)));
    }
    if ground && t_bottom.is_some_and(|b| b <= t_max + 1e-9 && b > 0.0) {
        let p = add(o, scale(d, t_max));
        let radius = length(p);
        let mu_sun = dot(sun, scale(p, 1.0 / radius));
        let reflected = scale(
            mul(transmittance(a, radius, mu_sun), v3(a.ground_albedo)),
            mu_sun.max(0.0) / PI,
        );
        luminance = add(luminance, mul(reflected, throughput));
    }
    (luminance, throughput, multiple)
}

// The tables' parameterizations (Hillaire's reference, as `atmosphere.slang`).

fn transmittance_params(a: &Atmosphere, u: f64, v: f64) -> (f64, f64) {
    let (bottom, top) = (f64::from(a.ground_radius), f64::from(a.top_radius));
    let h = (top * top - bottom * bottom).sqrt();
    let rho = h * v;
    let r = (rho * rho + bottom * bottom).sqrt();
    let d_min = top - r;
    let d_max = rho + h;
    let d = d_min + u * (d_max - d_min);
    let mu = if d == 0.0 {
        1.0
    } else {
        (h * h - rho * rho - d * d) / (2.0 * r * d)
    };
    (r, mu.clamp(-1.0, 1.0))
}

fn from_sub_uv(u: f64, resolution: f64) -> f64 {
    (u - 0.5 / resolution) * (resolution / (resolution - 1.0))
}

fn sky_view_params(a: &Atmosphere, u: f64, v: f64, r: f64) -> (f64, f64) {
    let u = from_sub_uv(u, f64::from(SKY_VIEW_SIZE[0]));
    let v = from_sub_uv(v, f64::from(SKY_VIEW_SIZE[1]));
    let bottom = f64::from(a.ground_radius);
    let beta = ((r * r - bottom * bottom).max(0.0).sqrt() / r).acos();
    let zenith_horizon = PI - beta;
    let mu = if v < 0.5 {
        let coord = 1.0 - 2.0 * v;
        (zenith_horizon * (1.0 - coord * coord)).cos()
    } else {
        let coord = v * 2.0 - 1.0;
        (zenith_horizon + beta * coord * coord).cos()
    };
    (mu, -(u * u * 2.0 - 1.0))
}

// ---------------------------------------------------------------------------------------
// The test bed.

/// What a frame read back.
struct Frame {
    /// The color target, rgba.
    color: Vec<[f32; 4]>,
    /// The tables (rgba texels, row by row; the volume slice by slice).
    transmittance: Vec<[f32; 4]>,
    multiple: Vec<[f32; 4]>,
    sky_view: Vec<[f32; 4]>,
    aerial: Vec<[f32; 4]>,
}

const TABLE_TEXELS: [u32; 4] = [
    TRANSMITTANCE_SIZE[0] * TRANSMITTANCE_SIZE[1],
    MULTIPLE_SCATTERING_SIZE * MULTIPLE_SCATTERING_SIZE,
    SKY_VIEW_SIZE[0] * SKY_VIEW_SIZE[1],
    AERIAL_SLICES * AERIAL_SLICES * AERIAL_SLICES,
];

fn decode(raw: &[u8]) -> Vec<[f32; 4]> {
    raw.chunks_exact(8)
        .map(|p| {
            let h = |k: usize| half::to_f32(u16::from_le_bytes([p[2 * k], p[2 * k + 1]]));
            [h(0), h(1), h(2), h(3)]
        })
        .collect()
}

/// Copies `texture` (`width × height × depth` texels of 8 bytes) to `offset` in `out`.
fn read_back<'g>(
    fg: &mut FrameGraph<'g>,
    texture: Texture,
    out: engine_render::graph::Buffer,
    offset: u64,
    [width, height, depth]: [u32; 3],
) {
    fg.add_pass("read back")
        .read(texture, Access::TransferRead)
        .write(out, Access::TransferWrite)
        .execute(move |ctx| {
            let region = vk::BufferImageCopy {
                buffer_offset: offset,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_extent: vk::Extent3D {
                    width,
                    height,
                    depth,
                },
                ..Default::default()
            };
            // SAFETY: recording; layouts arranged by the graph; the region fits.
            unsafe {
                ctx.device.cmd_copy_image_to_buffer(
                    ctx.cmd,
                    ctx.image(texture),
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    ctx.buffer(out),
                    &[region],
                );
            }
        });
}

/// The camera at `eye` looking at `target`, 90° vertical field of view, square.
fn camera(eye: Vec3, target: Vec3) -> Camera {
    Camera {
        clip_from_world: vulkan_perspective(core::f32::consts::FRAC_PI_2, 1.0, 0.05, 5000.0)
            * look_at(eye, target, Vec3::Y),
        position: eye,
    }
}

/// The direction through normalized device coordinates `ndc` of `camera`.
fn ray(camera: &Camera, ndc: [f32; 2]) -> Vec3 {
    let inverse = camera.clip_from_world.inverse();
    let near = inverse * Vec4::new(ndc[0], ndc[1], 0.0, 1.0);
    let mid = inverse * Vec4::new(ndc[0], ndc[1], 0.5, 1.0);
    (mid.truncate() / mid.w - near.truncate() / near.w).normalize()
}

/// Bilinear (trilinear for the volume) filtering of read-back texels at `uvw`, clamped.
fn sample(texels: &[[f32; 4]], size: [u32; 3], uvw: [f32; 3]) -> [f32; 4] {
    let mut out = [0.0f32; 4];
    let coords: Vec<(u32, u32, f32)> = (0..3)
        .map(|k| {
            let x = (uvw[k] * size[k] as f32 - 0.5).clamp(0.0, (size[k] - 1) as f32);
            let i = (x.floor() as u32).min(size[k].saturating_sub(2));
            let f = if size[k] > 1 { x - i as f32 } else { 0.0 };
            (i, (i + 1).min(size[k] - 1), f)
        })
        .collect();
    for corner in 0..8 {
        let pick = |k: usize| {
            let (lo, hi, f) = coords[k];
            if corner & (1 << k) != 0 {
                (hi, f)
            } else {
                (lo, 1.0 - f)
            }
        };
        let ((x, wx), (y, wy), (z, wz)) = (pick(0), pick(1), pick(2));
        let t = texels[((z * size[1] + y) * size[0] + x) as usize];
        for c in 0..4 {
            out[c] += t[c] * wx * wy * wz;
        }
    }
    out
}

#[test]
fn the_atmosphere_and_fog_match_the_cpu() {
    let Some((instance, device)) = gpu() else {
        return;
    };
    println!("adapter: {}", device.adapter().summary());
    let mut allocator = Allocator::new(&device);
    let bindless = Bindless::new(&device).unwrap();
    let renderer =
        ForwardRenderer::new(&device, &bindless, HDR, DEPTH, ShadowSettings::default()).unwrap();
    let mut geometry =
        GeometryPool::new(&device, &mut allocator, GeometryCapacity::default(), 1).unwrap();
    let mesh = geometry.add_mesh(&cube().as_data()).unwrap();
    let mut textures = TexturePool::new(&device, 1 << 20, 1);
    let mut materials = MaterialTable::new(1);
    // An emissive panel with a black, rough base (its own reflections negligible).
    let panel = materials.add(MaterialDesc {
        emissive: [10.0, 10.0, 10.0],
        ..MaterialDesc::solid([0.0; 3], 0.0, 1.0)
    });
    let mut ring = UploadRing::new(&device, &mut allocator, 1, 4 << 20).unwrap();
    let mut graph = RenderGraph::new(&device, 1);
    let mut environment =
        Environment::new(&device, &mut allocator, &bindless, Sky::default()).unwrap();
    let color_bytes = u64::from(SIZE * SIZE * 8);
    let table_offsets: Vec<u64> = TABLE_TEXELS
        .iter()
        .scan(color_bytes, |offset, &texels| {
            let at = *offset;
            *offset += u64::from(texels) * 8;
            Some(at)
        })
        .collect();
    let bytes = table_offsets[3] + u64::from(TABLE_TEXELS[3]) * 8;
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

    let mut frame_number = 0u64;
    let mut render = |allocator: &mut Allocator,
                      environment: &Environment,
                      camera: Camera,
                      light: DirectionalLight,
                      draws: &[DrawItem]|
     -> Frame {
        frame_number += 1;
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
        ring.begin_slot(0);
        let mut tables = false;
        {
            // SAFETY: nothing is in flight.
            let mut fg = unsafe { graph.begin_slot(0, cmd) };
            textures
                .begin_frame(&mut fg, allocator, &bindless, frame_number)
                .unwrap();
            let mats = materials
                .begin_frame(&mut fg, &device, allocator, &textures, frame_number)
                .unwrap();
            let geo = geometry
                .begin_frame(&mut fg, &device, allocator, frame_number)
                .unwrap();
            let color = fg.create_texture("color", TextureDesc::new_2d(HDR, SIZE, SIZE));
            let depth = fg.create_texture("depth", TextureDesc::new_2d(DEPTH, SIZE, SIZE));
            let passes = renderer
                .add_passes(
                    &mut fg,
                    &mut ring,
                    &bindless,
                    &geo,
                    &mats,
                    draws,
                    &ForwardView {
                        camera,
                        light,
                        color,
                        depth,
                        clear: Some([0.0; 4]),
                        occlusion: None,
                        temporal: None,
                        lights: &[],
                        environment: Some(environment),
                        reflections: Default::default(),
                    },
                )
                .unwrap();
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
            read_back(&mut fg, color, out, 0, [SIZE, SIZE, 1]);
            if let Some(AtmosphereTextures {
                transmittance,
                multiple_scattering,
                sky_view,
                aerial,
            }) = passes.atmosphere
            {
                tables = true;
                let sizes = [
                    [TRANSMITTANCE_SIZE[0], TRANSMITTANCE_SIZE[1], 1],
                    [MULTIPLE_SCATTERING_SIZE, MULTIPLE_SCATTERING_SIZE, 1],
                    [SKY_VIEW_SIZE[0], SKY_VIEW_SIZE[1], 1],
                    [AERIAL_SLICES; 3],
                ];
                for (k, texture) in [transmittance, multiple_scattering, sky_view, aerial]
                    .into_iter()
                    .enumerate()
                {
                    read_back(&mut fg, texture, out, table_offsets[k], sizes[k]);
                }
            }
            fg.execute_with_bindless(allocator, Some(&bindless))
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
        // SAFETY: the GPU is idle; the mapping covers the buffer.
        let raw = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), bytes as usize) };
        let table = |k: usize| {
            if tables {
                let start = table_offsets[k] as usize;
                decode(&raw[start..start + TABLE_TEXELS[k] as usize * 8])
            } else {
                Vec::new()
            }
        };
        Frame {
            color: decode(&raw[..color_bytes as usize]),
            transmittance: table(0),
            multiple: table(1),
            sky_view: table(2),
            aerial: table(3),
        }
    };

    // A panel facing the camera `distance` units down −Z (front face at −distance).
    let panel_at = |distance: f32| DrawItem {
        mesh,
        material: panel,
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(4.0 * distance, 4.0 * distance, 0.01),
            Quat::IDENTITY,
            Vec3::new(0.0, 0.0, -distance - 0.005),
        ),
    };
    let elevation = 25.0f32.to_radians();
    let to_sun = Vec3::new(
        0.6 * elevation.cos(),
        elevation.sin(),
        -0.8 * elevation.cos(),
    );
    let illuminance = Vec3::new(2.0, 2.0, 2.0);
    let sun = DirectionalLight {
        direction: -to_sun,
        color: illuminance,
        ambient: Vec3::ZERO,
    };
    // Something to draw far below (the renderer draws nothing without draws).
    let hidden = DrawItem {
        world_from_object: Mat4::from_translation(Vec3::new(0.0, -1.0e4, 0.0)),
        ..panel_at(1.0)
    };

    // 1. The tables, single scattering only (the CPU has no multiple scattering table).
    let single = Atmosphere {
        multiple_scattering: 0.0,
        origin_altitude: 0.5,
        ..Atmosphere::earth()
    };
    environment
        .set_atmosphere(&device, &mut allocator, &bindless, Some(single))
        .unwrap();
    let view = camera(Vec3::new(0.0, 0.0, 0.0), Vec3::new(0.0, 0.05, -1.0));
    let frame = render(&mut allocator, &environment, view, sun, &[hidden]);

    // Transmittance: every 16th texel against the CPU's 512 steps; straight up against
    // the closed form.
    let mut worst = 0.0f64;
    for y in (0..TRANSMITTANCE_SIZE[1]).step_by(9) {
        for x in (0..TRANSMITTANCE_SIZE[0]).step_by(17) {
            let u = (f64::from(x) + 0.5) / f64::from(TRANSMITTANCE_SIZE[0]);
            let v = (f64::from(y) + 0.5) / f64::from(TRANSMITTANCE_SIZE[1]);
            let (r, mu) = transmittance_params(&single, u, v);
            let expected = transmittance(&single, r, mu);
            let got = frame.transmittance[(y * TRANSMITTANCE_SIZE[0] + x) as usize];
            for c in 0..3 {
                let error = (f64::from(got[c]) - expected[c]).abs();
                worst = worst.max(error);
                assert!(
                    error < 2e-3 + 5e-3 * expected[c],
                    "transmittance at r {r} mu {mu}: {got:?} vs {expected:?}"
                );
            }
        }
    }
    println!("transmittance table: largest error {worst:.5}");

    // Sky view: texels across the table (above, near and below the horizon, towards and
    // away from the sun) against 2,000 steps.
    let radius = f64::from(single.ground_radius + 0.5);
    let mu_sun = f64::from(to_sun.y);
    let sun_lut = [(1.0 - mu_sun * mu_sun).sqrt(), mu_sun, 0.0];
    let mut worst = 0.0f64;
    for &y in &[2u32, 20, 40, 50, 53, 54, 56, 70, 100] {
        for &x in &[0u32, 10, 48, 96, 150, 191] {
            let u = (f64::from(x) + 0.5) / f64::from(SKY_VIEW_SIZE[0]);
            let v = (f64::from(y) + 0.5) / f64::from(SKY_VIEW_SIZE[1]);
            let (mu, cos_azimuth) = sky_view_params(&single, u, v, radius);
            let sin_zenith = (1.0 - mu * mu).max(0.0).sqrt();
            let dir = [
                sin_zenith * cos_azimuth,
                mu,
                sin_zenith * (1.0 - cos_azimuth * cos_azimuth).max(0.0).sqrt(),
            ];
            let (expected, _, _) = integrate(
                &single,
                [0.0, radius, 0.0],
                dir,
                sun_lut,
                2000,
                true,
                true,
                1e9,
            );
            let got = frame.sky_view[(y * SKY_VIEW_SIZE[0] + x) as usize];
            for c in 0..3 {
                let error = (f64::from(got[c]) - expected[c]).abs() / expected[c].max(1e-3);
                worst = worst.max(error);
                assert!(
                    error < 0.03,
                    "sky view texel ({x}, {y}) mu {mu:.4} azimuth {cos_azimuth:.3}: {got:?} vs {expected:?}"
                );
            }
        }
    }
    println!("sky-view table: largest relative error {worst:.4}");

    // Aerial perspective: froxel columns' rays from the camera to slice depths.
    let camera_planet = [0.0, f64::from(single.ground_radius + 0.5), 0.0];
    let mut worst = (0.0f64, 0.0f64);
    for &(x, y) in &[(16u32, 16u32), (3, 2), (29, 5), (8, 12)] {
        let ndc = [
            (x as f32 + 0.5) / AERIAL_SLICES as f32 * 2.0 - 1.0,
            (y as f32 + 0.5) / AERIAL_SLICES as f32 * 2.0 - 1.0,
        ];
        let dir = v3(ray(&view, ndc));
        for &slice in &[0u32, 3, 8, 15, 24, 31] {
            let w = (f64::from(slice) + 0.5) / f64::from(AERIAL_SLICES);
            let depth = w * w * f64::from(AERIAL_SLICES) * f64::from(AERIAL_KM_PER_SLICE);
            let (light, through, _) = integrate(
                &single,
                camera_planet,
                dir,
                v3(to_sun),
                2000,
                true,
                false,
                depth,
            );
            let got = frame.aerial[((slice * AERIAL_SLICES + y) * AERIAL_SLICES + x) as usize];
            let opacity = 1.0 - (through[0] + through[1] + through[2]) / 3.0;
            for c in 0..3 {
                let error = (f64::from(got[c]) - light[c]).abs() / light[c].max(1e-4);
                worst.0 = worst.0.max(error);
                assert!(
                    error < 0.05,
                    "aerial ({x}, {y}, {slice}): {got:?} vs {light:?}"
                );
            }
            let error = (f64::from(got[3]) - opacity).abs();
            worst.1 = worst.1.max(error);
            assert!(
                error < 5e-3,
                "aerial opacity ({x}, {y}, {slice}): {} vs {opacity}",
                got[3]
            );
        }
    }
    println!(
        "aerial perspective: largest relative error {:.4}, opacity {:.5}",
        worst.0, worst.1
    );

    // 2. Multiple scattering on: the table against Hillaire's approximation computed here.
    let earth = Atmosphere {
        origin_altitude: 0.5,
        ..Atmosphere::earth()
    };
    environment
        .set_atmosphere(&device, &mut allocator, &bindless, Some(earth))
        .unwrap();
    let frame = render(&mut allocator, &environment, view, sun, &[hidden]);
    let n = f64::from(MULTIPLE_SCATTERING_SIZE);
    let mut worst = 0.0f64;
    for &(x, y) in &[
        (31u32, 0u32),
        (20, 0),
        (16, 3),
        (28, 10),
        (24, 25),
        (17, 31),
    ] {
        let u = from_sub_uv((f64::from(x) + 0.5) / n, n);
        let v = from_sub_uv((f64::from(y) + 0.5) / n, n);
        let mu_sun = u * 2.0 - 1.0;
        let sun = [0.0, mu_sun, (1.0 - mu_sun * mu_sun).max(0.0).sqrt()];
        let (bottom, top) = (f64::from(earth.ground_radius), f64::from(earth.top_radius));
        let r = bottom + (v + 0.01).clamp(0.0, 1.0) * (top - bottom - 0.01);
        let (mut second, mut f_ms) = ([0.0; 3], [0.0; 3]);
        for k in 0..64 {
            let i = 0.5 + (k / 8) as f64;
            let j = 0.5 + (k % 8) as f64;
            let theta = 2.0 * PI * i / 8.0;
            let phi = (1.0 - 2.0 * j / 8.0).acos();
            let dir = [theta.cos() * phi.sin(), phi.cos(), theta.sin() * phi.sin()];
            let (l, _, m) = integrate(&earth, [0.0, r, 0.0], dir, sun, 100, false, true, 1e9);
            second = add(second, scale(l, 1.0 / 64.0));
            f_ms = add(f_ms, scale(m, 1.0 / 64.0));
        }
        let expected: Vec<f64> = (0..3).map(|c| second[c] / (1.0 - f_ms[c])).collect();
        let got = frame.multiple[(y * MULTIPLE_SCATTERING_SIZE + x) as usize];
        for c in 0..3 {
            assert!(f_ms[c] > 0.0 && f_ms[c] < 1.0, "f_ms {f_ms:?}");
            let error = (f64::from(got[c]) - expected[c]).abs() / expected[c].max(1e-5);
            worst = worst.max(error);
            assert!(
                error < 0.05 || (f64::from(got[c]) - expected[c]).abs() < 2e-5,
                "multiple scattering texel ({x}, {y}): {got:?} vs {expected:?} (f_ms {f_ms:?})"
            );
        }
    }
    println!("multiple scattering table: largest relative error {worst:.4}");
    // Energy: every texel finite, non-negative; multiple scattering (and the light the
    // ground bounces back up) brightens the sky by a bounded share: the series converged.
    for t in frame
        .transmittance
        .iter()
        .chain(&frame.multiple)
        .chain(&frame.sky_view)
        .chain(&frame.aerial)
    {
        assert!(t.iter().all(|c| c.is_finite() && *c >= 0.0), "{t:?}");
    }
    assert!(
        frame
            .transmittance
            .iter()
            .all(|t| t[..3].iter().all(|&c| c <= 1.0))
    );
    let zenith_texel =
        |sky: &[[f32; 4]]| sky[(SKY_VIEW_SIZE[0] * 2 + SKY_VIEW_SIZE[0] / 2) as usize];
    let single_frame_zenith = {
        environment
            .set_atmosphere(&device, &mut allocator, &bindless, Some(single))
            .unwrap();
        zenith_texel(&render(&mut allocator, &environment, view, sun, &[hidden]).sky_view)
    };
    let multiple_zenith = zenith_texel(&frame.sky_view);
    for c in 0..3 {
        let ratio = multiple_zenith[c] / single_frame_zenith[c];
        println!("multiple / single scattering at the zenith, channel {c}: {ratio:.3}");
        assert!(ratio > 1.0 && ratio < 2.0, "{ratio}");
    }

    // 3. The sun: the light's color follows the table's transmittance at the camera.
    for degrees in [60.0f32, 4.0] {
        let e = degrees.to_radians();
        let to_sun = Vec3::new(0.0, e.sin(), -e.cos());
        let cpu = earth.sun_light(Vec3::ONE, to_sun, 0.5);
        let r = f64::from(earth.ground_radius + 0.5);
        let (bottom, top) = (f64::from(earth.ground_radius), f64::from(earth.top_radius));
        // The table's texel at (r, mu), as `transmittance_uv`.
        let mu = f64::from(to_sun.y);
        let h = (top * top - bottom * bottom).sqrt();
        let rho = (r * r - bottom * bottom).sqrt();
        let dist = -r * mu + (r * r * (mu * mu - 1.0) + top * top).sqrt();
        let u = (dist - (top - r)) / (rho + h - (top - r));
        let gpu = sample(
            &frame.transmittance,
            [TRANSMITTANCE_SIZE[0], TRANSMITTANCE_SIZE[1], 1],
            [u as f32, (rho / h) as f32, 0.5],
        );
        println!("sun at {degrees}°: CPU {cpu}, table {gpu:?}");
        for c in 0..3 {
            assert!(
                (gpu[c] - cpu[c]).abs() < 0.01 + 0.01 * cpu[c],
                "{gpu:?} vs {cpu}"
            );
        }
    }
    let noon = earth.sun_light(Vec3::ONE, Vec3::Y, 0.0);
    let sunset = earth.sun_light(
        Vec3::ONE,
        Vec3::new(0.0, 2f32.to_radians().sin(), -1.0).normalize(),
        0.0,
    );
    assert!(
        noon.x / noon.z < 1.3 && sunset.x / sunset.z > 10.0,
        "{noon} {sunset}"
    );

    // 4. The full-screen pass: the emissive panel behind the aerial perspective (scaled up
    // so 40 units are 40 km).
    let scaled = Atmosphere {
        aerial_perspective_scale: 1000.0,
        ..earth
    };
    let none = Atmosphere {
        aerial_perspective_scale: 0.0,
        ..earth
    };
    let front = camera(Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0));
    let distance = 40.0f32;
    environment
        .set_atmosphere(&device, &mut allocator, &bindless, Some(none))
        .unwrap();
    let bare = render(
        &mut allocator,
        &environment,
        front,
        sun,
        &[panel_at(distance)],
    );
    environment
        .set_atmosphere(&device, &mut allocator, &bindless, Some(scaled))
        .unwrap();
    let hazy = render(
        &mut allocator,
        &environment,
        front,
        sun,
        &[panel_at(distance)],
    );
    for &(px, py) in &[(64u32, 64u32), (20, 30), (100, 90)] {
        let ndc = [
            (px as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0,
            (py as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0,
        ];
        let dir = ray(&front, ndc);
        let t = (distance - 0.0) / -dir.z;
        let slice = t * 1e-3 * 1000.0 / AERIAL_KM_PER_SLICE;
        let w = (slice / AERIAL_SLICES as f32).sqrt();
        let uv = [
            (px as f32 + 0.5) / SIZE as f32,
            (py as f32 + 0.5) / SIZE as f32,
        ];
        let ap = sample(&hazy.aerial, [AERIAL_SLICES; 3], [uv[0], uv[1], w]);
        let base = bare.color[(py * SIZE + px) as usize];
        let got = hazy.color[(py * SIZE + px) as usize];
        for c in 0..3 {
            let expected = base[c] * (1.0 - ap[3]) + ap[c] * illuminance[c];
            assert!(
                (got[c] - expected).abs() < 0.01 * expected + 0.01,
                "pixel ({px}, {py}): {got:?} vs {expected} (base {base:?}, froxels {ap:?})"
            );
        }
        if (px, py) == (64, 64) {
            println!("panel through 40 km of air: {base:?} → {got:?}");
        }
    }

    // 5. Height fog over the gradient sky (uniform radiance 0.5, so its average is exact).
    environment
        .set_atmosphere(&device, &mut allocator, &bindless, None)
        .unwrap();
    let radiance = 0.5f32;
    environment.set_sky(Sky::uniform(Vec3::splat(radiance)));
    let fog = HeightFog {
        density: 0.02,
        base_height: 1.0,
        falloff: 0.15,
        albedo: Vec3::new(0.9, 0.8, 0.7),
        sun_anisotropy: 0.5,
        start_distance: 2.0,
        max_opacity: 0.97,
    };
    let eye = Vec3::new(0.0, 2.0, 0.0);
    let views = [
        camera(eye, eye + Vec3::new(0.0, 0.0, -1.0)),
        camera(eye, eye + Vec3::new(0.0, 0.7, -1.0)),
    ];
    let panel_far = DrawItem {
        world_from_object: Mat4::from_scale_rotation_translation(
            Vec3::new(30.0, 4.0, 0.01),
            Quat::IDENTITY,
            Vec3::new(0.0, 1.0, -25.005),
        ),
        ..panel_at(1.0)
    };
    let hg = |g: f32, c: f32| {
        let d = 1.0 + g * g - 2.0 * g * c;
        (1.0 - g * g) / (4.0 * core::f32::consts::PI * d * d.sqrt())
    };
    for (k, view) in views.iter().enumerate() {
        environment.set_fog(None);
        let clear = render(&mut allocator, &environment, *view, sun, &[panel_far]);
        environment.set_fog(Some(fog));
        let foggy = render(&mut allocator, &environment, *view, sun, &[panel_far]);
        for &(px, py) in &[(64u32, 64u32), (64, 100), (30, 64), (64, 10)] {
            let ndc = [
                (px as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0,
                (py as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0,
            ];
            let dir = ray(view, ndc);
            // The panel's front face (z = −25), or the sky.
            let hit = (-25.0 - eye.z) / dir.z;
            let point = eye + dir * hit;
            let on_panel = dir.z < 0.0 && point.x.abs() <= 15.0 && (point.y - 1.0).abs() <= 2.0;
            let end = if on_panel { point } else { eye + dir * 1.0e9 };
            let transmittance = fog.transmittance(eye, end);
            let inscattered = (1.0 - transmittance)
                * (Vec3::splat(radiance) + illuminance * hg(fog.sun_anisotropy, dir.dot(to_sun)))
                * fog.albedo;
            let base = clear.color[(py * SIZE + px) as usize];
            let got = foggy.color[(py * SIZE + px) as usize];
            for c in 0..3 {
                let expected = base[c] * transmittance + inscattered[c];
                assert!(
                    (got[c] - expected).abs() < 0.01 * expected + 2e-3,
                    "view {k} pixel ({px}, {py}) on panel {on_panel}: {got:?} vs {expected} \
                     (clear {base:?}, transmittance {transmittance})"
                );
            }
            if px == 64 && py == 64 {
                println!("fog, view {k}: {base:?} → {got:?} (transmittance {transmittance:.4})");
            }
        }
    }

    // SAFETY: the device is idle; nothing here is used again.
    unsafe {
        environment.destroy(&mut allocator, &bindless);
        graph.destroy(&mut allocator, Some(&bindless));
        ring.destroy(&mut allocator);
        geometry.destroy(&mut allocator);
        materials.destroy(&mut allocator);
        textures.destroy(&mut allocator, &bindless);
        allocator.destroy_buffer(readback, readback_memory);
        d.destroy_command_pool(pool, None);
    }
    drop((renderer, bindless, allocator));
    let report = instance.validation_report();
    assert_eq!(
        report.errors, 0,
        "validation errors: {:#?}",
        report.first_errors
    );
}
