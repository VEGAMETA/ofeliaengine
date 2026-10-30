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

/// The quote that closes the Luau string the caret at byte `col` of `line` is in, when the
/// line leaves that string open (as it is while typed: the editor closes no quotes). The
/// completion's question closes it at the caret, so Luau answers as inside a string (its
/// string choices: an animator's parameters, ADR 0124's E12-5c), not as at an expression
/// (every global). None outside a string, in a closed one, and after what this does not
/// read: a comment, a long string, an interpolated string.
pub(crate) fn open_quote(line: &str, col: usize) -> Option<char> {
    let bytes = line.as_bytes();
    let col = col.min(bytes.len());
    let mut open: Option<u8> = None;
    let mut i = 0;
    // Up to the caret: the string it is in, if any.
    while i < col {
        let b = bytes[i];
        match open {
            // The escaped byte is skipped.
            Some(_) if b == b'\\' => i += 1,
            Some(q) if b == q => open = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' => open = Some(b),
                b'-' if bytes.get(i + 1) == Some(&b'-') => return None,
                b'`' => return None,
                b'[' if matches!(bytes.get(i + 1), Some(b'[' | b'=')) => return None,
                _ => {}
            },
        }
        i += 1;
    }
    let quote = open?;
    // After it: open unless it closes there.
    let mut j = i;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 1,
            b if b == quote => return None,
            _ => {}
        }
        j += 1;
    }
    Some(char::from(quote))
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

    /// E12-5c: a string the caret is in that its line leaves open is closed by its own
    /// quote; a closed one, code, comments and strings this does not read are left.
    #[test]
    fn a_string_left_open_at_the_caret_is_closed_for_the_question() {
        let at = |line: &str| open_quote(&line.replace('|', ""), line.find('|').unwrap());
        assert_eq!(at("    a:set(\"sp|"), Some('"'));
        assert_eq!(at("    a:set(\"sp|, 1)"), Some('"'), "no quote after it");
        assert_eq!(at("    a:set('|"), Some('\''));
        assert_eq!(at("    a:set(\"sp|\", 1)"), None, "closed after it");
        assert_eq!(
            at("    a:set(\"x\", 1) b(\"|"),
            Some('"'),
            "the second string"
        );
        assert_eq!(at("    a:set(\"x\", |"), None, "between strings");
        assert_eq!(
            at("    a:set(\"it\\\"s|"),
            Some('"'),
            "an escaped quote stays inside"
        );
        assert_eq!(
            at("    a:set(\"it's|"),
            Some('"'),
            "the other quote is text"
        );
        assert_eq!(at("    t[\"ke|"), Some('"'), "a key");
        assert_eq!(at("    -- set(\"sp|"), None, "a comment");
        assert_eq!(at("    x = [[ set(\"sp|"), None, "a long string");
        assert_eq!(at("    x = `{a(\"sp|"), None, "an interpolated string");
        assert_eq!(
            at("    local s = \"a -- b|"),
            Some('"'),
            "`--` inside a string"
        );
    }
}
