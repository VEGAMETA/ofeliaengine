//! A tile set (`.tileset`, ADR 0125's E13-3): the images its tiles are cut from, the
//! tiles' size, and what each tile adds (collision shapes, terrain marks, animation,
//! weight, data).
//!
//! The file is one value in the scenes' grammar. Entries are keyed by what they are: a
//! source by a short id, a terrain by its name, a tile by its source and its cell in the
//! source's grid (`k2:4,3`); a tile with nothing but defaults has no line:
//!
//! ```text
//! [tileset version=1 id=…]
//!
//! tile = [16, 16]
//! sources["k2"].image = asset("art/forest.png", 5f2c…)
//! terrains["grass"].color = [0.3, 0.7, 0.2, 1]
//! tiles["k2:0,3"].shapes.0 = [0, 0, 16, 0, 16, 16, 0, 16]
//! tiles["k2:7,1"].frames = ["k2:7,1", "k2:8,1", "k2:9,1"]
//! tiles["k2:2,5"].weight = 0.25
//! tiles["k2:2,5"].data["footstep"] = "grass"
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use engine_reflect::Reflect;

use crate::{AssetId, AssetRef, LoadError, ValueReport};

/// The shape of a tile set's grid (ADR 0125, question 8: a). Squares only for now; the
/// field is in the file from the start so maps made now stay valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum TileGrid {
    /// Square cells.
    #[default]
    Square = 0,
}

/// An image tiles are cut from: a grid of tile-sized cells, after a margin, with a gap
/// between cells.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct TileSource {
    /// The image.
    pub image: AssetRef,
    /// Pixels left clear at the image's left and top before the first cell.
    pub margin: [u32; 2],
    /// Pixels between neighbouring cells.
    pub gap: [u32; 2],
}

/// How a terrain picks its tiles (ADR 0125, question 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum TerrainMode {
    /// Each drawn tile chosen by the four painted cells at its corners (the dual grid).
    #[default]
    Corners = 0,
    /// A cell's tile chosen by which of its eight neighbours share its terrain.
    Blob = 1,
}

/// A terrain: a kind of ground painted on a terrain layer.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct Terrain {
    /// How it picks its tiles.
    pub mode: TerrainMode,
    /// Its colour in the editor's marks.
    #[reflect(hint = "color")]
    pub color: [f32; 4],
}

impl Default for Terrain {
    fn default() -> Self {
        Self {
            mode: TerrainMode::Corners,
            color: [1.0, 1.0, 1.0, 1.0],
        }
    }
}

/// What a tile adds to its pixels; every field at its default for a plain tile.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct TileDef {
    /// Collision polygons in the tile's pixels (x, y pairs from its top left), inside the
    /// tile and not overlapping.
    pub shapes: Vec<Vec<f32>>,
    /// Its surface: what physics and footsteps call the ground (empty: the default).
    pub surface: String,
    /// Bodies pass up through it and stand on it.
    pub one_way: bool,
    /// Its terrain marks: in corners mode its four corners (top left, top right, bottom
    /// right, bottom left), in blob mode its centre and eight neighbours.
    pub terrain: Vec<String>,
    /// Tiles shown in turn in its place (itself first), `frame_time` seconds each.
    pub frames: Vec<String>,
    /// Seconds each of `frames` shows.
    pub frame_time: f32,
    /// Shifted copies of its animation that cells pick by their place, so water does not
    /// ripple in step (1: all together).
    pub phases: u32,
    /// How often random painting picks it among its look-alikes (1 as often as others).
    pub weight: f32,
    /// What else the game reads from it (`footstep = grass`).
    pub data: BTreeMap<String, String>,
}

impl Default for TileDef {
    fn default() -> Self {
        Self {
            shapes: Vec::new(),
            surface: String::new(),
            one_way: false,
            terrain: Vec::new(),
            frames: Vec::new(),
            frame_time: 0.1,
            phases: 1,
            weight: 1.0,
            data: BTreeMap::new(),
        }
    }
}

/// A tile set: a `.tileset` file.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct TileSetFile {
    /// Its grid's shape.
    pub grid: TileGrid,
    /// A tile's size in pixels.
    pub tile: [u32; 2],
    /// How many pixels make a metre (a 16-pixel tile is a metre at 16).
    #[reflect(min = 0.001, max = 100000.0)]
    pub pixels_per_unit: f32,
    /// The images its tiles are cut from, by short ids.
    pub sources: BTreeMap<String, TileSource>,
    /// Its terrains, by name.
    pub terrains: BTreeMap<String, Terrain>,
    /// What its tiles add, by tile (`k2:4,3`); plain tiles have no entry.
    pub tiles: BTreeMap<String, TileDef>,
}

impl Default for TileSetFile {
    fn default() -> Self {
        Self {
            grid: TileGrid::Square,
            tile: [16, 16],
            pixels_per_unit: 16.0,
            sources: BTreeMap::new(),
            terrains: BTreeMap::new(),
            tiles: BTreeMap::new(),
        }
    }
}

/// A tile named by its source and cell (`k2:4,3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileName<'a> {
    /// The source's id.
    pub source: &'a str,
    /// Its column in the source's grid.
    pub column: u32,
    /// Its row in the source's grid.
    pub row: u32,
}

impl<'a> TileName<'a> {
    /// `text` (`k2:4,3`) read, if it names a tile.
    pub fn parse(text: &'a str) -> Option<Self> {
        let (source, cell) = text.split_once(':')?;
        let (column, row) = cell.split_once(',')?;
        Some(Self {
            source,
            column: column.trim().parse().ok()?,
            row: row.trim().parse().ok()?,
        })
    }
}

impl core::fmt::Display for TileName<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{},{}", self.source, self.column, self.row)
    }
}

/// The lines a default `T` writes (without the header), to leave out of a file.
fn default_lines<T: Reflect + Default>() -> Vec<String> {
    let mut all = String::new();
    crate::write_value("x", 1, None, &T::default(), None, &mut all);
    all.lines()
        .skip(1)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

impl TileSetFile {
    /// The kind in the file's header.
    pub const KIND: &'static str = "tileset";
    /// The format's version.
    pub const VERSION: u32 = 1;
    /// The files' extension.
    pub const EXTENSION: &'static str = "tileset";

    /// The file's text with its id; lines at their defaults left out (a reader takes them
    /// so), terrains always whole.
    pub fn to_text(&self, id: AssetId) -> String {
        let mut all = String::new();
        crate::write_value(Self::KIND, Self::VERSION, Some(id), self, None, &mut all);
        let file = default_lines::<Self>();
        let tile = default_lines::<TileDef>();
        let source = default_lines::<TileSource>();
        let mut out = String::with_capacity(all.len());
        for line in all.lines() {
            let entry = |prefix: &str, defaults: &[String]| {
                line.strip_prefix(prefix)
                    .and_then(|rest| rest.split_once("\"]."))
                    .is_some_and(|(_, field)| defaults.iter().any(|d| d == field))
            };
            let default = file.iter().any(|d| d == line)
                || entry("tiles[\"", &tile)
                || entry("sources[\"", &source);
            if !default {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// A tile set from its text, and what reading it reported (its id, lines it skipped).
    ///
    /// # Errors
    ///
    /// Not a tile set's file, a newer one, or a bad line (with its number).
    pub fn parse(text: &str) -> Result<(Self, ValueReport), LoadError> {
        let mut file = Self::default();
        let report = crate::read_value(Self::KIND, Self::VERSION, text, &mut file)?;
        Ok((file, report))
    }

    /// The tile set in file `path`.
    ///
    /// # Errors
    ///
    /// The file cannot be read or is not a tile set's (with its path).
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text)
            .map(|(file, _)| file)
            .map_err(|e| format!("{}:{e}", path.display()))
    }

    /// Tile `name`'s rectangle in its source's image, in pixels (left, top, width,
    /// height), if its source is one of the set's.
    pub fn tile_rect(&self, name: TileName<'_>) -> Option<[u32; 4]> {
        let source = self.sources.get(name.source)?;
        let [w, h] = self.tile;
        Some([
            source.margin[0] + name.column * (w + source.gap[0]),
            source.margin[1] + name.row * (h + source.gap[1]),
            w,
            h,
        ])
    }

    /// What tile `name` adds (the defaults for a plain tile).
    pub fn tile(&self, name: &str) -> TileDef {
        self.tiles.get(name).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tile set's text: entries by what they are, defaults left out, read back the same.
    #[test]
    fn a_tile_set_round_trips_without_its_defaults() {
        let image = AssetId::new();
        let mut set = TileSetFile::default();
        set.sources.insert(
            String::from("k2"),
            TileSource {
                image: AssetRef::file(image),
                margin: [1, 1],
                gap: [0, 0],
            },
        );
        set.terrains.insert(
            String::from("grass"),
            Terrain {
                mode: TerrainMode::Corners,
                color: [0.25, 0.75, 0.25, 1.0],
            },
        );
        set.tiles.insert(
            String::from("k2:0,3"),
            TileDef {
                shapes: vec![vec![0.0, 0.0, 16.0, 0.0, 16.0, 16.0, 0.0, 16.0]],
                ..TileDef::default()
            },
        );
        let mut data = BTreeMap::new();
        data.insert(String::from("footstep"), String::from("grass"));
        set.tiles.insert(
            String::from("k2:2,5"),
            TileDef {
                weight: 0.25,
                data,
                ..TileDef::default()
            },
        );
        set.tiles.insert(
            String::from("k2:7,1"),
            TileDef {
                frames: vec![String::from("k2:7,1"), String::from("k2:8,1")],
                frame_time: 0.15,
                ..TileDef::default()
            },
        );
        let id = AssetId::new();
        let text = set.to_text(id);
        assert!(!text.contains("pixels_per_unit"), "a default: {text}");
        assert!(!text.contains("phases"), "a default: {text}");
        assert!(!text.contains("one_way"), "a default: {text}");
        assert!(text.contains("tiles[\"k2:2,5\"].weight = 0.25"), "{text}");
        assert!(
            text.contains("terrains[\"grass\"].mode = Corners"),
            "kept whole: {text}"
        );
        let (read, report) = TileSetFile::parse(&text).unwrap();
        assert_eq!(report.id, Some(id));
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(read.tiles, set.tiles);
        assert_eq!(read.terrains, set.terrains);
        assert_eq!(read.sources["k2"].margin, [1, 1]);
        assert_eq!(read.sources["k2"].image.file, image);
    }

    /// A tile's name and its rectangle in its source.
    #[test]
    fn tiles_are_named_by_source_and_cell() {
        let name = TileName::parse("k2:4,3").unwrap();
        assert_eq!((name.source, name.column, name.row), ("k2", 4, 3));
        assert_eq!(name.to_string(), "k2:4,3");
        assert!(TileName::parse("k2").is_none());
        let mut set = TileSetFile::default();
        set.sources.insert(
            String::from("k2"),
            TileSource {
                margin: [1, 2],
                gap: [2, 2],
                ..TileSource::default()
            },
        );
        assert_eq!(set.tile_rect(name), Some([1 + 4 * 18, 2 + 3 * 18, 16, 16]));
        assert_eq!(set.tile_rect(TileName::parse("x:0,0").unwrap()), None);
    }
}
