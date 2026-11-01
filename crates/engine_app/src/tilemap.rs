//! Tile maps (ADR 0125's E13-3): a layer of tiles on a grid, its cells kept in the scene
//! as text that merges in git (question 3: a).
//!
//! A map is cut into chunks of 32 × 32 cells. Each non-empty row of a chunk is one line,
//! its cells written through a palette of short keys with runs:
//!
//! ```text
//! TileMap.tile_set = asset("tiles/forest.tileset", 0b6c…)
//! TileMap.palette["a3kq"] = "k2:0,3"
//! TileMap.palette["x0d4"] = "k2:1,3 flip_x"
//! TileMap.cells["0,0"][14] = ". . a3kq*12 x0d4 . a3kq*16"
//! TileMap.cells["0,0"][15] = "a3kq*32"
//! ```
//!
//! - A palette key is the first four base-36 digits of a hash of what it names (a tile
//!   and its orientation), longer only when two names in one map share them: two
//!   branches that add the same tile write the same line.
//! - A chunk `"cx,cy"` holds the cells x in `32·cx .. 32·cx + 32`, y likewise; row `r` is
//!   y = `32·cy + r`. A row's cells run from the chunk's left; `.` is an empty cell, and
//!   a token repeated three or more times is written once with `*n`. Empty cells at a
//!   row's end, empty rows and empty chunks have no text.
//! - A cell's value is a tile of the map's tile set (`k2:0,3`) and its orientation:
//!   `flip_x`, `flip_y`, `transpose`, in that order, space-separated.

use std::collections::BTreeMap;

use engine_reflect::Reflect;
use engine_scene::AssetRef;

/// Cells a chunk is wide and tall.
pub const CHUNK: i32 = 32;

/// A layer of tiles on a grid (ADR 0125's E13-3), its cells kept as text in the scene.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct TileMap {
    /// The tile set its tiles come from (`.tileset`).
    pub tile_set: AssetRef,
    /// Short keys for what its cells hold (a tile and its orientation).
    pub palette: BTreeMap<String, String>,
    /// Its cells: by chunk (`"cx,cy"`), then by row in the chunk, a row's cells through
    /// the palette (see the module's notes).
    #[reflect(hidden)]
    pub cells: BTreeMap<String, BTreeMap<u32, String>>,
    /// Its colour, linear, multiplying every tile's.
    #[reflect(hint = "color")]
    pub color: [f32; 4],
    /// Its sorting layer, as a sprite's.
    pub layer: i16,
    /// Its place within its layer, as a sprite's.
    pub order: i16,
    /// Not drawn; the entity keeps its other components.
    pub hidden: bool,
}
engine_ecs::reflect_component!(TileMap);

impl Default for TileMap {
    fn default() -> Self {
        Self::new(AssetRef::NONE)
    }
}

/// The chunk holding cell (`x`, `y`) and the cell's column and row in it.
fn place(x: i32, y: i32) -> ((i32, i32), usize, u32) {
    (
        (x.div_euclid(CHUNK), y.div_euclid(CHUNK)),
        x.rem_euclid(CHUNK) as usize,
        y.rem_euclid(CHUNK) as u32,
    )
}

/// A chunk's key in [`TileMap::cells`].
pub fn chunk_key(cx: i32, cy: i32) -> String {
    format!("{cx},{cy}")
}

/// Chunk key `key` read back.
fn parse_chunk(key: &str) -> Option<(i32, i32)> {
    let (x, y) = key.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// Row `text`'s tokens, each cell's (a palette key, or `.` for an empty cell), runs
/// expanded; at most [`CHUNK`].
fn row_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split_ascii_whitespace()
        .flat_map(|token| {
            let (key, count) = match token.rsplit_once('*') {
                Some((key, n)) => (key, n.parse::<usize>().unwrap_or(1)),
                None => (token, 1),
            };
            core::iter::repeat_n(key, count.min(CHUNK as usize))
        })
        .take(CHUNK as usize)
}

/// A row's cells (`None` empty) written as a row's text: runs of three or more shortened,
/// empty cells at its end left out.
fn row_text(cells: &[Option<&str>]) -> String {
    let end = cells.iter().rposition(Option::is_some).map_or(0, |k| k + 1);
    let mut out = String::new();
    let mut k = 0;
    while k < end {
        let token = cells[k].unwrap_or(".");
        let mut n = 1;
        while k + n < end && cells[k + n].unwrap_or(".") == token {
            n += 1;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        if n >= 3 {
            out.push_str(token);
            out.push('*');
            out.push_str(&n.to_string());
        } else {
            out.push_str(token);
            for _ in 1..n {
                out.push(' ');
                out.push_str(token);
            }
        }
        k += n;
    }
    out
}

/// Base-36 digits of `v`, most significant first (13 for any 64-bit value).
fn base36(mut v: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = [b'0'; 13];
    for d in out.iter_mut().rev() {
        *d = DIGITS[(v % 36) as usize];
        v /= 36;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl TileMap {
    /// An empty map of tile set `tile_set`.
    pub fn new(tile_set: AssetRef) -> Self {
        Self {
            tile_set,
            palette: BTreeMap::new(),
            cells: BTreeMap::new(),
            color: [1.0; 4],
            layer: 0,
            order: 0,
            hidden: false,
        }
    }

    /// What cell (`x`, `y`) holds (a tile and its orientation, `k2:0,3 flip_x`), if
    /// anything.
    pub fn get(&self, x: i32, y: i32) -> Option<&str> {
        let ((cx, cy), column, row) = place(x, y);
        let text = self.cells.get(&chunk_key(cx, cy))?.get(&row)?;
        let key = row_tokens(text).nth(column)?;
        self.palette.get(key).map(String::as_str)
    }

    /// Cell (`x`, `y`) made to hold `value` (a tile and its orientation), or emptied for
    /// `None`; its row written again, a new palette key made when `value` has none.
    pub fn set(&mut self, x: i32, y: i32, value: Option<&str>) {
        let key = value.map(|v| self.key_for(v));
        let ((cx, cy), column, row) = place(x, y);
        let chunk_name = chunk_key(cx, cy);
        let chunk = self.cells.entry(chunk_name.clone()).or_default();
        let old = chunk.get(&row).map(String::as_str).unwrap_or("");
        let mut cells: [Option<&str>; CHUNK as usize] = [None; CHUNK as usize];
        for (k, token) in row_tokens(old).enumerate() {
            cells[k] = (token != ".").then_some(token);
        }
        cells[column] = key.as_deref();
        let text = row_text(&cells);
        if text.is_empty() {
            chunk.remove(&row);
        } else {
            chunk.insert(row, text);
        }
        if chunk.is_empty() {
            self.cells.remove(&chunk_name);
        }
    }

    /// The palette key for `value`: its own if it has one, else a new one from its hash
    /// (four digits, more only when they are taken).
    fn key_for(&mut self, value: &str) -> String {
        if let Some((key, _)) = self.palette.iter().find(|(_, v)| *v == value) {
            return key.clone();
        }
        let digits = base36(engine_core::hash::state_hash64(value.as_bytes()));
        let key = (4..=digits.len())
            .map(|n| &digits[..n])
            .find(|k| !self.palette.contains_key(*k))
            .map_or_else(|| digits.clone() + "_", str::to_owned);
        self.palette.insert(key.clone(), value.to_owned());
        key
    }

    /// Every non-empty cell with what it holds, chunk by chunk, row by row.
    pub fn for_each(&self, mut f: impl FnMut(i32, i32, &str)) {
        for (name, rows) in &self.cells {
            let Some((cx, cy)) = parse_chunk(name) else {
                continue;
            };
            for (&row, text) in rows {
                for (column, key) in row_tokens(text).enumerate() {
                    if let Some(value) = self.palette.get(key) {
                        f(cx * CHUNK + column as i32, cy * CHUNK + row as i32, value);
                    }
                }
            }
        }
    }

    /// Palette keys no cell uses taken away (after an edit that may have emptied some).
    pub fn tidy(&mut self) {
        let mut used = std::collections::BTreeSet::new();
        for rows in self.cells.values() {
            for text in rows.values() {
                for token in text.split_ascii_whitespace() {
                    used.insert(token.rsplit_once('*').map_or(token, |(k, _)| k));
                }
            }
        }
        self.palette.retain(|k, _| used.contains(k.as_str()));
    }
}

/// Cells of a chunk as the GPU reads them ([`engine_render2d::Canvas::draw_tiles`]).
type ChunkCells = [u32; (CHUNK * CHUNK) as usize];

/// A tile map as drawn (ADR 0125's E13-3b): its tiles in a list (a cell holds its tile's
/// place there plus one, and its orientation) and its chunks' cells.
#[derive(Default)]
struct Layer {
    tiles: Vec<String>,
    chunks: std::collections::HashMap<(i32, i32), Box<ChunkCells>>,
}

impl Layer {
    /// `map`'s cells made again.
    fn read(&mut self, map: &TileMap) {
        self.tiles.clear();
        self.chunks.clear();
        let Self { tiles, chunks } = self;
        map.for_each(|x, y, value| {
            let (tile, orientation) = parse_cell(value);
            let k = match tiles.iter().position(|t| t == tile) {
                Some(k) => k,
                None => {
                    tiles.push(tile.to_owned());
                    tiles.len() - 1
                }
            };
            let ((cx, cy), column, row) = place(x, y);
            let cells = chunks
                .entry((cx, cy))
                .or_insert_with(|| Box::new([0; (CHUNK * CHUNK) as usize]));
            cells[row as usize * CHUNK as usize + column] =
                (k as u32 + 1) | (u32::from(orientation) << 20);
        });
    }
}

type ChangedMaps =
    engine_ecs::QueryState<(engine_ecs::Entity, &'static TileMap), engine_ecs::Changed<TileMap>>;
type DrawnMaps = engine_ecs::QueryState<
    (
        engine_ecs::Entity,
        &'static TileMap,
        &'static engine_ecs::GlobalTransform,
    ),
    engine_ecs::Without<engine_runtime::Inactive>,
>;

/// The tile maps' runtime (ADR 0125's E13-3b): each map's cells read when it changes, and
/// its visible chunks recorded into the 2D canvas, a quad a chunk.
pub(crate) struct TileLayers {
    changed: ChangedMaps,
    drawn: DrawnMaps,
    layers: std::collections::HashMap<engine_ecs::Entity, Layer>,
    entries: Vec<engine_render2d::TileEntry>,
}

impl TileLayers {
    pub(crate) fn new(world: &mut engine_ecs::World) -> Self {
        Self {
            changed: engine_ecs::QueryState::new(world),
            drawn: engine_ecs::QueryState::new(world),
            layers: std::collections::HashMap::new(),
            entries: Vec::new(),
        }
    }

    /// Records the visible chunks of every shown map into `canvas`, its place in the draw
    /// order from `order` (layer, order, y); returns the chunks drawn. A map's cells are
    /// read again when it changed; its tile set and their images are asked of `assets`.
    pub(crate) fn record(
        &mut self,
        world: &engine_ecs::World,
        assets: &mut crate::assets::Assets,
        canvas: &mut engine_render2d::Canvas,
        order: impl Fn(i16, i16, f32) -> engine_render2d::Order,
        view: Option<crate::sprites::ViewRect>,
    ) -> u32 {
        engine_profiler::zone!("app::TileLayers::record");
        let Self {
            changed,
            drawn,
            layers,
            entries,
        } = self;
        changed.for_each_readonly(world, |(e, map)| {
            layers.entry(e).or_default().read(map);
        });
        let mut count = 0;
        drawn.for_each_readonly(world, |(e, map, global)| {
            if map.hidden || map.tile_set.is_none() {
                return;
            }
            let Some(layer) = layers.get(&e) else {
                return;
            };
            assets.ensure_tile_set(map.tile_set.file);
            let Some(set) = assets.tile_set(map.tile_set.file) else {
                return;
            };
            // Its tiles' images and regions this frame.
            entries.clear();
            entries.extend(layer.tiles.iter().map(|name| {
                let clear = engine_render2d::TileEntry {
                    texture: None,
                    sampler: engine_rhi::SamplerKind::NearestClamp,
                    uv: [0.0; 4],
                };
                let Some(tile) = engine_scene::TileName::parse(name) else {
                    return clear;
                };
                let (Some(&slot), Some(rect)) =
                    (set.slots.get(tile.source), set.file.tile_rect(tile))
                else {
                    return clear;
                };
                let Some(image) = assets.sprite_shown(slot) else {
                    return clear;
                };
                let [x, y, w, h] = rect.map(|v: u32| v as f32);
                let size = image.size.max(engine_core::math::Vec2::ONE);
                engine_render2d::TileEntry {
                    texture: Some(image.index),
                    sampler: image.sampler,
                    uv: [x / size.x, y / size.y, (x + w) / size.x, (y + h) / size.y],
                }
            }));
            let table = canvas.tile_table(entries);
            // A cell's size in metres, the map's scale applied (its rotation is not).
            let m = global.0.matrix3;
            let ppu = if set.file.pixels_per_unit > 0.0 {
                set.file.pixels_per_unit
            } else {
                16.0
            };
            let cell = engine_core::math::Vec2::new(
                set.file.tile[0] as f32 / ppu * m.x_axis.truncate().length(),
                set.file.tile[1] as f32 / ppu * m.y_axis.truncate().length(),
            );
            let origin = global.0.translation.truncate();
            let chunk = cell * CHUNK as f32;
            let place = order(map.layer, map.order, origin.y);
            let color = engine_core::math::Vec4::from_array(map.color);
            for (&(cx, cy), cells) in &layer.chunks {
                let min = origin + chunk * engine_core::math::Vec2::new(cx as f32, cy as f32);
                let max = min + chunk;
                if let Some(view) = view
                    && (max.cmplt(view.min).any() || min.cmpgt(view.max).any())
                {
                    continue;
                }
                let quad = engine_render2d::Quad::rect((min + max) * 0.5, chunk)
                    .color(color)
                    .order(place);
                canvas.draw_tiles(&quad, table, cells);
                count += 1;
            }
        });
        count
    }
}

/// A cell's value read: its tile and orientation (bits: 1 flip x, 2 flip y, 4 transpose).
pub fn parse_cell(value: &str) -> (&str, u8) {
    let mut words = value.split_ascii_whitespace();
    let tile = words.next().unwrap_or("");
    let mut orientation = 0;
    for word in words {
        orientation |= match word {
            "flip_x" => 1,
            "flip_y" => 2,
            "transpose" => 4,
            _ => 0,
        };
    }
    (tile, orientation)
}

/// A cell's value written: `tile` and the words of `orientation` (see [`parse_cell`]).
pub fn cell_value(tile: &str, orientation: u8) -> String {
    let mut out = tile.to_owned();
    for (bit, word) in [(1, " flip_x"), (2, " flip_y"), (4, " transpose")] {
        if orientation & bit != 0 {
            out.push_str(word);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cells set and read across chunks and negative places; rows written with runs and
    /// without trailing empties; emptied rows and chunks gone.
    #[test]
    fn cells_are_rows_of_palette_keys_with_runs() {
        let mut map = TileMap::default();
        for x in 2..14 {
            map.set(x, 14, Some("k2:0,3"));
        }
        map.set(14, 14, Some("k2:1,3"));
        map.set(15, 14, Some("k2:1,3 flip_x"));
        map.set(-1, -1, Some("k2:0,3"));
        assert_eq!(map.get(5, 14), Some("k2:0,3"));
        assert_eq!(map.get(15, 14), Some("k2:1,3 flip_x"));
        assert_eq!(map.get(0, 14), None);
        assert_eq!(map.get(-1, -1), Some("k2:0,3"));
        assert_eq!(map.get(-1, 0), None);
        let grass = map.key_for("k2:0,3");
        let a = map.key_for("k2:1,3");
        let b = map.key_for("k2:1,3 flip_x");
        assert_eq!(grass.len(), 4);
        assert_eq!(map.cells["0,0"][&14], format!(". . {grass}*12 {a} {b}"));
        assert_eq!(map.cells["-1,-1"][&31], format!(".*31 {grass}"));
        // Emptying the lone cell takes its row and chunk away; tidy drops unused keys.
        map.set(-1, -1, None);
        assert!(!map.cells.contains_key("-1,-1"));
        map.set(14, 14, None);
        map.set(15, 14, None);
        map.tidy();
        assert_eq!(map.palette.len(), 1);
        let mut seen = 0;
        map.for_each(|x, y, v| {
            assert!((2..14).contains(&x) && y == 14 && v == "k2:0,3");
            seen += 1;
        });
        assert_eq!(seen, 12);
    }

    /// The same tile gets the same key in two maps (two branches merge), and a taken key
    /// is lengthened.
    #[test]
    fn palette_keys_come_from_what_they_name() {
        let mut one = TileMap::default();
        let mut two = TileMap::default();
        two.set(0, 0, Some("k2:5,5"));
        one.set(0, 0, Some("k2:0,3"));
        two.set(1, 0, Some("k2:0,3"));
        assert_eq!(one.key_for("k2:0,3"), two.key_for("k2:0,3"));
        let mut clash = TileMap::default();
        let key = clash.key_for("k2:0,3");
        clash.palette.insert(String::from("zz"), String::new());
        let taken = clash.palette.remove(&key).unwrap();
        clash
            .palette
            .insert(key.clone(), String::from("something else"));
        let longer = clash.key_for(&taken);
        assert!(longer.len() == 5 && longer.starts_with(&key), "{longer}");
    }

    /// A map in a scene: its lines are the module's notes' (`cells["0,0"][14] = …`), and
    /// loading them gives the map back.
    #[test]
    fn a_tile_map_round_trips_through_a_scene() {
        let registry = engine_scene::SceneRegistry::from_program();
        let mut world = engine_ecs::World::new();
        let mut map = TileMap::new(AssetRef::file(engine_scene::AssetId::new()));
        for x in 0..20 {
            map.set(x, 14, Some("k2:0,3"));
        }
        map.set(40, -3, Some("k2:1,3 flip_x"));
        world.spawn((engine_scene::SceneId::new(), map.clone()));
        let mut text = String::new();
        engine_scene::save(&mut world, &registry, &mut text).unwrap();
        let key = map.key_for("k2:0,3");
        assert!(
            text.contains(&format!("TileMap.cells[\"0,0\"][14] = \"{key}*20\"")),
            "{text}"
        );
        assert!(text.contains("TileMap.cells[\"1,-1\"][29]"), "{text}");
        let mut loaded = engine_ecs::World::new();
        engine_scene::load(&mut loaded, &registry, &text).unwrap();
        let mut q = engine_ecs::QueryState::<&TileMap>::new(&mut loaded);
        let maps: Vec<TileMap> = q.iter(&mut loaded).cloned().collect();
        assert_eq!(maps, [map]);
    }

    /// Orientation words read and written.
    #[test]
    fn cell_values_carry_their_orientation() {
        assert_eq!(parse_cell("k2:1,3 flip_x transpose"), ("k2:1,3", 5));
        assert_eq!(cell_value("k2:1,3", 3), "k2:1,3 flip_x flip_y");
        assert_eq!(parse_cell(&cell_value("a:0,0", 7)), ("a:0,0", 7));
    }

    /// A 256 × 256 layer as the scene's text: its size and how long writing and reading it
    /// take, fully painted with 16 tiles at random (the worst case) and as a level is
    /// (runs of ground). Run with `--nocapture` for the numbers.
    #[test]
    fn a_painted_layers_text_and_how_fast_it_reads() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut random = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for (name, level) in [("random", false), ("level", true)] {
            let mut map = TileMap::default();
            let start = std::time::Instant::now();
            for y in 0..256 {
                for x in 0..256 {
                    let tile = if level {
                        // Ground below 100, a few walls, sky above.
                        match y {
                            0..100 if random() % 16 == 0 => Some(format!("k2:{},1", random() % 4)),
                            0..100 => Some(String::from("k2:0,0")),
                            100 if x % 7 != 0 => Some(String::from("k2:1,0")),
                            _ => None,
                        }
                    } else {
                        Some(format!("k2:{},{}", random() % 4, random() % 4))
                    };
                    map.set(x, y, tile.as_deref());
                }
            }
            let painted = start.elapsed();
            let mut text = String::new();
            let start = std::time::Instant::now();
            engine_scene::write_value("tilemap", 1, None, &map, None, &mut text);
            let written = start.elapsed();
            let start = std::time::Instant::now();
            let mut read = TileMap::default();
            engine_scene::read_value("tilemap", 1, &text, &mut read).unwrap();
            let parsed = start.elapsed();
            let start = std::time::Instant::now();
            let mut cells = 0;
            read.for_each(|_, _, _| cells += 1);
            let decoded = start.elapsed();
            assert_eq!(read, map);
            println!(
                "{name}: {} KB, {} lines, {cells} cells; painted cell by cell {painted:?}, written {written:?}, read {parsed:?}, decoded {decoded:?}",
                text.len() / 1024,
                text.lines().count()
            );
        }
    }
}
