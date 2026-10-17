//! Editor core (CLAUDE.md §12). The first piece is [`History`]: undoable transactions
//! over the world (ADR 0031). Panels, tools, the command palette and docking build on
//! it and on `engine_ui`. [`Commands`] and [`Keymap`] are the command palette's and
//! shortcuts' core. [`shell`] lays out the editor window (the team's Godot layout) and
//! [`paint`] draws `engine_ui` frames with `engine_render2d` and `engine_text`; [`icons`]
//! names the icon glyphs it uses. [`input`] turns window events into UI input and
//! [`camera`] is the viewport's orbit camera.

pub mod app;
pub mod camera;
pub mod expr;
pub mod icons;
pub mod input;
pub mod inspector;
pub mod paint;
mod palette;
pub mod shell;
mod snap;
mod undo;

pub use app::{EditorGame, EditorOverlay, Start, editor, run, run_frames};
pub use palette::{
    Chord, CommandId, CommandInfo, Commands, Conflict, Fuzzy, KeyContext, Keymap, Match,
};
pub use snap::{Quantity, Snapping};
pub use undo::{History, Transaction};
