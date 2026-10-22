//! Scripts open in tabs beside the scenes (ADR 0107 E8, `docs/research/ux/code-editor.md`
//! §1–2): each one's file and its text being edited (`code::Document`), where its view is
//! scrolled, and the keys that edit it, VS Code's.

use crate::Chord;
use crate::app::is_function_key;
use crate::code::{Document, Motion, Pos, char_column, col_at};
use crate::session::Base;
use crate::shell::{CODE_LINE, CODE_PAD};
use engine_input::Key;
use std::path::PathBuf;
use std::time::Instant;

/// Lines a notch of the wheel scrolls (VS Code's).
pub(crate) const WHEEL_LINES: f32 = 3.0;

/// A script open in a tab.
pub(crate) struct Script {
    /// Its file, relative to the project.
    pub(crate) path: PathBuf,
    /// Its text, being edited.
    pub(crate) doc: Document,
    /// How far its view is scrolled: logical pixels across and down.
    pub(crate) scroll: [f32; 2],
    /// The primary caret is brought into view at the next frame: it moved, or the text
    /// changed.
    pub(crate) reveal: bool,
    /// Its file as read or last saved: a change made outside shows against it.
    pub(crate) base: Option<Base>,
    /// The text's version in its autosave.
    pub(crate) autosaved: Option<u64>,
    /// When its unsaved text is due in its autosave.
    pub(crate) due: Option<Instant>,
    /// Closing it was asked with unsaved changes: asked again, they are dropped.
    pub(crate) closing: bool,
    /// Its file's lines end with `\r\n`, and it starts with a byte order mark: saved so
    /// again.
    crlf: bool,
    bom: bool,
}

impl Script {
    /// The script at `path` (relative to the project) whose file holds `text`, read when
    /// it was `base`.
    pub(crate) fn new(path: PathBuf, text: &str, base: Option<Base>) -> Self {
        let body = text.strip_prefix('\u{feff}');
        Self {
            crlf: text.contains("\r\n"),
            bom: body.is_some(),
            path,
            doc: Document::new(body.unwrap_or(text)),
            scroll: [0.0; 2],
            reveal: false,
            base,
            autosaved: None,
            due: None,
            closing: false,
        }
    }

    /// The text as its file holds it: its line endings and its byte order mark kept.
    pub(crate) fn file_text(&self) -> String {
        let mut text = self.doc.text();
        if self.crlf {
            text = text.replace('\n', "\r\n");
        }
        if self.bom {
            text.insert(0, '\u{feff}');
        }
        text
    }

    /// Its tab's label: the file's name, `*` after it while it has unsaved changes.
    pub(crate) fn label(&self) -> String {
        let name = self
            .path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if self.doc.is_dirty() {
            format!("{name} *")
        } else {
            name
        }
    }

    /// The place in the text under `at`, in logical pixels from the code area's corner,
    /// a character `advance` wide and the gutter `gutter`: the line under it, the column
    /// boundary nearest it.
    pub(crate) fn pos_at(&self, at: [f32; 2], advance: f32, gutter: f32) -> Pos {
        let line = ((at[1] + self.scroll[1]) / CODE_LINE).floor().max(0.0) as usize;
        let line = line.min(self.doc.line_count() - 1);
        let column = ((at[0] - gutter - CODE_PAD + self.scroll[0]) / advance.max(1.0))
            .round()
            .max(0.0) as usize;
        Pos::new(line, col_at(self.doc.line(line), column))
    }

    /// Scrolls as little as shows the primary caret in a view `size` big (the gutter, of
    /// `gutter`, included), a few characters to spare across.
    pub(crate) fn reveal_in(&mut self, size: [f32; 2], advance: f32, gutter: f32) {
        let head = self.doc.primary().head;
        let top = head.line as f32 * CODE_LINE;
        if top + CODE_LINE > self.scroll[1] + size[1] {
            self.scroll[1] = top + CODE_LINE - size[1];
        }
        if top < self.scroll[1] {
            self.scroll[1] = top;
        }
        let x = char_column(self.doc.line(head.line), head.col) as f32 * advance;
        let width = (size[0] - gutter - CODE_PAD).max(advance * 8.0);
        let margin = advance * 4.0;
        if x > self.scroll[0] + width - margin {
            self.scroll[0] = x - width + margin;
        }
        if x < self.scroll[0] + margin {
            self.scroll[0] = (x - margin).max(0.0);
        }
        self.clamp_scroll();
    }

    /// The scroll kept within the text: at most the last line at the top.
    pub(crate) fn clamp_scroll(&mut self) {
        let last = (self.doc.line_count() - 1) as f32 * CODE_LINE;
        self.scroll[1] = self.scroll[1].clamp(0.0, last);
        self.scroll[0] = self.scroll[0].max(0.0);
    }
}

/// The gutter's width in a script of `lines` lines: room for the largest number, two
/// digits at least.
pub(crate) fn gutter_width(lines: usize, advance: f32) -> f32 {
    let digits = (lines.max(1).ilog10() + 1).max(2) as f32;
    (digits + 1.0) * advance + CODE_PAD
}

/// What a key did in a script, besides editing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    /// The text or the carets changed: the primary caret is brought into view.
    Edited,
    /// This was copied or cut: onto the clipboard.
    Copy(String),
    /// The clipboard is pasted at the carets.
    Paste,
    /// The view scrolls by this many lines, the carets where they are (Ctrl+Up, Ctrl+Down).
    Scroll(i32),
    /// The key is the code's with nothing to do (one that types: its character comes
    /// apart), or nothing changed in view.
    Taken,
}

/// `chord` applied to `doc` with VS Code's keys, `page` lines a page: what it did, or
/// `None` when the key is the editor's instead (Ctrl+S, the palette, Ctrl+Tab, F5).
pub(crate) fn key(doc: &mut Document, chord: Chord, page: usize) -> Option<Effect> {
    use Effect::{Copy, Edited, Paste, Scroll, Taken};
    let Chord {
        ctrl,
        shift,
        alt,
        key,
    } = chord;
    let motion = match (ctrl, alt, key) {
        (false, false, Key::ArrowLeft) => Some(Motion::Left),
        (false, false, Key::ArrowRight) => Some(Motion::Right),
        (false, false, Key::ArrowUp) => Some(Motion::Up),
        (false, false, Key::ArrowDown) => Some(Motion::Down),
        (false, false, Key::Home) => Some(Motion::Home),
        (false, false, Key::End) => Some(Motion::End),
        (false, false, Key::PageUp) => Some(Motion::PageUp(page)),
        (false, false, Key::PageDown) => Some(Motion::PageDown(page)),
        (true, false, Key::ArrowLeft) => Some(Motion::WordLeft),
        (true, false, Key::ArrowRight) => Some(Motion::WordRight),
        (true, false, Key::Home) => Some(Motion::Start),
        (true, false, Key::End) => Some(Motion::Finish),
        _ => None,
    };
    if let Some(motion) = motion {
        doc.move_carets(motion, shift);
        return Some(Edited);
    }
    Some(match (ctrl, alt, key) {
        (false, false, Key::Enter | Key::NumpadEnter) => {
            doc.newline();
            Edited
        }
        (false, false, Key::Backspace) => {
            doc.backspace();
            Edited
        }
        // Shift+Delete cuts, as in Windows.
        (false, false, Key::Delete) if shift => Copy(doc.cut()),
        (false, false, Key::Delete) => {
            doc.delete();
            Edited
        }
        (false, false, Key::Tab) => {
            if shift {
                doc.outdent();
            } else {
                doc.indent();
            }
            Edited
        }
        // Escape: the other carets gone, then the selection.
        (false, false, Key::Escape) => {
            if doc.carets().len() > 1 {
                doc.single_caret();
            } else {
                doc.place(doc.primary().head, false, false);
            }
            Taken
        }
        // Ctrl+Enter: a new line under the caret's.
        (true, false, Key::Enter | Key::NumpadEnter) if !shift => {
            doc.move_carets(Motion::End, false);
            doc.newline();
            Edited
        }
        (true, false, Key::Backspace) => {
            doc.delete_word(true);
            Edited
        }
        (true, false, Key::Delete) => {
            doc.delete_word(false);
            Edited
        }
        (true, false, Key::KeyA) if !shift => {
            doc.select_all();
            Taken
        }
        (true, false, Key::KeyC | Key::Insert) if !shift => Copy(doc.copy()),
        (true, false, Key::KeyX) if !shift => Copy(doc.cut()),
        (true, false, Key::KeyV) if !shift => Paste,
        (false, false, Key::Insert) if shift => Paste,
        (true, false, Key::KeyZ) => {
            if shift {
                doc.redo();
            } else {
                doc.undo();
            }
            Edited
        }
        (true, false, Key::KeyY) if !shift => {
            doc.redo();
            Edited
        }
        (true, false, Key::KeyD) if !shift => {
            doc.select_next_match();
            Edited
        }
        (true, false, Key::Slash | Key::NumpadDivide) => {
            doc.toggle_comment();
            Edited
        }
        (true, false, Key::BracketRight) => {
            doc.indent_lines();
            Edited
        }
        (true, false, Key::BracketLeft) => {
            doc.outdent();
            Edited
        }
        (true, false, Key::KeyK) if shift => {
            doc.delete_lines();
            Edited
        }
        (true, false, Key::ArrowUp) => Scroll(-1),
        (true, false, Key::ArrowDown) => Scroll(1),
        (false, true, Key::ArrowUp) if !shift => {
            doc.move_lines(false);
            Edited
        }
        (false, true, Key::ArrowDown) if !shift => {
            doc.move_lines(true);
            Edited
        }
        (true, true, Key::ArrowUp) => {
            doc.add_caret(false);
            Edited
        }
        (true, true, Key::ArrowDown) => {
            doc.add_caret(true);
            Edited
        }
        // The keys that type, whose characters come apart, and the others without Ctrl
        // or Alt; F1 to F24 stay the editor's.
        (false, false, k) if !is_function_key(k) => Taken,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(s: &str) -> Chord {
        Chord::parse(s).unwrap()
    }

    #[test]
    fn the_code_takes_its_keys_and_leaves_the_editor_its_own() {
        let mut doc = Document::new("local a = 1\nlocal b = 2");
        assert_eq!(key(&mut doc, chord("End"), 10), Some(Effect::Edited));
        assert_eq!(doc.primary().head, Pos::new(0, 11));
        assert_eq!(key(&mut doc, chord("Shift+Home"), 10), Some(Effect::Edited));
        assert_eq!(doc.selected_text(), "local a = 1");
        assert_eq!(
            key(&mut doc, chord("Ctrl+C"), 10),
            Some(Effect::Copy(String::from("local a = 1")))
        );
        assert_eq!(key(&mut doc, chord("Ctrl+V"), 10), Some(Effect::Paste));
        assert_eq!(key(&mut doc, chord("Ctrl+Slash"), 10), Some(Effect::Edited));
        assert_eq!(doc.line(0), "-- local a = 1");
        assert_eq!(key(&mut doc, chord("Ctrl+Z"), 10), Some(Effect::Edited));
        assert_eq!(doc.line(0), "local a = 1");
        // Letters type (their characters come apart); the editor keeps saving, the
        // palette, the tabs and the function keys.
        assert_eq!(key(&mut doc, chord("W"), 10), Some(Effect::Taken));
        assert_eq!(key(&mut doc, chord("Shift+F"), 10), Some(Effect::Taken));
        for editor in [
            "Ctrl+S",
            "Ctrl+Shift+P",
            "Ctrl+Tab",
            "Ctrl+W",
            "F5",
            "Shift+F5",
        ] {
            assert_eq!(key(&mut doc, chord(editor), 10), None, "{editor}");
        }
        assert_eq!(
            key(&mut doc, chord("Ctrl+ArrowDown"), 10),
            Some(Effect::Scroll(1))
        );
        assert_eq!(
            key(&mut doc, chord("Ctrl+Alt+ArrowDown"), 10),
            Some(Effect::Edited)
        );
        assert_eq!(doc.carets().len(), 2);
        assert_eq!(key(&mut doc, chord("Escape"), 10), Some(Effect::Taken));
        assert_eq!(doc.carets().len(), 1);
    }

    #[test]
    fn places_under_the_pointer_and_the_caret_kept_in_view() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut s = Script::new(PathBuf::from("scripts/door.luau"), &text, None);
        let (advance, gutter) = (8.0, 40.0);
        // Line 2, the boundary nearest 3.4 characters in.
        let p = s.pos_at(
            [gutter + CODE_PAD + 3.4 * advance, 2.5 * CODE_LINE],
            advance,
            gutter,
        );
        assert_eq!(p, Pos::new(2, 3));
        s.doc.place(Pos::new(60, 0), false, false);
        s.reveal_in([400.0, 10.0 * CODE_LINE], advance, gutter);
        assert_eq!(
            s.scroll[1],
            51.0 * CODE_LINE,
            "the caret's line at the bottom"
        );
        s.doc.place(Pos::new(5, 0), false, false);
        s.reveal_in([400.0, 10.0 * CODE_LINE], advance, gutter);
        assert_eq!(s.scroll[1], 5.0 * CODE_LINE, "and at the top");
        s.scroll[1] = 1e6;
        s.clamp_scroll();
        assert_eq!(
            s.scroll[1],
            100.0 * CODE_LINE,
            "the last line at most at the top"
        );
        assert_eq!(s.label(), "door.luau");
        s.doc.type_text("x");
        assert_eq!(s.label(), "door.luau *");
        // Saved as the file was: its line ends and its byte order mark.
        let s = Script::new(PathBuf::from("a.luau"), "\u{feff}a\r\nb\r\n", None);
        assert_eq!(s.doc.line(0), "a");
        assert_eq!(s.file_text(), "\u{feff}a\r\nb\r\n");
        assert_eq!(gutter_width(9, 8.0), 3.0 * 8.0 + CODE_PAD);
        assert_eq!(gutter_width(1500, 8.0), 5.0 * 8.0 + CODE_PAD);
    }
}
