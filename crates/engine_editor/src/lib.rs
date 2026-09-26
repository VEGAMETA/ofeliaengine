//! Editor core (CLAUDE.md §12). The first piece is [`History`]: undoable transactions
//! over the world (ADR 0031). Panels, tools, the command palette and docking build on
//! it and on `engine_ui`. [`Commands`] and [`Keymap`] are the command palette's and
//! shortcuts' core.

mod palette;
mod undo;

pub use palette::{Chord, CommandId, CommandInfo, Commands, Conflict, KeyContext, Keymap, Match};
pub use undo::{History, Transaction};
