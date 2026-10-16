//! A scene's navigation mesh in the cook and the pack (ADR 0103): baked from its level
//! geometry (its static bodies' colliders, as placed) for the cook's agent, kept as
//! Detour's tile blobs in one pack entry (version 8), read only when a game navigates.

use crate::colliders::{CookedBody, CookedCollider, Motion};
use crate::pack::{EntryData, EntryKind, Pack, PackError, invalid};
use engine_collision::{Hull, Shape};
use engine_core::jobs::ThreadPool;
use engine_core::math::{Mat4, Vec3};
use engine_nav::{NavMeshData, NavTile};

/// How far a round shape's faces may lie inside it (scene units): well under a cell.
const ROUND_TOLERANCE: f32 = 0.02;

/// The level's triangles in the world: every static body's shapes as it places them, its
/// meshes as they are and its boxes, round shapes and hulls as their hulls' faces. A
/// mirroring placement keeps their faces facing out (walkable faces face up).
pub fn level_triangles(
    colliders: &[CookedCollider],
    bodies: &[CookedBody],
) -> (Vec<Vec3>, Vec<[u32; 3]>) {
    let (mut vertices, mut triangles) = (Vec::new(), Vec::new());
    for body in bodies
        .iter()
        .filter(|b| b.motion == Motion::Static && !b.sensor)
    {
        let Some(collider) = colliders.get(body.collider as usize) else {
            continue;
        };
        let to_world = Mat4::from_cols_array(&body.world_from_object);
        let mirrored = to_world.determinant() < 0.0;
        let mut add = |points: &[Vec3], faces: &[[u32; 3]]| {
            let base = vertices.len() as u32;
            vertices.extend(points.iter().map(|&p| to_world.transform_point3(p)));
            triangles.extend(faces.iter().map(|&[a, b, c]| {
                let t = if mirrored { [a, c, b] } else { [a, b, c] };
                t.map(|i| i + base)
            }));
        };
        for s in &collider.collider.shapes {
            match &s.shape {
                Shape::Mesh {
                    vertices: v,
                    triangles: t,
                    ..
                } => add(v, t),
                shape => {
                    // A flat hull (no volume) has no faces to stand on.
                    if let Ok(hull) = Hull::new(&shape.hull_points(ROUND_TOLERANCE)) {
                        add(&hull.points, &hull.triangles);
                    }
                }
            }
        }
    }
    (vertices, triangles)
}

/// The navigation entry: `meta = [tiles]`; the grid's origin and tile width (four `f32`),
/// then per tile its column, row and byte count (three `u32`), its bytes, and padding to
/// four bytes.
pub(crate) fn nav_entry(data: &NavMeshData) -> ([u32; 7], Vec<u8>) {
    let mut bytes = Vec::new();
    for v in [
        data.origin[0],
        data.origin[1],
        data.origin[2],
        data.tile_width,
    ] {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    for tile in &data.tiles {
        bytes.extend_from_slice(&tile.x.to_le_bytes());
        bytes.extend_from_slice(&tile.y.to_le_bytes());
        bytes.extend_from_slice(&(tile.bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&tile.bytes);
        bytes.resize(bytes.len().next_multiple_of(4), 0);
    }
    ([data.tiles.len() as u32, 0, 0, 0, 0, 0, 0], bytes)
}

/// The navigation mesh of a navigation entry (see [`nav_entry`]).
pub fn read_nav_entry(meta: [u32; 7], data: &[u8]) -> Result<NavMeshData, PackError> {
    let truncated = || invalid("navigation entry truncated");
    let word = |at: usize| -> Result<[u8; 4], PackError> {
        data.get(at..at + 4)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(truncated)
    };
    let float = |at: usize| word(at).map(f32::from_le_bytes);
    let origin = [float(0)?, float(4)?, float(8)?];
    let tile_width = float(12)?;
    let mut at = 16;
    let mut tiles = Vec::with_capacity((meta[0] as usize).min(data.len() / 12));
    for _ in 0..meta[0] {
        let x = i32::from_le_bytes(word(at)?);
        let y = i32::from_le_bytes(word(at + 4)?);
        let len = u32::from_le_bytes(word(at + 8)?) as usize;
        let bytes = data
            .get(at + 12..at + 12 + len)
            .ok_or_else(truncated)?
            .to_vec();
        at = (at + 12 + len).next_multiple_of(4);
        tiles.push(NavTile { x, y, bytes });
    }
    Ok(NavMeshData {
        origin,
        tile_width,
        tiles,
    })
}

/// The navigation mesh of an open pack, or `None` when it has none (cooked without, or
/// before version 8).
pub fn load_navigation(pack: &Pack, jobs: &ThreadPool) -> Result<Option<NavMeshData>, PackError> {
    engine_profiler::zone!("assets::load_navigation");
    let indices: Vec<u32> = (0..pack.entries().len() as u32)
        .filter(|&i| pack.entries()[i as usize].kind() == EntryKind::NavMesh)
        .collect();
    if indices.is_empty() {
        return Ok(None);
    }
    let found = std::sync::Mutex::new(None);
    pack.load(&indices, jobs, 1, |_, e, data: EntryData| {
        let nav = read_nav_entry(e.meta, &data)?;
        if let Ok(mut slot) = found.lock()
            && slot.replace(nav).is_some()
        {
            return Err(invalid("more than one navigation entry"));
        }
        Ok(())
    })?;
    Ok(found.into_inner().ok().flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_navigation_entry_round_trips() {
        let data = NavMeshData {
            origin: [-1.0, 0.5, 2.0],
            tile_width: 12.0,
            tiles: vec![
                NavTile {
                    x: 0,
                    y: 1,
                    bytes: vec![1, 2, 3],
                },
                NavTile {
                    x: -2,
                    y: 3,
                    bytes: vec![9; 9],
                },
            ],
        };
        let (meta, bytes) = nav_entry(&data);
        assert_eq!(read_nav_entry(meta, &bytes).unwrap(), data);
        assert!(read_nav_entry(meta, &bytes[..bytes.len() - 8]).is_err());
    }

    #[test]
    fn static_primitives_count_and_mirrors_keep_faces_facing_out() {
        use engine_collision::{Collider, ColliderShape, Origin};
        use engine_core::math::Quat;
        let collider = |shape| CookedCollider {
            collider: Collider {
                shapes: vec![ColliderShape {
                    id: 0,
                    shape,
                    surface: 0,
                }],
                origin: Origin::Generated,
            },
            baked: None,
        };
        let colliders = vec![
            // One triangle facing up.
            collider(Shape::Mesh {
                vertices: vec![Vec3::ZERO, Vec3::Z, Vec3::X],
                triangles: vec![[0, 1, 2]],
                surfaces: Vec::new(),
            }),
            collider(Shape::Box {
                center: Vec3::ZERO,
                rotation: Quat::IDENTITY,
                half_extents: Vec3::ONE,
            }),
        ];
        let bodies = vec![
            // Mirrored along x.
            CookedBody {
                collider: 0,
                world_from_object: Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0)).to_cols_array(),
                ..CookedBody::default()
            },
            CookedBody {
                collider: 1,
                ..CookedBody::default()
            },
            // Moving bodies are not the level.
            CookedBody {
                collider: 1,
                motion: Motion::Dynamic,
                ..CookedBody::default()
            },
        ];
        let (vertices, triangles) = level_triangles(&colliders, &bodies);
        assert_eq!(triangles.len(), 1 + 12);
        let normal = |t: [u32; 3]| {
            let [a, b, c] = t.map(|i| vertices[i as usize]);
            (b - a).cross(c - a).normalize()
        };
        assert!(normal(triangles[0]).y > 0.99);
        // The box's faces face out: its top's two triangles up, its bottom's down.
        let facing = |y: f32| {
            triangles[1..]
                .iter()
                .filter(|&&t| (normal(t).y - y).abs() < 0.01)
                .count()
        };
        assert_eq!((facing(1.0), facing(-1.0)), (2, 2));
    }

    #[test]
    fn a_level_baked_from_its_bodies_survives_its_pack_as_version_8() {
        use crate::cook::{CookedScene, load_pack, open_scene, write_pack};
        use engine_collision::{Collider, ColliderShape, Origin};
        use engine_nav::{BakeSettings, NavMesh, PathStatus, bake};
        // A 20 m floor, a static body's mesh collider placed 1 m up; a sensor over it that
        // the bake leaves out.
        let floor = CookedCollider {
            collider: Collider {
                shapes: vec![ColliderShape {
                    id: 0,
                    shape: Shape::Mesh {
                        vertices: vec![
                            Vec3::new(-10.0, 0.0, -10.0),
                            Vec3::new(-10.0, 0.0, 10.0),
                            Vec3::new(10.0, 0.0, 10.0),
                            Vec3::new(10.0, 0.0, -10.0),
                        ],
                        triangles: vec![[0, 1, 2], [0, 2, 3]],
                        surfaces: vec![0, 0],
                    },
                    surface: 0,
                }],
                origin: Origin::Generated,
            },
            baked: None,
        };
        let up = |y: f32| Mat4::from_translation(Vec3::new(0.0, y, 0.0)).to_cols_array();
        let colliders = vec![floor];
        let bodies = vec![
            CookedBody {
                collider: 0,
                world_from_object: up(1.0),
                ..CookedBody::default()
            },
            CookedBody {
                collider: 0,
                world_from_object: up(2.0),
                sensor: true,
                ..CookedBody::default()
            },
        ];
        let (vertices, triangles) = level_triangles(&colliders, &bodies);
        assert_eq!(triangles.len(), 2);
        assert!(vertices.iter().all(|v| v.y == 1.0));
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let nav = bake(&vertices, &triangles, &BakeSettings::default(), Some(&jobs)).unwrap();
        assert!(!nav.tiles.is_empty());
        let scene = CookedScene {
            colliders,
            bodies,
            navigation: Some(nav),
            ..CookedScene::default()
        };
        let path = std::env::temp_dir().join(format!(
            "engine_assets_navigation_{}.pack",
            std::process::id()
        ));
        write_pack(&path, &scene, &jobs, 3).unwrap();
        let header = std::fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes([header[4], header[5], header[6], header[7]]),
            8
        );
        let (loaded, _) = load_pack(&path, &jobs, 4).unwrap();
        // A scene that is only drawn does not load it.
        let (pack, layout, _) = open_scene(&path, &jobs, 4).unwrap();
        assert!(layout.scene.navigation.is_none());
        assert_eq!(load_navigation(&pack, &jobs).unwrap(), scene.navigation);
        drop(pack);
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, scene);
        // It navigates: across the floor, at its height.
        let mut mesh = NavMesh::new(loaded.navigation.as_ref().unwrap()).unwrap();
        let mut corners = Vec::new();
        let extent = Vec3::new(1.0, 2.0, 1.0);
        let status = mesh.path(
            Vec3::new(-8.0, 1.0, 0.0),
            Vec3::new(8.0, 1.0, 0.0),
            extent,
            &mut corners,
        );
        assert_eq!(status, PathStatus::Complete);
        assert!(
            corners.iter().all(|p| (p.y - 1.0).abs() < 0.3),
            "{corners:?}"
        );
    }

    #[test]
    fn the_cook_bakes_a_level_unless_told_not_to() {
        use crate::cook::{CookSettings, cook_scene};
        use crate::gltf_import::{ImportedScene, MeshData, NodeData, Primitive};
        use engine_core::math::Quat;
        let floor = Primitive {
            positions: vec![
                [-10.0, 0.0, -10.0],
                [-10.0, 0.0, 10.0],
                [10.0, 0.0, 10.0],
                [10.0, 0.0, -10.0],
            ],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: Vec::new(),
            tangents: Vec::new(),
            indices: vec![0, 1, 2, 0, 2, 3],
            material: None,
            min: [-10.0, 0.0, -10.0],
            max: [10.0, 0.0, 10.0],
            joints: Vec::new(),
            weights: Vec::new(),
        };
        let scene = ImportedScene {
            meshes: vec![MeshData {
                name: "floor".into(),
                primitives: vec![floor],
            }],
            nodes: vec![NodeData {
                name: "floor".into(),
                translation: [0.0; 3],
                rotation: Quat::IDENTITY.to_array(),
                scale: [1.0; 3],
                mesh: Some(0),
                children: Vec::new(),
                skin: None,
                physics: None,
            }],
            roots: vec![0],
            ..ImportedScene::default()
        };
        let jobs = ThreadPool::builder().workers(2).build().unwrap();
        let cook =
            |settings: CookSettings| cook_scene(&scene, None, None, &jobs, &settings).unwrap();
        let (cooked, stats) = cook(CookSettings::default());
        let nav = cooked.navigation.expect("a navigation mesh");
        assert!(!nav.tiles.is_empty() && stats.navigation_seconds > 0.0);
        for settings in [
            CookSettings {
                navigation: None,
                ..CookSettings::default()
            },
            CookSettings {
                colliders: false,
                ..CookSettings::default()
            },
        ] {
            assert!(cook(settings).0.navigation.is_none());
        }
    }
}
