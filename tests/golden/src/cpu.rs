//! A game's CPU stages on the player, for `perf_gate --cpu` (core audit 3: the perf gate's
//! GPU passes do not see the CPU, and a frame renderer stage twelve times slower passed every
//! gate).
//!
//! The game: Sponza from its pack, streamed by an offscreen player at 1920 × 1080 with the
//! device class's defaults; its 7 shadowed local lights; 2,000 cubes in hierarchies of 10
//! along the nave, their roots spinning every tick (moving casters: what the GPU scene's
//! extraction and upload and the shadows' caches pay for). After the scene settles, each
//! stage's median over the timed frames.

use crate::scene::{Skip, Source, default_view, load, sponza_lights, sun_at, vendor_name};
use engine_app::{Game, Player, PlayerSettings, Window};
use engine_assets::write_pack;
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Quat, Vec3};
use engine_ecs::{GlobalTransform, QueryState, Transform, World, bytemuck, pod_component};
use engine_render::forward::LocalShadowSettings;
use engine_render::material::MaterialDesc;
use engine_render::mesh::cube;
use engine_runtime::{MeshRenderer, PointLight, SpotLight, Sun, look_rotation};
use engine_streaming::SceneStreamer;
use std::path::PathBuf;

/// The stages of `engine_app::FrameReport` the check compares, and the frame's own CPU.
pub const STAGES: [&str; 14] = [
    "simulate",
    "propagate",
    "extract",
    "interpolate",
    "animate",
    "probes",
    "streaming",
    "pools",
    "upload",
    "compose",
    "execute",
    "submit",
    "present",
    "cpu frame",
];

/// Moving cubes: 200 hierarchies of 10.
const MOVERS: usize = 2_000;
/// Frames in a row with nothing streaming before the scene counts as settled.
const QUIET_FRAMES: usize = 70;
/// The most frames the scene may take to settle.
const MAX_FRAMES: usize = 6_000;

/// Turns its entity about Y, radians per second.
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Spin(f32);
pod_component!(Spin);

struct Movers {
    spins: QueryState<(&'static mut Transform, &'static Spin)>,
}

impl Game for Movers {
    fn tick(&mut self, world: &mut World, dt: f32) {
        self.spins.for_each(world, |(t, spin)| {
            t.rotation *= Quat::from_rotation_y(spin.0 * dt);
        });
    }
}

/// One run: the adapter's description, its vendor, and each of [`STAGES`]' median µs over
/// `frames` frames.
///
/// # Errors
///
/// Without a GPU or the Sponza test asset.
pub fn measure(frames: usize) -> Result<(String, String, Vec<f64>), Skip> {
    let source = Source::Gltf(PathBuf::from("Sponza/Sponza.gltf"));
    let loaded = load(&source, false, false, false, true)?;
    let cooked = loaded.cooked.expect("a glTF scene");
    let jobs = ThreadPool::builder().build().expect("job system");
    let pack = std::env::temp_dir().join(format!("engine_golden_cpu_{}.pack", std::process::id()));
    write_pack(&pack, &cooked, &jobs, 1).expect("pack");
    drop(jobs);
    let measured = measure_pack(&pack, frames);
    let _ = std::fs::remove_file(&pack);
    measured
}

fn measure_pack(pack: &std::path::Path, frames: usize) -> Result<(String, String, Vec<f64>), Skip> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let settings = PlayerSettings {
        title: String::from("engine_golden cpu"),
        pack: Some(pack.to_path_buf()),
        window: Window::Offscreen {
            width: 1920,
            height: 1080,
        },
        validation: Some(false),
        local_shadows: Some(LocalShadowSettings::default()),
        // The process as Windows started it; no cache file; variants on first use.
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => return Err(Skip::NoGpu(e.to_string())),
        Err(e) => panic!("cpu: the player: {e}"),
    };
    let info = player.device().adapter();
    let adapter = info.summary();
    let vendor = vendor_name(info.vendor_id);
    player.set_frame_time(Some(1.0 / 60.0));
    let (min, max) = player.scene_info().bounds;
    let size = max - min;
    {
        let setup = player.setup();
        let cube_mesh = setup
            .geometry
            .add_mesh(&cube().as_data())
            .expect("cube mesh");
        let paints: Vec<_> = (0..16)
            .map(|k| {
                let h = k as f32 / 16.0;
                setup.materials.add(MaterialDesc::solid(
                    [0.3 + 0.6 * h, 0.55, 0.9 - 0.6 * h],
                    0.0,
                    0.5,
                ))
            })
            .collect();
        let world = setup.world;
        let sun = sun_at(75.0);
        world.spawn((
            GlobalTransform(Affine3A::from_rotation_translation(
                Quat::from_rotation_arc(Vec3::NEG_Z, sun.direction.normalize()),
                Vec3::ZERO,
            )),
            Sun {
                color: sun.color,
                ambient: sun.ambient,
            },
        ));
        for light in sponza_lights() {
            match light.spot {
                Some(spot) => world.spawn((
                    GlobalTransform(Affine3A::from_rotation_translation(
                        Quat::from_rotation_arc(Vec3::NEG_Z, spot.direction.normalize()),
                        light.position,
                    )),
                    SpotLight {
                        shadow: 1,
                        ..SpotLight::new(light.color, light.range, spot.inner, spot.outer)
                    },
                )),
                None => world.spawn((
                    GlobalTransform(Affine3A::from_translation(light.position)),
                    PointLight {
                        shadow: 1,
                        ..PointLight::new(light.color, light.range)
                    },
                )),
            };
        }
        // The flight's movers (`engine_app/examples/flight.rs`).
        let hierarchies = MOVERS.div_ceil(10);
        for s in 0..hierarchies {
            let f = (s as f32 + 0.5) / hierarchies as f32;
            let root = world.spawn((
                Transform::from_xyz(
                    min.x + size.x * (0.1 + 0.8 * f),
                    min.y + size.y * (0.1 + 0.25 * ((s * 7) % 10) as f32 / 10.0),
                    min.z + size.z * (0.3 + 0.4 * ((s * 3) % 10) as f32 / 10.0),
                )
                .with_scale(Vec3::splat(0.25)),
                GlobalTransform::IDENTITY,
                MeshRenderer::new(cube_mesh, paints[s % paints.len()]),
                Spin(0.5 + (s % 5) as f32 * 0.3),
            ));
            for c in 0..9 {
                let a = c as f32 * 0.7;
                let child = world.spawn((
                    Transform::from_xyz(a.cos() * 2.0, 0.6 * c as f32, a.sin() * 2.0)
                        .with_scale(Vec3::splat(0.6)),
                    GlobalTransform::IDENTITY,
                    MeshRenderer::new(cube_mesh, paints[(s + c) % paints.len()]),
                ));
                world.set_parent(child, Some(root)).expect("parent");
            }
        }
    }
    let (eye, target) = default_view(min, max);
    let extent = (max - min).length();
    let camera = player.camera(1.0, extent * 0.001, extent * 2.0);
    player.world_mut().spawn((
        Transform::from_translation(eye).with_rotation(look_rotation(eye, target, Vec3::Y)),
        camera,
    ));
    let mut game = Movers {
        spins: QueryState::new(player.world_mut()),
    };
    // Settle: nothing streaming for a while.
    let mut quiet = 0;
    for _ in 0..MAX_FRAMES {
        let idle = player.streamer().is_none_or(SceneStreamer::idle);
        quiet = if idle { quiet + 1 } else { 0 };
        if quiet >= QUIET_FRAMES {
            break;
        }
        player.frame(&mut game).expect("frame");
    }
    let mut columns: Vec<Vec<f64>> = vec![Vec::with_capacity(frames); STAGES.len()];
    while columns[0].len() < frames {
        let Some(r) = player.frame(&mut game).expect("frame") else {
            continue;
        };
        let values = [
            r.simulate,
            r.propagate,
            r.extract,
            r.interpolate,
            r.animate,
            r.probes,
            r.streaming,
            r.pools,
            r.upload,
            r.compose,
            r.execute,
            r.submit,
            r.present,
            r.cpu,
        ];
        for (column, v) in columns.iter_mut().zip(values) {
            column.push(v);
        }
    }
    let medians = columns
        .iter_mut()
        .map(|c| {
            c.sort_by(f64::total_cmp);
            c[c.len() / 2]
        })
        .collect();
    Ok((adapter, vendor, medians))
}
