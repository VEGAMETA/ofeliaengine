//! Videos in the player (ADR 0100), offscreen under synchronization validation: a video
//! opened in the game's set-up plays on a screen in the scene (a material's emission) and
//! in the UI (a sprite of its texture), and closing it gives the material back.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use engine_app::{FrameParts, Game, MaterialSlot, VideoDesc, VideoEvent, VideoId, Window};
use engine_core::math::{Vec2, Vec3};
use engine_ecs::{GlobalTransform, Transform, World};
use engine_frame::Placement;
use engine_render::material::MaterialDesc;
use engine_render::mesh::cube;
use engine_render2d::{Canvas, Quad};
use engine_runtime::MeshRenderer;

const CLIP: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../engine_video/tests/clips/svt_8bit_320x180.webm"
);

/// The test scene, with a video on a screen; its events counted.
struct Screen {
    cubes: common::Cubes,
    video: VideoId,
    shown: usize,
    close: bool,
}

impl Game for Screen {
    fn tick(&mut self, world: &mut World, dt: f32) {
        self.cubes.tick(world, dt);
    }

    fn animate(&mut self, parts: &mut FrameParts<'_>, dt: f32) {
        self.cubes.animate(parts, dt);
        self.shown += parts
            .videos
            .events(self.video)
            .filter(|e| matches!(e, VideoEvent::Shown(_)))
            .count();
        if self.close {
            parts.videos.close(self.video);
        }
    }
}

#[test]
fn a_video_plays_on_a_screen_and_in_the_ui() {
    let (width, height) = (320u32, 240u32);
    let Some((mut player, cubes)) = common::player_with(|settings| {
        settings.window = Window::Offscreen { width, height };
        settings.validation = Some(true);
        settings.video_decode = true;
    }) else {
        return;
    };
    let (video, screen) = {
        let setup = player.setup();
        let data: Arc<[u8]> = std::fs::read(CLIP).unwrap().into();
        let video = setup.videos.open(
            data,
            VideoDesc {
                sound: false,
                looping: true,
                ..VideoDesc::default()
            },
        );
        let screen = setup.materials.add(MaterialDesc {
            emissive: [1.0, 1.0, 1.0],
            ..MaterialDesc::solid([0.0, 0.0, 0.0], 0.0, 1.0)
        });
        setup.videos.show_in(video, screen, MaterialSlot::Emissive);
        let mesh = setup.geometry.add_mesh(&cube().as_data()).unwrap();
        setup.world.spawn((
            Transform::from_xyz(0.0, 1.2, 0.0).with_scale(Vec3::new(3.2, 1.8, 0.05)),
            GlobalTransform::IDENTITY,
            MeshRenderer::new(mesh, screen),
        ));
        (video, screen)
    };
    let mut game = Screen {
        cubes,
        video,
        shown: 0,
        close: false,
    };
    // The UI: a sprite of the video's texture in the top left corner, once it has a
    // picture.
    let mut index = None;
    for _ in 0..30 {
        player.frame(&mut game).unwrap();
        if index.is_none()
            && let Some(i) = player.setup().videos.texture_index(video)
        {
            index = Some(i);
            let mut hud = Canvas::new();
            hud.draw(&Quad::sprite(
                i,
                Vec2::new(56.0, 36.0),
                Vec2::new(96.0, 54.0),
            ));
            hud.prepare();
            player
                .ui_mut()
                .unwrap()
                .canvases
                .push((hud, Placement::Screen));
        }
    }
    let first = player.capture(&mut game).unwrap().unwrap();
    for _ in 0..20 {
        player.frame(&mut game).unwrap();
    }
    let second = player.capture(&mut game).unwrap().unwrap();
    println!("{} pictures shown in 52 frames", game.shown);
    assert!(index.is_some(), "the video's texture never got a picture");
    // 30 pictures a second, 60 frames a second.
    assert!((20..=27).contains(&game.shown), "{} pictures", game.shown);

    let region = |rgba: &[u8], x0: u32, y0: u32, w: u32, h: u32| -> Vec<[u8; 3]> {
        (y0..y0 + h)
            .flat_map(|y| (x0..x0 + w).map(move |x| (x, y)))
            .map(|(x, y)| {
                let i = ((y * width + x) * 4) as usize;
                [rgba[i], rgba[i + 1], rgba[i + 2]]
            })
            .collect()
    };
    // The sprite: the clip's colour bars, many colours, moving.
    let (sprite_a, sprite_b) = (
        region(&first, 12, 12, 88, 48),
        region(&second, 12, 12, 88, 48),
    );
    let colours = sprite_a.iter().collect::<BTreeSet<_>>().len();
    println!("the sprite shows {colours} colours");
    assert!(colours > 50, "{colours} colours in the sprite");
    assert_ne!(sprite_a, sprite_b, "the sprite's picture moved on");
    // The screen in the scene, above the cubes: lit by the video, not black.
    let screen_a = region(&first, width / 2 - 40, 40, 80, 30);
    let lit = screen_a
        .iter()
        .filter(|p| p.iter().any(|&c| c > 40))
        .count();
    let colours = screen_a.iter().collect::<BTreeSet<_>>().len();
    println!(
        "the screen: {lit} of {} pixels lit, {colours} colours",
        screen_a.len()
    );
    assert!(lit > screen_a.len() / 2 && colours > 30);

    // Closed: the material shows its factors alone again (a plain white emission).
    game.close = true;
    player.ui_mut().unwrap().canvases.clear();
    for _ in 0..5 {
        player.frame(&mut game).unwrap();
    }
    let closed = player.capture(&mut game).unwrap().unwrap();
    // `ENGINE_VIDEO_TEST_IMAGES=<dir>` keeps the captures, to look at.
    if let Some(dir) = std::env::var_os("ENGINE_VIDEO_TEST_IMAGES") {
        for (name, rgba) in [("first", &first), ("second", &second), ("closed", &closed)] {
            let png = engine_render::png::encode_png(
                width,
                height,
                engine_render::png::PngFormat::Rgba,
                rgba,
            )
            .unwrap();
            std::fs::write(
                std::path::Path::new(&dir).join(format!("video_{name}.png")),
                png,
            )
            .unwrap();
        }
    }
    let screen_closed = region(&closed, width / 2 - 40, 40, 80, 30);
    let colours = screen_closed.iter().collect::<BTreeSet<_>>().len();
    println!("closed, the screen shows {colours} colours");
    assert!(
        colours < 64,
        "{colours} colours: the video is still on the screen"
    );
    assert_eq!(player.setup().videos.texture(video), None);
    let _ = screen;

    let report = player.validation_report();
    println!(
        "validation: enabled={} errors={} warnings={}",
        report.enabled, report.errors, report.warnings
    );
    assert_eq!(report.errors, 0, "{:#?}", report.first_errors);
}
