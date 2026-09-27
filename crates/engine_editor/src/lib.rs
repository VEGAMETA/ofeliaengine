//! Editor core (CLAUDE.md §12). The first piece is [`History`]: undoable transactions
//! over the world (ADR 0031). Panels, tools, the command palette and docking build on
//! it and on `engine_ui`.

mod undo;

pub use undo::{History, Transaction};
