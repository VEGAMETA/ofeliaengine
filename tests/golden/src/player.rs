//! Shots rendered by the player (`engine_app`, ADR 0099 step 4): the path games run. The
//! scene is cooked as the test bed's is, written to a pack and streamed by an offscreen
//! player; the sun, the local lights, the reflection probes and the GI volume are
//! entities; the camera is an entity the player renders through. A [`Shot`] with `player`
//! set goes here from [`crate::scene::render`], so one description renders both ways and
//! both are held to the same golden image.

use crate::scene::{
    RenderOptions, Rendered, Shot, Skip, Source, default_view, load, median, probe_grid,
};
use engine_app::{FrameParts, Game, Player, PlayerSettings, Window};
use engine_assets::write_pack;
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Quat, Vec3};
use engine_ecs::{GlobalTransform, Transform, World};
use engine_frame::Exposure;
use engine_render::forward::{ContactShadowSettings, LocalShadowSettings};
use engine_render::gi::ProbeGi;
use engine_render::reflections::ProbeSettings;
use engine_runtime::{
    Camera, GiVolume, PointLight, ReflectionProbe, SpotLight, Sun, look_rotation,
};
use engine_streaming::SceneStreamer;
use std::collections::BTreeMap;
use std::path::Path;

/// Frames after the camera's cut before the capture, beside a temporal shot's history:
/// screen-space reflections' own filter (as the test bed waits).
const SSR_FRAMES: usize = 8;
/// Frames in a row with nothing streaming before the scene counts as settled (texture
/// feedback lags a few frames, and a coarser level waits for its hold).
const QUIET_FRAMES: usize = 70;
/// The most frames a shot may take to settle.
const MAX_FRAMES: usize = 6_000;

/// The game: nothing moves; the world is set up before the first frame.
struct Still;

impl Game for Still {
    fn tick(&mut self, _world: &mut World, _dt: f32) {}
    fn animate(&mut self, _parts: &mut FrameParts<'_>, _dt: f32) {}
}

/// Fails on the fields the player's path does not take: those shots stay on the test
/// bed's own wiring.
fn check_supported(shot: &Shot) {
    let unsupported = [
        (
            "a procedural scene",
            matches!(shot.source, Source::Procedural),
        ),
        ("no sky", !shot.sky),
        ("an explicit prepass", shot.prepass),
        ("subsurface scattering", shot.subsurface),
        ("an animation", shot.animation.is_some()),
        ("a fixed render size", shot.render_size.is_some()),
        ("camera motion", shot.camera_motion != Vec3::ZERO),
        ("a texture budget", shot.texture_budget.is_some()),
        ("particles", shot.particles),
        ("a panorama", shot.panorama.is_some()),
        ("decals", shot.decals),
        ("Brixelizer GI", shot.brixelizer_gi),
        ("the sun going out", shot.sun_off_after.is_some()),
        ("GI readback", shot.gi_readback),
        ("a UI layer", shot.ui),
        ("frame generation", shot.frame_generation),
        (
            "material types",
            shot.stripes_floor || shot.shading_models || shot.fabric_curtains,
        ),
        ("several captures", shot.captures != 1),
        ("an async compute override", shot.async_compute.is_some()),
    ];
    let set: Vec<&str> = unsupported
        .iter()
        .filter(|(_, on)| *on)
        .map(|(what, _)| *what)
        .collect();
    assert!(
        set.is_empty(),
        "{}: the player's path does not take {set:?}",
        shot.name
    );
}

/// Renders `shot` through an offscreen player (see the module documentation).
///
/// # Panics
///
/// On fields the player's path does not take, and when a frame fails.
pub(crate) fn render(shot: &Shot, options: RenderOptions) -> Result<Rendered, Skip> {
    check_supported(shot);
    let loaded = load(&shot.source, false, false, false, shot.lods)?;
    let cooked = loaded.cooked.expect("a glTF scene");
    let jobs = ThreadPool::builder().build().expect("job system");
    let pack = std::env::temp_dir().join(format!(
        "engine_golden_player_{}_{}.pack",
        shot.name,
        std::process::id()
    ));
    write_pack(&pack, &cooked, &jobs, 1).expect("pack");
    drop(jobs);
    let rendered = render_pack(shot, options, &pack);
    let _ = std::fs::remove_file(&pack);
    rendered
}

fn render_pack(shot: &Shot, options: RenderOptions, pack: &Path) -> Result<Rendered, Skip> {
    let required = std::env::var("ENGINE_REQUIRE_GPU").is_ok_and(|v| v == "1");
    let settings = PlayerSettings {
        title: format!("engine_golden {}", shot.name),
        pack: Some(pack.to_path_buf()),
        window: Window::Offscreen {
            width: shot.size[0],
            height: shot.size[1],
        },
        validation: Some(options.validation),
        antialiasing: Some(shot.antialiasing),
        quality: Some(shot.quality),
        ambient_occlusion: Some(shot.ao),
        meshlet_culling: Some(shot.meshlets),
        local_shadows: shot.local_shadows.then(LocalShadowSettings::default),
        contact_shadows: shot.contact_shadows.then(ContactShadowSettings::default),
        reflection_probes: shot.probe_grid.map(|grid| ProbeSettings {
            capacity: grid.iter().product::<u32>().max(1),
            ..ProbeSettings::default()
        }),
        screen_space_reflections: shot.ssr,
        // The device class's, as the test bed's.
        global_illumination: if shot.gi.is_some() { None } else { Some(None) },
        // The test process stays as Windows started it; no cache file; variants made on
        // first use (the warm-up thread's work is not the frame's).
        process_setup: false,
        pipeline_cache: Some(None),
        warm_up: false,
        ..PlayerSettings::default()
    };
    let mut player = match Player::new(&settings) {
        Ok(player) => player,
        Err(e) if !required => return Err(Skip::NoGpu(e.to_string())),
        Err(e) => panic!("{}: the player: {e}", shot.name),
    };
    player.set_frame_time(Some(1.0 / 60.0));

    // The world: the sky's environment, the sun, the lights, the probes, the volume.
    let (min, max) = player.scene_info().bounds;
    {
        let setup = player.setup();
        setup
            .environment
            .set_atmosphere(
                setup.device,
                setup.allocator,
                setup.bindless,
                shot.atmosphere,
            )
            .expect("atmosphere");
        setup.environment.set_fog(shot.fog);
        setup.environment.set_intensity(shot.sky_intensity);
        let world = setup.world;
        let sun = shot.sun;
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
        for light in &shot.lights {
            let shadow = u32::from(shot.local_shadows);
            match light.spot {
                Some(spot) => world.spawn((
                    GlobalTransform(Affine3A::from_rotation_translation(
                        Quat::from_rotation_arc(Vec3::NEG_Z, spot.direction.normalize()),
                        light.position,
                    )),
                    SpotLight {
                        shadow,
                        ..SpotLight::new(light.color, light.range, spot.inner, spot.outer)
                    },
                )),
                None => world.spawn((
                    GlobalTransform(Affine3A::from_translation(light.position)),
                    PointLight {
                        shadow,
                        ..PointLight::new(light.color, light.range)
                    },
                )),
            };
        }
        if let Some(grid) = shot.probe_grid {
            for probe in probe_grid(min, max, grid) {
                world.spawn((
                    GlobalTransform(Affine3A::from_translation(probe.position)),
                    ReflectionProbe::new(probe.parallax.half_extents),
                ));
            }
        }
        if let Some(spacing) = shot.gi {
            world.spawn((
                GlobalTransform(Affine3A::from_translation((min + max) * 0.5)),
                GiVolume::new((max - min) * 0.5, spacing),
            ));
        }
    }
    // The camera, and the view settings it does not carry (as the test bed's).
    let (default_eye, default_target) = default_view(min, max);
    let eye = shot.eye.unwrap_or(default_eye);
    let target = shot.target.unwrap_or(default_target);
    let extent = (max - min).length();
    let camera = Camera {
        exposure: shot.exposure,
        ..player.camera(shot.fov_y.to_radians(), extent * 0.001, extent * 2.0)
    };
    let camera = player.world_mut().spawn((
        Transform::from_translation(eye).with_rotation(look_rotation(eye, target, Vec3::Y)),
        camera,
    ));
    {
        let base = player.view_settings_mut();
        base.occlusion_culling = shot.occlusion_culling;
        base.cached_shadows = shot.cached_shadows;
        base.exposure = Exposure::Fixed(shot.exposure);
        base.curve = shot.curve;
        base.grading = shot.grading;
        base.color_vision = shot.color_vision;
        base.depth_of_field = shot.depth_of_field;
        base.kind = shot.kind;
        base.bloom = shot.bloom;
        base.clear = Some([0.02, 0.02, 0.05, 1.0]);
    }

    // Frames until the world looks as it will: the view's textures and levels arrived and
    // nothing streams, the probes captured since, the GI volume converged. Then the
    // camera's cut starts a temporal shot's history, and the capture comes as many frames
    // later as the test bed's.
    let temporal = shot.antialiasing.is_temporal();
    let accumulate = match shot.capture_after {
        Some(frames) => frames as usize,
        None if temporal => shot.temporal_frames.max(1) as usize - 1,
        None => 0,
    } + if shot.ssr.is_some() { SSR_FRAMES } else { 0 };
    let mut game = Still;
    let mut quiet = 0;
    let mut since_cut: Option<usize> = None;
    let mut rgba8 = None;
    for _ in 0..MAX_FRAMES {
        let idle = player.streamer().is_none_or(SceneStreamer::idle);
        quiet = if idle { quiet + 1 } else { 0 };
        let probes_ready = player
            .reflection_probes()
            .is_none_or(|p| p.stats().pending == 0);
        let gi_ready = shot.gi.is_none()
            || player.gi_settings().is_none()
            || player.gi_volume().is_some_and(ProbeGi::is_converged);
        if since_cut.is_none()
            && player.scene_settled()
            && quiet >= QUIET_FRAMES
            && probes_ready
            && gi_ready
        {
            since_cut = Some(0);
            if let Some(camera) = player.world_mut().get_mut::<Camera>(camera) {
                camera.cut();
            }
        }
        if since_cut == Some(accumulate) {
            rgba8 = player.capture(&mut game).expect("frame");
            break;
        }
        player.frame(&mut game).expect("frame");
        if let Some(n) = &mut since_cut {
            *n += 1;
        }
    }
    let rgba8 =
        rgba8.unwrap_or_else(|| panic!("{}: not settled after {MAX_FRAMES} frames", shot.name));
    // GPU times of the frames after the capture.
    let mut timings: BTreeMap<&'static str, Vec<f64>> = BTreeMap::new();
    let mut frame_times = Vec::new();
    for _ in 0..options.timed_frames {
        player.frame(&mut game).expect("frame");
        let mut frame: BTreeMap<&'static str, f64> = BTreeMap::new();
        for time in player
            .graph()
            .gpu_pass_times()
            .iter()
            .filter(|t| !matches!(t.name, "offscreen frame" | "offscreen capture"))
        {
            *frame.entry(time.name).or_default() += f64::from(time.gpu_us);
        }
        for (name, us) in frame {
            timings.entry(name).or_default().push(us);
        }
        if let Some(us) = player.graph().gpu_frame_time() {
            frame_times.push(f64::from(us));
        }
    }
    let report = player.validation_report();
    assert!(
        !options.validation || report.errors == 0,
        "{}: validation errors: {:#?}",
        shot.name,
        report.first_errors
    );
    let mut gi_parts = Vec::new();
    if let Some(volume) = player.gi_volume() {
        gi_parts.push(("probe volume", volume.memory_bytes()));
    }
    if let Some(rays) = player.ray_scene() {
        let stats = rays.stats();
        gi_parts.push((
            "acceleration structures",
            stats.blas_bytes + stats.tlas_bytes,
        ));
    }
    let adapter = player.device().adapter();
    Ok(Rendered {
        adapter: adapter.summary(),
        vendor_id: adapter.vendor_id,
        width: shot.size[0],
        height: shot.size[1],
        frames: vec![rgba8.clone()],
        rgba8,
        pass_us: timings
            .into_iter()
            .map(|(name, mut v)| (name, median(&mut v)))
            .collect(),
        frame_us: median(&mut frame_times),
        startup_us: BTreeMap::new(),
        async_compute: player.graph().async_compute(),
        antialiasing: player.antialiasing(),
        gi_bytes: gi_parts.iter().map(|(_, bytes)| bytes).sum(),
        gi_parts,
        gi_settle: (0, 0.0),
        gi_means: Vec::new(),
        gi_image: None,
        ui_regions: Vec::new(),
    })
}
