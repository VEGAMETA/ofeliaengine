//! Save games (ADR 0104).
//!
//! - **Everything but what is marked:** every entity with a [`SceneId`] or a reflected
//!   component, and its reflected components, and every reflected resource, unless a type
//!   or field is marked `#[reflect(skip)]` or an entity [`Transient`]. The hierarchy comes
//!   along; entity references are remapped.
//! - **Taken in a frame** ([`capture`]): plain-data components are copied, the rest written
//!   at once; nothing else that grows with the save runs in the frame.
//! - **Written behind it** ([`write_save`]): a binary of named fields (each type's fields
//!   named once per file) or, for development, the same as text; zstd; a checksum; a
//!   temporary file swapped in, the previous save kept as `.bak`.
//! - **Read back** ([`read_save`], both encodings always) and **applied** ([`apply`]):
//!   fields by name, numbers converted, variants by name; what no longer fits is reported,
//!   not fatal.
//!
//! ```
//! use engine_ecs::reflect::Reflect;
//! use engine_ecs::{World, reflect_component};
//!
//! #[derive(Reflect, Default, Clone, Copy, Debug, PartialEq)]
//! struct Health { current: f32 }
//! reflect_component!(Health);
//!
//! let mut world = World::new();
//! world.spawn((Health { current: 40.0 },));
//! let image = engine_save::capture(&world, &engine_save::CaptureOptions::default());
//! let bytes = engine_save::encode(&image, engine_save::Encoding::Text, &Default::default()).unwrap();
//! let save = engine_save::decode(&bytes).unwrap();
//! let mut loaded = World::new();
//! engine_save::apply(&mut loaded, &save).unwrap();
//! ```

mod apply;
mod body;
mod capture;
mod file;
mod schema;
mod text;
mod value;
mod wire;

pub use apply::{ApplyOptions, LoadReport, apply, apply_with};
pub use body::LoadedSave;
pub use capture::{CaptureOptions, SaveImage, Transient, capture};
pub use engine_scene::SceneId;
pub use file::{
    Encoding, SaveHeader, SlotInfo, WriteStats, decode, encode, list_slots, read_header, read_save,
    slot_path, text_file, write_save,
};
pub use text::dump;

/// Why a save could not be written or read.
#[derive(Debug)]
pub enum SaveError {
    /// The file system refused.
    Io(std::io::Error),
    /// Not a save, damaged, or from a newer engine.
    Damaged(String),
    /// A line of a text save could not be read.
    Text {
        /// Its line number, from 1.
        line: usize,
        /// What was wrong.
        message: String,
    },
}

impl SaveError {
    pub(crate) fn damaged(why: impl Into<String>) -> Self {
        Self::Damaged(why.into())
    }
}

impl core::fmt::Display for SaveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Damaged(why) => write!(f, "not a usable save: {why}"),
            Self::Text { line, message } => write!(f, "line {line}: {message}"),
        }
    }
}

impl std::error::Error for SaveError {}

impl From<std::io::Error> for SaveError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
