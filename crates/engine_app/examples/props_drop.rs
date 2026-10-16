//! Props dropped on Sponza (ADR 0101, 8c-4), rendered: concave props (U-shaped benches,
//! L-shaped walls, tables and chairs made of overlapping boards, crates) split into convex
//! parts by the cook, each scaled another way (uniform, along its axes, mirrored), fall
//! into the courtyard of a cooked Sponza and pile up on its floor and planters. Frames are
//! written as PNGs for a video.
//!
//! The level is the pack's (its colliders baked by `ofeliaengine cook`); the props are the
//! game's own: their meshes added to the player, their colliders made by the same cook step
//! (`cook_colliders`) and placed by `ScenePhysics`, their bodies moving their entities.
//!
//! ```text
//! ofeliaengine cook test-assets/Sponza/Sponza.gltf -o sponza.pack
//! cargo run -p engine_app --release --example props_drop -- sponza.pack <frames dir>
//!     [--seconds <s>]      game time (default 5)
//!     [--every <n>]        a frame written every n (default 2: 30 a second)
//!     [--size <w>x<h>]     (default 1280x720)
//! ```

#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used,
    missing_docs
)]

use engine_app::{Game, Player, PlayerSettings, ScenePhysics, Window};
use engine_assets::colliders::{ColliderRules, DefaultBodies, cook_colliders};
use engine_assets::{ImportedScene, MeshData, NodeData, Primitive};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Affine3A, Quat, Vec3};
use engine_ecs::{Entity, GlobalTransform, Transform, World};
use engine_physics::{PhysicsBackend, PhysicsSettings, QueryFilter, Ray};
use engine_render::material::MaterialDesc;
use engine_render::mesh::OwnedMeshData;
use engine_render::png::{PngFormat, encode_png};
use engine_runtime::{MeshRenderer, Sun, look_rotation};
use std::path::PathBuf;
use std::time::Instant;

/// A closed mesh: positions and outward-wound triangles.
#[derive(Default, Clone)]
struct Solid {
    positions: Vec<[f32; 3]>,
    indices: Vec<u32>,
}

impl Solid {
    /// A box from `lo` to `hi`, added to the mesh.
    fn board(mut self, lo: Vec3, hi: Vec3) -> Self {
        let base = self.positions.len() as u32;
        for i in 0..8 {
            self.positions.push([
                if i & 1 == 0 { lo.x } else { hi.x },
                if i & 2 == 0 { lo.y } else { hi.y },
                if i & 4 == 0 { lo.z } else { hi.z },
            ]);
        }
        let faces = [
            [0, 4, 6, 0, 6, 2],
            [1, 3, 7, 1, 7, 5],
            [0, 1, 5, 0, 5, 4],
            [2, 6, 7, 2, 7, 3],
            [0, 2, 3, 0, 3, 1],
            [4, 5, 7, 4, 7, 6],
        ];
        self.indices
            .extend(faces.iter().flatten().map(|&i| base + i));
        self
    }

    /// A prism: the counter-clockwise outline (x, z), from y = 0 to `height`.
    fn prism(outline: &[(f32, f32)], caps: &[[u32; 3]], height: f32) -> Self {
        let n = outline.len() as u32;
        let mut positions: Vec<[f32; 3]> = outline.iter().map(|&(x, z)| [x, 0.0, z]).collect();
        positions.extend(outline.iter().map(|&(x, z)| [x, height, z]));
        let mut indices = Vec::new();
        for &[a, b, c] in caps {
            indices.extend([a, b, c, a + n, c + n, b + n]);
        }
        for k in 0..n {
            let k1 = (k + 1) % n;
            indices.extend([k, k + n, k1, k1, k + n, k1 + n]);
        }
        Self { positions, indices }
    }

    /// For the collider: the glTF-like primitive the cook reads.
    fn primitive(&self) -> Primitive {
        let (lo, hi) = self
            .positions
            .iter()
            .fold(([f32::MAX; 3], [f32::MIN; 3]), |(lo, hi), p| {
                (
                    [lo[0].min(p[0]), lo[1].min(p[1]), lo[2].min(p[2])],
                    [hi[0].max(p[0]), hi[1].max(p[1]), hi[2].max(p[2])],
                )
            });
        Primitive {
            positions: self.positions.clone(),
            normals: vec![[0.0, 1.0, 0.0]; self.positions.len()],
            uvs: Vec::new(),
            tangents: Vec::new(),
            indices: self.indices.clone(),
            material: None,
            min: lo,
            max: hi,
            joints: Vec::new(),
            weights: Vec::new(),
        }
    }

    /// For drawing: flat-shaded, a vertex per corner of every triangle.
    fn render(&self) -> OwnedMeshData {
        let mut mesh = OwnedMeshData::default();
        for t in self.indices.chunks_exact(3) {
            let [a, b, c] = [t[0], t[1], t[2]].map(|i| Vec3::from(self.positions[i as usize]));
            let n = (b - a).cross(c - a).normalize_or_zero().to_array();
            for p in [a, b, c] {
                mesh.indices.push(mesh.positions.len() as u32);
                mesh.positions.push(p.to_array());
                mesh.normals.push(n);
                mesh.uvs.push([0.0, 0.0]);
            }
        }
        mesh
    }
}

/// The props' shapes, in metres: a U-shaped bench, an L-shaped wall, a crate, a table and
/// a chair (boards overlapping where they join: not a closed mesh, the cook remeshes it).
fn shapes() -> Vec<(&'static str, Solid, [f32; 3])> {
    let u = [
        (0.0, 0.0),
        (3.0, 0.0),
        (3.0, 2.0),
        (2.0, 2.0),
        (2.0, 1.0),
        (1.0, 1.0),
        (1.0, 2.0),
        (0.0, 2.0),
    ];
    let u = u.map(|(x, z)| (x * 0.25, z * 0.25));
    let l = [
        (0.0, 0.0),
        (2.0, 0.0),
        (2.0, 0.5),
        (0.5, 0.5),
        (0.5, 2.0),
        (0.0, 2.0),
    ]
    .map(|(x, z)| (x * 0.3, z * 0.3));
    let leg = |x: f32, z: f32, h: f32, w: f32| (Vec3::new(x, 0.0, z), Vec3::new(x + w, h, z + w));
    let mut table = Solid::default().board(Vec3::new(0.0, 0.66, 0.0), Vec3::new(1.2, 0.72, 0.8));
    for (x, z) in [(0.05, 0.05), (1.09, 0.05), (0.05, 0.69), (1.09, 0.69)] {
        let (lo, hi) = leg(x, z, 0.69, 0.06);
        table = table.board(lo, hi);
    }
    let mut chair = Solid::default()
        .board(Vec3::new(0.0, 0.42, 0.0), Vec3::new(0.45, 0.47, 0.45))
        .board(Vec3::new(0.0, 0.45, 0.4), Vec3::new(0.45, 0.95, 0.45));
    for (x, z) in [(0.02, 0.02), (0.39, 0.02), (0.02, 0.39), (0.39, 0.39)] {
        let (lo, hi) = leg(x, z, 0.44, 0.04);
        chair = chair.board(lo, hi);
    }
    vec![
        (
            "u",
            Solid::prism(
                &u,
                &[
                    [0, 1, 4],
                    [0, 4, 5],
                    [1, 2, 3],
                    [1, 3, 4],
                    [0, 5, 6],
                    [0, 6, 7],
                ],
                0.4,
            ),
            [0.85, 0.35, 0.2],
        ),
        (
            "l",
            Solid::prism(&l, &[[0, 1, 2], [0, 2, 3], [0, 3, 4], [0, 4, 5]], 0.5),
            [0.25, 0.5, 0.85],
        ),
        (
            "crate",
            Solid::default().board(Vec3::ZERO, Vec3::splat(0.4)),
            [0.75, 0.6, 0.3],
        ),
        ("table", table, [0.55, 0.35, 0.2]),
        ("chair", chair, [0.3, 0.65, 0.35]),
    ]
}

/// The game: the physics stepped every tick once `running`, its bodies' entities smoothed
/// every frame.
struct Drop {
    physics: ScenePhysics,
    running: bool,
}

impl Game for Drop {
    fn tick(&mut self, world: &mut World, dt: f32) {
        if !self.running {
            return;
        }
        if let Err(e) = self.physics.step(world, dt) {
            println!("physics: {e}");
        }
    }

    fn present(&mut self, world: &mut World, alpha: f32) {
        self.physics.interpolate(world, alpha);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let option = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
    };
    let (Some(pack), Some(out)) = (args.first(), args.get(1)) else {
        println!(
            "usage: props_drop <sponza.pack> <frames dir> [--seconds s] [--every n] [--size wxh]"
        );
        return;
    };
    let seconds: f64 = option("--seconds").map_or(5.0, |s| s.parse().unwrap());
    let every: usize = option("--every").map_or(2, |s| s.parse().unwrap());
    let (width, height) = option("--size").map_or((1280, 720), |s| {
        let (w, h) = s.split_once('x').unwrap();
        (w.parse().unwrap(), h.parse().unwrap())
    });
    let out = PathBuf::from(out);
    std::fs::create_dir_all(&out).unwrap();
    let start = Instant::now();
    let mut player = Player::new(&PlayerSettings {
        title: String::from("props_drop"),
        pack: Some(PathBuf::from(pack)),
        window: Window::Offscreen { width, height },
        process_setup: false,
        pipeline_cache: Some(None),
        ..PlayerSettings::default()
    })
    .unwrap();
    let mut physics = ScenePhysics::new(&PhysicsSettings::default()).unwrap();
    let level = player.load_physics(&mut physics).unwrap();
    println!("level: {level:?}");
    let (lo, hi) = player.scene_info().bounds;
    let (centre, size) = ((lo + hi) * 0.5, hi - lo);
    let floor = physics
        .physics()
        .raycast(
            &Ray {
                origin: Vec3::new(centre.x, lo.y + 0.6 * size.y, centre.z),
                direction: Vec3::new(0.0, -size.y, 0.0),
            },
            QueryFilter::default(),
        )
        .map_or(lo.y, |h| h.point.y);

    // Three layers of seven props down the courtyard, each turned and scaled its own way.
    let shapes = shapes();
    let scales = [
        Vec3::ONE,
        Vec3::splat(0.6),
        Vec3::splat(1.5),
        Vec3::new(1.6, 0.6, 1.0),
        Vec3::new(1.0, 1.7, 1.0),
        Vec3::new(-1.0, 1.0, 1.0),
        Vec3::new(1.0, 1.0, -1.3),
    ];
    let mut seed = 0x5EED_u64;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f32 / (1u64 << 53) as f32
    };
    let mut nodes = Vec::new();
    for layer in 0..3 {
        for column in -3..=3 {
            let k = nodes.len();
            let shape = (k * 3 + layer) % shapes.len();
            let scale = scales[(k * 5 + 1) % scales.len()];
            let at = Vec3::new(
                centre.x + column as f32 * 1.8,
                floor + 1.4 + layer as f32 * 1.7,
                centre.z + if column % 2 == 0 { 0.45 } else { -0.45 } - 0.4,
            );
            let turn = Quat::from_rotation_y(random() * core::f32::consts::TAU)
                * Quat::from_rotation_x((random() - 0.5) * 0.9);
            nodes.push(NodeData {
                name: format!("{} {k}", shapes[shape].0),
                translation: at.to_array(),
                rotation: turn.to_array(),
                scale: scale.to_array(),
                mesh: Some(shape),
                children: Vec::new(),
                skin: None,
                physics: None,
            });
        }
    }
    let props = ImportedScene {
        meshes: shapes
            .iter()
            .map(|(name, solid, _)| MeshData {
                name: (*name).to_owned(),
                primitives: vec![solid.primitive()],
            })
            .collect(),
        roots: (0..nodes.len()).collect(),
        nodes,
        collider_rules: ColliderRules {
            bodies: DefaultBodies::Dynamic,
            ..ColliderRules::default()
        },
        ..ImportedScene::default()
    };
    let jobs = ThreadPool::builder().build().unwrap();
    let cook = Instant::now();
    let ids: Vec<(u32, u32)> = (0..props.nodes.len() as u32).map(|i| (i, 1)).collect();
    let (colliders, bodies, stats) =
        cook_colliders(&props, &ids, &props.collider_rules, None, Some(&jobs));
    println!(
        "props: {} bodies, {} meshes decomposed in {:.2} s: {}",
        bodies.len(),
        stats.decomposed,
        cook.elapsed().as_secs_f64(),
        colliders
            .iter()
            .map(|c| c.collider.shapes.len().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    // Their meshes, materials and entities (one per node: what its body moves).
    let entities: Vec<Entity> = {
        let setup = player.setup();
        let meshes: Vec<_> = shapes
            .iter()
            .map(|(_, solid, _)| setup.geometry.add_mesh(&solid.render().as_data()).unwrap())
            .collect();
        let materials: Vec<_> = shapes
            .iter()
            .map(|(_, _, color)| setup.materials.add(MaterialDesc::solid(*color, 0.0, 0.7)))
            .collect();
        props
            .nodes
            .iter()
            .map(|n| {
                let mesh = n.mesh.unwrap();
                let (t, r, s) = (
                    Vec3::from(n.translation),
                    Quat::from_array(n.rotation),
                    Vec3::from(n.scale),
                );
                setup.world.spawn((
                    Transform::from_translation(t)
                        .with_rotation(r)
                        .with_scale(s),
                    GlobalTransform(Affine3A::from_scale_rotation_translation(s, r, t)),
                    MeshRenderer::new(meshes[mesh], materials[mesh]),
                ))
            })
            .collect()
    };
    let report = physics
        .add(&colliders, &bodies, player.world_mut(), &entities)
        .unwrap();
    println!("props placed: {report:?}");

    // The camera at the courtyard's end, above its floor, looking down it; the sun from
    // above.
    let eye = Vec3::new(centre.x - 7.3, floor + 3.4, centre.z);
    let target = Vec3::new(centre.x + 1.0, floor + 0.2, centre.z);
    let camera = player.camera(65f32.to_radians(), 0.05, size.length() * 2.0);
    player.world_mut().spawn((
        Transform::from_translation(eye).with_rotation(look_rotation(eye, target, Vec3::Y)),
        camera,
    ));
    player.world_mut().spawn((
        GlobalTransform(Affine3A::from_rotation_translation(
            Quat::from_rotation_arc(Vec3::NEG_Z, Vec3::new(-0.3, -1.0, -0.25).normalize()),
            Vec3::ZERO,
        )),
        Sun {
            color: Vec3::splat(3.0),
            ambient: Vec3::splat(0.1),
        },
    ));
    player.set_frame_time(Some(1.0 / 60.0));
    let mut game = Drop {
        physics,
        running: false,
    };
    // The scene's textures stream in before the props fall.
    for _ in 0..600 {
        if player.scene_settled() {
            break;
        }
        player.frame(&mut game).unwrap();
    }
    game.running = true;
    println!("ready after {:.1} s", start.elapsed().as_secs_f64());
    let frames = (seconds * 60.0) as usize;
    let mut written = 0;
    for f in 0..frames {
        if f % every == 0 {
            if let Some(rgba) = player.capture(&mut game).unwrap() {
                let png = encode_png(width, height, PngFormat::Rgba, &rgba).unwrap();
                std::fs::write(out.join(format!("frame_{written:04}.png")), png).unwrap();
                written += 1;
            }
        } else {
            player.frame(&mut game).unwrap();
        }
    }
    println!(
        "{written} frames in {} after {:.1} s",
        out.display(),
        start.elapsed().as_secs_f64()
    );
}
