//! The code editor's completion list (ADR 0107 E8-3c): what Luau's analysis offers at the
//! word being typed, narrowed as the word grows, VS Code's keys (Up and Down, Enter or Tab
//! to write one, Escape to close).

use crate::Chord;
use crate::code::{Document, Pos};
use engine_input::Key;
use engine_luau_analysis::{Completion, CompletionKind};

/// Rows the list shows at once.
pub(crate) const ROWS: usize = 10;

/// The completion list of a script's tab, while open.
#[derive(Debug)]
pub(crate) struct Completing {
    /// Where the word being completed starts.
    pub(crate) start: Pos,
    /// What the analysis offered (nothing until it answered), and which of it matches
    /// what is typed, best first.
    pub(crate) items: Vec<Completion>,
    pub(crate) shown: Vec<usize>,
    /// The row picked among those shown, and the first one in view.
    pub(crate) selected: usize,
    pub(crate) first: usize,
    /// The analysis answered.
    pub(crate) answered: bool,
    /// What it was narrowed to last, and whether an answer came since.
    typed: String,
    fresh: bool,
}

/// What a key did to the open list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListKey {
    /// The picked row moved.
    Moved,
    /// The picked one is written.
    Accept,
    /// The list closes.
    Close,
}

impl Completing {
    /// A list for the word starting at `start` (the analysis's answer is matched to it).
    pub(crate) fn new(start: Pos) -> Self {
        Self {
            start,
            items: Vec::new(),
            shown: Vec::new(),
            selected: 0,
            first: 0,
            answered: false,
            typed: String::new(),
            fresh: false,
        }
    }

    /// The analysis's answer.
    pub(crate) fn answer(&mut self, items: Vec<Completion>) {
        self.items = items;
        self.answered = true;
        self.fresh = true;
    }

    /// Narrows the list to `typed` when it changed or an answer came.
    pub(crate) fn narrow(&mut self, typed: &str) {
        if !self.answered || (!self.fresh && self.typed == typed) {
            return;
        }
        self.fresh = false;
        typed.clone_into(&mut self.typed);
        self.filter(typed);
    }

    /// The rows matching `typed`: those starting with it (in any case) first, then those
    /// holding it, each group in the analysis's order.
    pub(crate) fn filter(&mut self, typed: &str) {
        let typed = typed.to_lowercase();
        self.shown.clear();
        let lowered: Vec<String> = self.items.iter().map(|c| c.label.to_lowercase()).collect();
        self.shown
            .extend((0..self.items.len()).filter(|&i| lowered[i].starts_with(&typed)));
        if !typed.is_empty() {
            self.shown.extend(
                (0..self.items.len())
                    .filter(|&i| !lowered[i].starts_with(&typed) && lowered[i].contains(&typed)),
            );
        }
        self.selected = 0;
        self.first = 0;
    }

    /// What `chord` does to the list, if it is one of its keys.
    pub(crate) fn key(&mut self, chord: Chord) -> Option<ListKey> {
        if chord.ctrl || chord.alt {
            return None;
        }
        let n = self.shown.len();
        let step = |by: isize, selected: usize| {
            if n == 0 {
                0
            } else {
                (selected as isize + by).rem_euclid(n as isize) as usize
            }
        };
        let key = match chord.key {
            Key::ArrowDown => {
                self.selected = step(1, self.selected);
                ListKey::Moved
            }
            Key::ArrowUp => {
                self.selected = step(-1, self.selected);
                ListKey::Moved
            }
            Key::PageDown => {
                self.selected = (self.selected + ROWS).min(n.saturating_sub(1));
                ListKey::Moved
            }
            Key::PageUp => {
                self.selected = self.selected.saturating_sub(ROWS);
                ListKey::Moved
            }
            Key::Enter | Key::NumpadEnter | Key::Tab if n > 0 => ListKey::Accept,
            Key::Escape => ListKey::Close,
            _ => return None,
        };
        if self.selected < self.first {
            self.first = self.selected;
        } else if self.selected >= self.first + ROWS {
            self.first = self.selected + 1 - ROWS;
        }
        Some(key)
    }

    /// The picked row's completion.
    pub(crate) fn picked(&self) -> Option<&Completion> {
        self.shown.get(self.selected).map(|&i| &self.items[i])
    }

    /// Writes the picked completion in place of what is typed of the word (up to the
    /// caret), with its parentheses when it is a function and none follow: one undo step.
    pub(crate) fn accept(&self, doc: &mut Document) -> bool {
        let Some(c) = self.picked() else {
            return false;
        };
        let head = doc.primary().head;
        if head.line != self.start.line || head.col < self.start.col {
            return false;
        }
        let rest = &doc.line(head.line)[head.col..];
        let parens = c.call && !rest.trim_start().starts_with('(');
        let text = if parens {
            format!("{}()", c.label)
        } else {
            c.label.clone()
        };
        doc.select(self.start, head, false);
        doc.insert(&text);
        // Between the parentheses when it takes arguments.
        if parens && !c.no_arguments {
            let at = doc.primary().head;
            doc.place(Pos::new(at.line, at.col - 1), false, false);
        }
        true
    }
}

/// Where the word ending at `at` starts (letters, digits and `_` before it).
pub(crate) fn word_start(doc: &Document, at: Pos) -> Pos {
    let line = doc.line(at.line);
    let col = line[..at.col]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| c.is_alphanumeric() || c == '_')
        .last()
        .map_or(at.col, |(i, _)| i);
    Pos::new(at.line, col)
}

/// A completion's icon, VS Code's codicon for its kind.
pub(crate) fn icon(c: &Completion) -> &'static str {
    match c.kind {
        CompletionKind::Property if c.call => crate::icons::METHOD,
        CompletionKind::Property => crate::icons::FIELD,
        CompletionKind::Binding if c.call => crate::icons::METHOD,
        CompletionKind::Binding => crate::icons::VARIABLE,
        CompletionKind::Keyword => crate::icons::KEYWORD,
        CompletionKind::Type => crate::icons::TYPE,
        CompletionKind::Module => crate::icons::MODULE,
        CompletionKind::String => crate::icons::STRING,
        CompletionKind::Other => crate::icons::MISC,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, kind: CompletionKind, call: bool) -> Completion {
        Completion {
            label: label.to_owned(),
            kind,
            detail: String::new(),
            fits: false,
            call,
            no_arguments: false,
            deprecated: false,
        }
    }

    #[test]
    fn the_list_narrows_moves_and_writes_with_parentheses() {
        let mut doc = Document::new("local n = Door.op");
        doc.place(Pos::new(0, 17), false, false);
        let start = word_start(&doc, doc.primary().head);
        assert_eq!(start, Pos::new(0, 15));
        let mut list = Completing::new(start);
        list.answer(vec![
            item("speed", CompletionKind::Property, false),
            item("open", CompletionKind::Property, true),
            item("open_angle", CompletionKind::Property, false),
            item("reopen", CompletionKind::Property, false),
        ]);
        list.narrow("op");
        let labels: Vec<&str> = list
            .shown
            .iter()
            .map(|&i| list.items[i].label.as_str())
            .collect();
        assert_eq!(
            labels,
            ["open", "open_angle", "reopen"],
            "starting with it first"
        );
        assert_eq!(list.key(Chord::key(Key::ArrowUp)), Some(ListKey::Moved));
        assert_eq!(list.picked().unwrap().label, "reopen", "around the top");
        assert_eq!(list.key(Chord::key(Key::ArrowDown)), Some(ListKey::Moved));
        assert_eq!(list.key(Chord::key(Key::Enter)), Some(ListKey::Accept));
        assert!(list.accept(&mut doc));
        assert_eq!(doc.line(0), "local n = Door.open()");
        assert_eq!(
            doc.primary().head,
            Pos::new(0, 20),
            "between the parentheses"
        );
        assert!(doc.undo());
        assert_eq!(doc.line(0), "local n = Door.op", "one step");
        assert_eq!(list.key(Chord::key(Key::Escape)), Some(ListKey::Close));
        assert_eq!(
            list.key(Chord::key(Key::KeyA)),
            None,
            "typing goes on in the code"
        );
    }
}
