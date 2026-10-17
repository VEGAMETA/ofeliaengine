//! A single-line text field (ADR 0030). One field is edited at a time: its buffer, caret
//! and selection live in the [`Ui`] between frames; the field shows the caller's text
//! otherwise. The layout measures where the caret and the selection are, and keeps the
//! caret in view by scrolling the text.

use crate::{Color, Rect, Size, Style, Ui, UiKey};

/// How an edit ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditEnd {
    /// Enter, Tab or a click elsewhere: keep the text.
    Commit,
    /// Escape: drop it.
    Cancel,
}

/// The text field being edited.
#[derive(Debug, Clone)]
pub(crate) struct TextEdit {
    /// The field (its box's id).
    pub(crate) id: u64,
    /// The text node inside it.
    pub(crate) node: u64,
    pub(crate) text: String,
    /// The caret and the selection's other end: byte offsets at character boundaries.
    pub(crate) caret: usize,
    pub(crate) anchor: usize,
    /// How far the text is scrolled left to keep the caret in view, in pixels.
    pub(crate) scroll: f32,
    /// A press or drag at this x from the text's left edge (scrolled), turned into a caret
    /// by the next layout; `true` keeps the anchor (a drag, Shift+click).
    pub(crate) pointer: Option<(f32, bool)>,
    /// The caret's and the selection's x, from the last layout.
    pub(crate) caret_x: f32,
    pub(crate) select_x: [f32; 2],
    /// The text node's rectangle in the last layout.
    pub(crate) rect: Rect,
    /// The text's size in pixels, for measuring.
    pub(crate) size: f32,
    /// The field was built this frame; an edit whose field is gone is dropped.
    pub(crate) seen: bool,
    pub(crate) ended: Option<EditEnd>,
}

impl TextEdit {
    fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    /// Removes the selection; false when there was none.
    fn delete_selection(&mut self) -> bool {
        let (a, b) = self.selection();
        if a == b {
            return false;
        }
        self.text.replace_range(a..b, "");
        self.caret = a;
        self.anchor = a;
        true
    }

    fn insert(&mut self, s: &str) {
        self.delete_selection();
        self.text.insert_str(self.caret, s);
        self.caret += s.len();
        self.anchor = self.caret;
    }

    /// Moves the caret to `to`, extending the selection with Shift.
    fn go(&mut self, to: usize, extend: bool) {
        self.caret = to;
        if !extend {
            self.anchor = to;
        }
    }
}

/// The character boundary before `i`.
fn prev_char(text: &str, i: usize) -> usize {
    text[..i].char_indices().next_back().map_or(0, |(j, _)| j)
}

/// The character boundary after `i`.
fn next_char(text: &str, i: usize) -> usize {
    i + text[i..].chars().next().map_or(0, char::len_utf8)
}

/// The start of the word before `i` (spaces skipped first), as Ctrl+Left moves.
fn prev_word(text: &str, mut i: usize) -> usize {
    while i > 0
        && text[..i]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        i = prev_char(text, i);
    }
    let word = text[..i]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric);
    while i > 0
        && text[..i]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace() && c.is_alphanumeric() == word)
    {
        i = prev_char(text, i);
    }
    i
}

/// The end of the word after `i` (then its spaces), as Ctrl+Right moves.
fn next_word(text: &str, mut i: usize) -> usize {
    let word = text[i..].chars().next().is_some_and(char::is_alphanumeric);
    while i < text.len()
        && text[i..]
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace() && c.is_alphanumeric() == word)
    {
        i = next_char(text, i);
    }
    while i < text.len() && text[i..].chars().next().is_some_and(char::is_whitespace) {
        i = next_char(text, i);
    }
    i
}

/// How a text field looks and starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextField<'a> {
    /// The field's box: size, padding, background, radius (it always clips).
    pub style: Style,
    /// Text size in pixels.
    pub size: f32,
    /// Text color.
    pub color: Color,
    /// Shown dimmed while the text is empty and not being edited.
    pub hint: &'a str,
    /// The hint's color.
    pub hint_color: Color,
    /// Start editing as soon as it is built, everything selected (renaming in place, a
    /// number turned into a field).
    pub autofocus: bool,
    /// A search box over a list (the command palette): Up and Down are left to the list,
    /// and an edit started by `autofocus` or from code keeps the text with the caret at its
    /// end (a prefix such as `@` stays).
    pub search: bool,
}

/// What a text field reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextFieldResponse {
    /// The field's id.
    pub id: u64,
    /// Under the mouse.
    pub hovered: bool,
    /// Being edited.
    pub editing: bool,
    /// The edit started this frame.
    pub started: bool,
    /// The text changed this frame ([`Ui::edited_text`]).
    pub changed: bool,
    /// The edit ended with Enter, Tab or a click elsewhere: keep [`Ui::edited_text`].
    pub committed: bool,
    /// The edit ended with Escape: keep nothing.
    pub cancelled: bool,
}

impl Ui {
    /// A single-line text field showing `text`; while it is edited, its buffer instead.
    /// A click, Tab onto it, [`Ui::edit_field`] or `autofocus` starts an edit; Enter,
    /// Tab and a click elsewhere commit it (read [`Ui::edited_text`] in that frame);
    /// Escape cancels it. Typing, Backspace, Delete, the arrows (Ctrl: by word), Home,
    /// End, Shift to select, and Ctrl+A, C, X, V work as in Windows' own fields (the
    /// application puts the clipboard in [`crate::UiInput::paste`] and takes
    /// [`Ui::take_copied`]).
    pub fn text_field(
        &mut self,
        key: &str,
        text: &str,
        field: &TextField<'_>,
    ) -> TextFieldResponse {
        let id = self.id(key);
        let hovered = self.is_hovered(id);
        let pressed = hovered && self.input.mouse_pressed;
        let mut out = TextFieldResponse {
            id,
            hovered,
            ..TextFieldResponse::default()
        };
        // An edit that ended in this frame elsewhere (another field took over) reports
        // here.
        if let Some(ending) = &self.ending
            && ending.id == id
        {
            out.committed = ending.ended == Some(EditEnd::Commit);
            out.cancelled = ending.ended == Some(EditEnd::Cancel);
        }
        let editing_here = self
            .edit
            .as_ref()
            .is_some_and(|e| e.id == id && e.ended.is_none());
        let requested = self.edit_request == Some(id);
        let start = !editing_here
            && !out.committed
            && (pressed
                || requested
                || (self.focused == id && self.edit.as_ref().is_none_or(|e| e.id != id))
                || (field.autofocus && self.edit.is_none()));
        if start {
            if let Some(mut old) = self.edit.take() {
                if old.ended.is_none() {
                    old.ended = Some(EditEnd::Commit);
                }
                self.ending = Some(old);
            }
            let len = text.len();
            let select_all = !pressed && !field.search;
            // A click puts the caret where it landed; the other starts select it all (a
            // search box keeps the caret at the end).
            let pointer = if pressed {
                self.input.mouse.map(|m| {
                    (
                        m[0] - (self.press_rect.pos[0] + field.style.padding[0]),
                        false,
                    )
                })
            } else {
                None
            };
            self.edit = Some(TextEdit {
                id,
                node: 0,
                text: text.to_owned(),
                caret: len,
                anchor: if select_all { 0 } else { len },
                scroll: 0.0,
                pointer,
                caret_x: 0.0,
                select_x: [0.0; 2],
                rect: Rect::default(),
                size: field.size,
                seen: true,
                ended: None,
            });
            self.focused = id;
            self.edit_request = None;
            out.started = true;
        }
        let (shift, ctrl) = (self.input.shift, self.input.ctrl);
        let mouse = self.input.mouse;
        let mouse_down = self.input.mouse_down;
        let held_here = self.active == id && mouse_down;
        let mut copied = None;
        if let Some(edit) = self
            .edit
            .as_mut()
            .filter(|e| e.id == id && e.ended.is_none())
        {
            edit.seen = true;
            edit.size = field.size;
            if self.focused != id {
                // Tab moved the focus, or a click landed elsewhere.
                edit.ended = Some(EditEnd::Commit);
            } else {
                if !out.started
                    && let Some(m) = mouse
                {
                    if pressed {
                        edit.pointer = Some((m[0] - edit.rect.pos[0] + edit.scroll, shift));
                    } else if held_here {
                        edit.pointer = Some((m[0] - edit.rect.pos[0] + edit.scroll, true));
                    }
                }
                let before = edit.text.len();
                let mut changed = false;
                for &key in &self.input.keys {
                    match key {
                        UiKey::Enter => edit.ended = Some(EditEnd::Commit),
                        UiKey::Left => {
                            let (a, _) = edit.selection();
                            let to = if ctrl {
                                prev_word(&edit.text, edit.caret)
                            } else if !shift && a != edit.caret.max(edit.anchor) {
                                a
                            } else {
                                prev_char(&edit.text, edit.caret)
                            };
                            edit.go(to, shift);
                        }
                        UiKey::Right => {
                            let (_, b) = edit.selection();
                            let to = if ctrl {
                                next_word(&edit.text, edit.caret)
                            } else if !shift && b != edit.caret.min(edit.anchor) {
                                b
                            } else {
                                next_char(&edit.text, edit.caret)
                            };
                            edit.go(to, shift);
                        }
                        UiKey::Up | UiKey::Down if field.search => {}
                        UiKey::Home | UiKey::Up => edit.go(0, shift),
                        UiKey::End | UiKey::Down => edit.go(edit.text.len(), shift),
                        UiKey::Backspace => {
                            if !edit.delete_selection() && edit.caret > 0 {
                                let from = if ctrl {
                                    prev_word(&edit.text, edit.caret)
                                } else {
                                    prev_char(&edit.text, edit.caret)
                                };
                                edit.text.replace_range(from..edit.caret, "");
                                edit.caret = from;
                                edit.anchor = from;
                            }
                            changed = true;
                        }
                        UiKey::Delete => {
                            if !edit.delete_selection() && edit.caret < edit.text.len() {
                                let to = if ctrl {
                                    next_word(&edit.text, edit.caret)
                                } else {
                                    next_char(&edit.text, edit.caret)
                                };
                                edit.text.replace_range(edit.caret..to, "");
                            }
                            changed = true;
                        }
                        UiKey::SelectAll => {
                            edit.anchor = 0;
                            edit.caret = edit.text.len();
                        }
                        UiKey::Copy | UiKey::Cut => {
                            let (a, b) = edit.selection();
                            if a < b {
                                copied = Some(edit.text[a..b].to_owned());
                                if key == UiKey::Cut {
                                    edit.delete_selection();
                                    changed = true;
                                }
                            }
                        }
                        UiKey::Paste => {
                            // One line: line breaks and tabs become spaces.
                            let one_line: String = self
                                .input
                                .paste
                                .chars()
                                .map(|c| if c.is_control() { ' ' } else { c })
                                .collect();
                            edit.insert(&one_line);
                            changed = true;
                        }
                        _ => {}
                    }
                }
                if !self.input.text.is_empty() {
                    let typed = &self.input.text;
                    edit.insert(typed);
                    changed = true;
                }
                out.changed = changed || edit.text.len() != before;
            }
            out.editing = edit.ended.is_none();
            out.committed |= edit.ended == Some(EditEnd::Commit);
            out.cancelled |= edit.ended == Some(EditEnd::Cancel);
            if edit.ended.is_some() && self.focused == id {
                self.focused = 0;
            }
        } else if let Some(edit) = self.edit.as_ref().filter(|e| e.id == id) {
            // Ended earlier in this frame (Escape in `begin_frame`).
            out.committed |= edit.ended == Some(EditEnd::Commit);
            out.cancelled |= edit.ended == Some(EditEnd::Cancel);
        }
        if copied.is_some() {
            self.copied = copied;
        }

        let editing = out.editing;
        let (shown, color) = match self.edit.as_ref().filter(|e| e.id == id && editing) {
            Some(_) => (None, field.color),
            None if text.is_empty() => (Some(field.hint), field.hint_color),
            None => (Some(text), field.color),
        };
        let style = Style {
            clip: true,
            ..field.style
        };
        self.begin(key, style);
        let text_style = Style {
            size: [Size::Grow(1.0), Size::Fit],
            ..Style::default()
        };
        let node = match shown {
            Some(s) => self.text_node("text", text_style, s, field.size, color, false),
            None => {
                // The buffer itself into the frame's text.
                let start = self.text.len();
                if let Some(edit) = &self.edit {
                    self.text.push_str(&edit.text);
                }
                let span = crate::TextSpan {
                    range: start..self.text.len(),
                    size: field.size,
                    color,
                    wrap: false,
                };
                let node = self.id("text");
                self.open(node, text_style, Some(span));
                self.close();
                node
            }
        };
        self.end();
        if let Some(edit) = self.edit.as_mut().filter(|e| e.id == id) {
            edit.node = node;
        }
        let r = self.sense(id, true);
        out.hovered = r.hovered;
        out
    }

    /// The text of the field being edited, or of the edit that ended in this frame (read
    /// it when [`TextFieldResponse::committed`]).
    pub fn edited_text(&self) -> &str {
        match (&self.edit, &self.ending) {
            (Some(edit), _) if edit.ended.is_some() => &edit.text,
            (_, Some(ending)) => &ending.text,
            (Some(edit), None) => &edit.text,
            (None, None) => "",
        }
    }

    /// Starts editing field `id` (a [`Ui::text_field`]'s id) when it is next built, its text
    /// all selected: F2 on a name, Enter on a number.
    pub fn edit_field(&mut self, id: u64) {
        self.edit_request = Some(id);
    }

    /// The field being edited, if any: keyboard shortcuts should wait while one is.
    pub fn editing(&self) -> Option<u64> {
        self.edit
            .as_ref()
            .filter(|e| e.ended.is_none())
            .map(|e| e.id)
    }

    /// What Ctrl+C or Ctrl+X took this frame, for the application to put on the
    /// clipboard.
    pub fn take_copied(&mut self) -> Option<String> {
        self.copied.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_jumps_skip_spaces_and_stop_at_punctuation() {
        let t = "light.shadow  bias";
        assert_eq!(next_word(t, 0), 5);
        assert_eq!(next_word(t, 5), 6);
        assert_eq!(next_word(t, 6), 14);
        assert_eq!(prev_word(t, t.len()), 14);
        assert_eq!(prev_word(t, 14), 6);
        assert_eq!(prev_word(t, 6), 5);
        let u = "дверь Ω";
        assert_eq!(next_char(u, 0), 2);
        assert_eq!(prev_char(u, u.len()), u.len() - 2);
        assert_eq!(prev_word(u, u.len()), u.len() - 2);
    }
}
