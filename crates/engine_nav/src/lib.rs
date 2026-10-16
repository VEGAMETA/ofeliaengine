//! Navigation (ADR 0103): where agents can walk, and how they get there.
//!
//! - **[`bake`]** makes a navigation mesh from level geometry (a triangle soup) for an
//!   agent of a height, radius, step and slope ([`BakeSettings`]): tiles baked in parallel
//!   by Recast. The result ([`NavMeshData`]) is Detour's tile blobs, what a pack keeps.
//! - **[`NavMesh`]** loads the tiles and answers queries with Detour, without allocating:
//!   - [`NavMesh::nearest`]: the nearest walkable point;
//!   - [`NavMesh::path`]: the corners of a path (A* over polygons, then string pulled);
//!   - [`NavMesh::raycast`]: how far a straight walk gets.
//!
//! Recast and Detour are vendored (`third_party/recastnavigation`, Zlib) behind
//! `csrc/shim.cpp`. Y is up, as in the engine.

#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;
use engine_core::jobs::ThreadPool;
use engine_core::math::Vec3;
use std::sync::OnceLock;

mod ffi {
    use core::ffi::c_void;

    #[repr(C)]
    pub(crate) struct Bake {
        pub(crate) cell_size: f32,
        pub(crate) cell_height: f32,
        pub(crate) agent_height: f32,
        pub(crate) agent_radius: f32,
        pub(crate) agent_max_climb: f32,
        pub(crate) agent_max_slope: f32,
        pub(crate) region_min_size: i32,
        pub(crate) region_merge_size: i32,
        pub(crate) edge_max_len: f32,
        pub(crate) edge_max_error: f32,
        pub(crate) verts_per_poly: i32,
        pub(crate) detail_sample_dist: f32,
        pub(crate) detail_sample_max_error: f32,
        pub(crate) tile_size: i32,
    }

    #[repr(C)]
    pub(crate) struct EngineNavMesh {
        _private: [u8; 0],
    }

    unsafe extern "C" {
        pub(crate) fn engine_nav_bake_tile(
            verts: *const f32,
            nverts: i32,
            tris: *const i32,
            ntris: i32,
            origin: *const f32,
            min_y: f32,
            max_y: f32,
            tx: i32,
            ty: i32,
            settings: *const Bake,
            out_size: *mut i32,
            status: *mut i32,
        ) -> *mut u8;
        pub(crate) fn engine_nav_free(data: *mut c_void);
        pub(crate) fn engine_nav_create(
            origin: *const f32,
            tile_width: f32,
            max_tiles: i32,
            max_polys: i32,
            max_nodes: i32,
        ) -> *mut EngineNavMesh;
        pub(crate) fn engine_nav_destroy(nav: *mut EngineNavMesh);
        pub(crate) fn engine_nav_add_tile(
            nav: *mut EngineNavMesh,
            bytes: *const u8,
            size: i32,
        ) -> u32;
        pub(crate) fn engine_nav_nearest(
            nav: *const EngineNavMesh,
            point: *const f32,
            extent: *const f32,
            out: *mut f32,
        ) -> i32;
        pub(crate) fn engine_nav_path(
            nav: *mut EngineNavMesh,
            start: *const f32,
            end: *const f32,
            extent: *const f32,
            polys: *mut u64,
            max_polys: i32,
            out: *mut f32,
            max_points: i32,
            count: *mut i32,
        ) -> i32;
        pub(crate) fn engine_nav_raycast(
            nav: *mut EngineNavMesh,
            start: *const f32,
            end: *const f32,
            extent: *const f32,
        ) -> f32;
    }
}

/// How a navigation mesh is baked: the agent it is for, and how finely.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BakeSettings {
    /// Voxel size on the ground (m): a third or half of the agent's radius keeps narrow
    /// passages open.
    pub cell_size: f32,
    /// Voxel height (m): half of `agent_max_climb` or less.
    pub cell_height: f32,
    /// The agent's height (m): lower ceilings are not walkable.
    pub agent_height: f32,
    /// The agent's radius (m): the mesh keeps this far from walls.
    pub agent_radius: f32,
    /// The highest step it climbs (m).
    pub agent_max_climb: f32,
    /// The steepest slope it walks (degrees).
    pub agent_max_slope: f32,
    /// A tile's side in cells: tiles bake in parallel, and a change rebakes its own only.
    pub tile_cells: u32,
    /// Walkable islands smaller than this many cells across are dropped.
    pub region_min_cells: u32,
    /// Regions smaller than this many cells across are merged into their neighbours.
    pub region_merge_cells: u32,
    /// The longest polygon edge along a wall (m).
    pub edge_max_length: f32,
    /// How far a simplified wall may stray from the voxels (cells).
    pub edge_max_error: f32,
    /// The height detail's sample spacing (cells).
    pub detail_sample_distance: f32,
    /// How far the height detail may stray from the surface (cells).
    pub detail_sample_max_error: f32,
}

impl Default for BakeSettings {
    /// A human-sized agent: 1.8 m tall, 0.4 m wide-radius, 0.4 m steps, 45° slopes; cells of
    /// 0.15 × 0.1 m in tiles of 12 m.
    fn default() -> Self {
        Self {
            cell_size: 0.15,
            cell_height: 0.1,
            agent_height: 1.8,
            agent_radius: 0.4,
            agent_max_climb: 0.4,
            agent_max_slope: 45.0,
            tile_cells: 80,
            region_min_cells: 8,
            region_merge_cells: 20,
            edge_max_length: 12.0,
            edge_max_error: 1.3,
            detail_sample_distance: 6.0,
            detail_sample_max_error: 1.0,
        }
    }
}

impl BakeSettings {
    fn ffi(&self) -> ffi::Bake {
        ffi::Bake {
            cell_size: self.cell_size,
            cell_height: self.cell_height,
            agent_height: self.agent_height,
            agent_radius: self.agent_radius,
            agent_max_climb: self.agent_max_climb,
            agent_max_slope: self.agent_max_slope,
            region_min_size: self.region_min_cells as i32,
            region_merge_size: self.region_merge_cells as i32,
            edge_max_len: self.edge_max_length,
            edge_max_error: self.edge_max_error,
            verts_per_poly: 6,
            detail_sample_dist: self.detail_sample_distance,
            detail_sample_max_error: self.detail_sample_max_error,
            tile_size: self.tile_cells as i32,
        }
    }

    /// A tile's side in metres.
    pub fn tile_width(&self) -> f32 {
        self.tile_cells as f32 * self.cell_size
    }
}

/// One tile of a baked mesh: its place in the grid and Detour's blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavTile {
    /// Its column (along X).
    pub x: i32,
    /// Its row (along Z).
    pub y: i32,
    /// Detour's tile data.
    pub bytes: Vec<u8>,
}

/// A baked navigation mesh: tiles on a grid starting at `origin`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NavMeshData {
    /// The grid's corner (its lowest X and Z; Y unused).
    pub origin: [f32; 3],
    /// A tile's side (m).
    pub tile_width: f32,
    /// The tiles with something walkable in them.
    pub tiles: Vec<NavTile>,
}

/// Why a bake or a load failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavError {
    /// The settings cannot bake (a size of zero, a tile too large).
    Settings(&'static str),
    /// Recast ran out of memory, or a step of its failed, on a tile.
    Bake {
        /// The tile's column and row.
        tile: (i32, i32),
    },
    /// Detour refused a tile (its status code).
    Tile(u32),
    /// Detour could not make the mesh or its query.
    Create,
}

impl core::fmt::Display for NavError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Settings(why) => write!(f, "navigation settings: {why}"),
            Self::Bake { tile } => write!(f, "baking navigation tile {tile:?} failed"),
            Self::Tile(status) => {
                write!(f, "Detour refused a navigation tile (status {status:#x})")
            }
            Self::Create => f.write_str("Detour could not make the navigation mesh"),
        }
    }
}

impl std::error::Error for NavError {}

/// Bakes `triangles` (indices into `vertices`, world space, counter-clockwise seen from
/// outside: only faces turned up can be walked on) into a navigation mesh for `settings`'
/// agent; the tiles in parallel on `jobs` when given.
pub fn bake(
    vertices: &[Vec3],
    triangles: &[[u32; 3]],
    settings: &BakeSettings,
    jobs: Option<&ThreadPool>,
) -> Result<NavMeshData, NavError> {
    engine_profiler::zone!("nav::bake");
    if settings.cell_size <= 0.0 || settings.cell_height <= 0.0 || settings.tile_cells == 0 {
        return Err(NavError::Settings(
            "cell size, cell height and tile size must be positive",
        ));
    }
    if settings.tile_cells > 4096 {
        return Err(NavError::Settings("tiles of at most 4096 cells across"));
    }
    if vertices.is_empty() || triangles.is_empty() {
        return Ok(NavMeshData {
            tile_width: settings.tile_width(),
            ..NavMeshData::default()
        });
    }
    let (lo, hi) = vertices.iter().fold(
        (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)),
        |(lo, hi), &v| (lo.min(v), hi.max(v)),
    );
    let tile = settings.tile_width();
    let columns = (((hi.x - lo.x) / tile).ceil() as i32).max(1);
    let rows = (((hi.z - lo.z) / tile).ceil() as i32).max(1);
    // Each tile also rasterizes its border (the agent's radius and 3 cells), so its edges
    // match its neighbours'.
    let border = ((settings.agent_radius / settings.cell_size).ceil() + 3.0) * settings.cell_size;
    let mut per_tile: Vec<Vec<i32>> = vec![Vec::new(); (columns * rows) as usize];
    for (t, tri) in triangles.iter().enumerate() {
        let [a, b, c] = tri.map(|i| vertices.get(i as usize).copied().unwrap_or(Vec3::ZERO));
        let (min, max) = (a.min(b).min(c), a.max(b).max(c));
        let x0 = (((min.x - border - lo.x) / tile).floor() as i32).clamp(0, columns - 1);
        let x1 = (((max.x + border - lo.x) / tile).floor() as i32).clamp(0, columns - 1);
        let z0 = (((min.z - border - lo.z) / tile).floor() as i32).clamp(0, rows - 1);
        let z1 = (((max.z + border - lo.z) / tile).floor() as i32).clamp(0, rows - 1);
        for z in z0..=z1 {
            for x in x0..=x1 {
                per_tile[(z * columns + x) as usize].push(t as i32);
            }
        }
    }
    let flat: Vec<f32> = vertices.iter().flat_map(|v| v.to_array()).collect();
    let origin = [lo.x, lo.y, lo.z];
    let (min_y, max_y) = (lo.y - 1.0, hi.y + settings.agent_height + 1.0);
    let ffi_settings = settings.ffi();
    let results: Vec<OnceLock<Result<Option<NavTile>, NavError>>> =
        per_tile.iter().map(|_| OnceLock::new()).collect();
    let bake_one = |k: usize| {
        let (x, y) = (k as i32 % columns, k as i32 / columns);
        let list = &per_tile[k];
        let result = if list.is_empty() {
            Ok(None)
        } else {
            let tris: Vec<i32> = list
                .iter()
                .flat_map(|&t| triangles[t as usize].map(|i| i as i32))
                .collect();
            let (mut size, mut status) = (0i32, 0i32);
            // SAFETY: the arrays are alive for the call and sized as passed; the blob the
            // shim returns is copied, then freed by the shim's allocator.
            unsafe {
                let blob = ffi::engine_nav_bake_tile(
                    flat.as_ptr(),
                    vertices.len() as i32,
                    tris.as_ptr(),
                    list.len() as i32,
                    origin.as_ptr(),
                    min_y,
                    max_y,
                    x,
                    y,
                    &ffi_settings,
                    &mut size,
                    &mut status,
                );
                if blob.is_null() {
                    if status == 0 {
                        Ok(None)
                    } else {
                        Err(NavError::Bake { tile: (x, y) })
                    }
                } else {
                    let bytes = core::slice::from_raw_parts(blob, size as usize).to_vec();
                    ffi::engine_nav_free(blob.cast::<c_void>());
                    Ok(Some(NavTile { x, y, bytes }))
                }
            }
        };
        let _ = results[k].set(result);
    };
    match jobs {
        Some(jobs) => jobs.parallel_for(per_tile.len(), 1, |_, range| {
            for k in range {
                bake_one(k);
            }
        }),
        None => (0..per_tile.len()).for_each(bake_one),
    }
    let mut tiles = Vec::new();
    for r in results {
        if let Some(tile) = r.into_inner().unwrap_or(Ok(None))? {
            tiles.push(tile);
        }
    }
    Ok(NavMeshData {
        origin,
        tile_width: tile,
        tiles,
    })
}

/// How a path came out ([`NavMesh::path`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStatus {
    /// To the target.
    Complete,
    /// To the reachable point nearest the target (it is on another island, or beyond the
    /// search's limit).
    Partial,
    /// None: the start or the target is not near the mesh.
    None,
}

/// What a straight walk along the mesh met ([`NavMesh::raycast`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Raycast {
    /// It reached the end.
    Reached,
    /// A wall, this fraction of the way.
    Blocked(f32),
    /// The start is not near the mesh.
    OffMesh,
}

/// Most polygons a path search keeps.
const MAX_POLYS: usize = 1024;
/// Most corners a path has.
const MAX_CORNERS: usize = 256;

/// A navigation mesh and a query over it (Detour). Paths and raycasts take `&mut self`
/// (they use the query's search state); [`NavMesh::nearest`] takes `&self`.
pub struct NavMesh {
    raw: *mut ffi::EngineNavMesh,
    polys: Box<[u64; MAX_POLYS]>,
    corners: Box<[f32; MAX_CORNERS * 3]>,
    tiles: usize,
}

// SAFETY: the Detour objects are owned by this value alone and used through `&mut self`
// (or `&self` for the mesh's read-only queries), never shared between threads.
unsafe impl Send for NavMesh {}
// SAFETY: the `&self` methods call only Detour's const queries (`findNearestPoly`), which
// read the mesh and touch none of the query's search state (its node pools and open list,
// used by the `&mut self` ones); the mesh is not changed after loading.
unsafe impl Sync for NavMesh {}

impl core::fmt::Debug for NavMesh {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NavMesh")
            .field("tiles", &self.tiles)
            .finish()
    }
}

impl NavMesh {
    /// Loads `data`'s tiles.
    pub fn new(data: &NavMeshData) -> Result<Self, NavError> {
        engine_profiler::zone!("nav::NavMesh::new");
        let max_tiles = data.tiles.len().max(1).next_power_of_two() as i32;
        // SAFETY: plain values; the result is checked.
        let raw = unsafe {
            ffi::engine_nav_create(
                data.origin.as_ptr(),
                data.tile_width,
                max_tiles,
                0xFFFF,
                4096,
            )
        };
        if raw.is_null() {
            return Err(NavError::Create);
        }
        let mesh = Self {
            raw,
            polys: Box::new([0; MAX_POLYS]),
            corners: Box::new([0.0; MAX_CORNERS * 3]),
            tiles: data.tiles.len(),
        };
        for tile in &data.tiles {
            // SAFETY: the shim copies the bytes into Detour's own allocation.
            let status = unsafe {
                ffi::engine_nav_add_tile(raw, tile.bytes.as_ptr(), tile.bytes.len() as i32)
            };
            if status & (1 << 31) != 0 {
                return Err(NavError::Tile(status));
            }
        }
        Ok(mesh)
    }

    /// Tiles loaded.
    pub fn tiles(&self) -> usize {
        self.tiles
    }

    /// The walkable point nearest `point`, within `extent` of it on each axis.
    pub fn nearest(&self, point: Vec3, extent: Vec3) -> Option<Vec3> {
        let mut out = [0.0f32; 3];
        // SAFETY: valid pointers to three floats each; the mesh is alive.
        let found = unsafe {
            ffi::engine_nav_nearest(
                self.raw,
                point.to_array().as_ptr(),
                extent.to_array().as_ptr(),
                out.as_mut_ptr(),
            )
        };
        (found != 0).then(|| Vec3::from(out))
    }

    /// A path from `from` to `to` (each taken to the mesh within `extent`): its corners into
    /// `out` (cleared first; start, the turns, the end), at most 256. Allocates nothing once
    /// `out` has room.
    pub fn path(&mut self, from: Vec3, to: Vec3, extent: Vec3, out: &mut Vec<Vec3>) -> PathStatus {
        engine_profiler::zone!("nav::path");
        out.clear();
        let mut count = 0i32;
        // SAFETY: the buffers are ours and sized as passed; the mesh is alive.
        let result = unsafe {
            ffi::engine_nav_path(
                self.raw,
                from.to_array().as_ptr(),
                to.to_array().as_ptr(),
                extent.to_array().as_ptr(),
                self.polys.as_mut_ptr(),
                MAX_POLYS as i32,
                self.corners.as_mut_ptr(),
                MAX_CORNERS as i32,
                &mut count,
            )
        };
        for c in self.corners.chunks_exact(3).take(count.max(0) as usize) {
            out.push(Vec3::new(c[0], c[1], c[2]));
        }
        match result {
            1 => PathStatus::Complete,
            2 => PathStatus::Partial,
            _ => PathStatus::None,
        }
    }

    /// How far a straight walk from `from` (taken to the mesh within `extent`) towards `to`
    /// gets along the mesh.
    pub fn raycast(&mut self, from: Vec3, to: Vec3, extent: Vec3) -> Raycast {
        // SAFETY: valid pointers to three floats each; the mesh is alive.
        let t = unsafe {
            ffi::engine_nav_raycast(
                self.raw,
                from.to_array().as_ptr(),
                to.to_array().as_ptr(),
                extent.to_array().as_ptr(),
            )
        };
        if t < 0.0 {
            Raycast::OffMesh
        } else if t >= 1.0 {
            Raycast::Reached
        } else {
            Raycast::Blocked(t)
        }
    }
}

impl Drop for NavMesh {
    fn drop(&mut self) {
        // SAFETY: made by `engine_nav_create`, destroyed once.
        unsafe { ffi::engine_nav_destroy(self.raw) };
    }
}
