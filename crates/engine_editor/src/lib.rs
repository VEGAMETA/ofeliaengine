//! Editor core (CLAUDE.md §12). The first piece is [`History`]: undoable transactions
//! over the world (ADR 0031). Panels, tools, the command palette and docking build on
//! it and on `engine_ui`. [`Commands`] and [`Keymap`] are the command palette's and
//! shortcuts' core. [`shell`] lays out the editor window (the team's Godot layout) and
//! [`paint`] draws `engine_ui` frames with `engine_render2d` and `engine_text`; [`icons`]
//! names the icon glyphs it uses. [`input`] turns window events into UI input and
//! [`camera`] is the viewport's orbit camera. The session and the scenes' autosaves live in
//! the project's `.ofelia/` folder (ADR 0107 §4).

pub mod app;
mod breakpoints;
mod build;
pub mod camera;
mod checker;
mod code;
mod complete;
mod draw;
mod drop;
pub mod expr;
mod faces;
mod find;
mod gizmo;
mod graph;
mod handles;
mod highlight;
pub mod icons;
mod import;
pub mod input;
pub mod inspector;
mod marked;
mod material;
mod mixer;
mod node_functions;
mod outline;
mod output;
pub mod paint;
mod palette;
mod prefabs;
mod profile;
mod script;
mod session;
pub mod shell;
mod slang_check;
mod slang_highlight;
mod slangd;
mod snap;
mod sound_handles;
mod sound_view;
mod text_highlight;
mod thumbnails;
mod transform;
mod undo;

pub use app::{EditorGame, EditorOverlay, Start, editor, run, run_frames, run_game};
pub use build::{BUILD_VAR, GameCrate};
pub use palette::{
    Chord, CommandId, CommandInfo, Commands, Conflict, Fuzzy, KeyContext, Keymap, Match,
};
pub use snap::{Quantity, Snapping};
pub use undo::{History, Kept, PlayChange, PlayMark, Rewrite, Transaction, WorldSnapshot};
