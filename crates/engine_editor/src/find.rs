//! Find and replace in a script (ADR 0107 E8-2, `docs/research/ux/code-editor.md` §2): VS
//! Code's find bar over the code. What is typed is found as text, upper and lower case
//! apart or whole words only when asked; the matches in view are lit, the current one
//! more; Enter goes to the next, Shift+Enter to the one before; Replace takes the current
//! one, Replace All every one as one undo step.

use crate::code::{Document, Pos};

/// A script tab's find bar, while it is open.
#[derive(Debug, Default)]
pub(crate) struct Find {
    /// What is looked for.
    pub(crate) query: String,
    /// What replaces it.
    pub(crate) replace: String,
    /// Upper and lower case apart (Alt+C).
    pub(crate) case: bool,
    /// Whole words only (Alt+W).
    pub(crate) word: bool,
    /// The replace field shows (Ctrl+H).
    pub(crate) replacing: bool,
    /// Where finding goes from as the query is typed: the caret when the bar opened, then
    /// the match each step went to.
    pub(crate) origin: Pos,
    /// The matches in the text's order: their starts and ends.
    pub(crate) matches: Vec<(Pos, Pos)>,
    /// The match the last step went to.
    pub(crate) current: Option<usize>,
    /// What the matches were found for: the text's version, the query and the options.
    found_version: Option<u64>,
    found_query: String,
    found_options: (bool, bool),
    /// A line in lower case, while finding.
    lowered: String,
}

impl Find {
    /// The matches found again if the text, the query or an option changed since.
    pub(crate) fn refresh(&mut self, doc: &Document) {
        if self.found_version == Some(doc.version())
            && self.found_query == self.query
            && self.found_options == (self.case, self.word)
        {
            return;
        }
        self.found_version = Some(doc.version());
        self.found_query.clone_from(&self.query);
        self.found_options = (self.case, self.word);
        self.matches.clear();
        self.current = None;
        if self.query.is_empty() || self.query.contains('\n') {
            return;
        }
        let needle = if self.case {
            self.query.clone()
        } else {
            self.query.to_lowercase()
        };
        for line in 0..doc.line_count() {
            let text = doc.line(line);
            // A line whose lower case keeps every character's length is compared in lower
            // case; another (rare: `İ`) as it is.
            let hay = if self.case || !same_lengths(text) {
                text
            } else {
                self.lowered.clear();
                self.lowered
                    .extend(text.chars().flat_map(char::to_lowercase));
                self.lowered.as_str()
            };
            let mut from = 0;
            while let Some(at) = hay[from..].find(&needle) {
                let start = from + at;
                let end = start + needle.len();
                if !self.word || whole_word(text, start, end) {
                    self.matches
                        .push((Pos::new(line, start), Pos::new(line, end)));
                }
                from = end;
            }
        }
    }

    /// The first match at or after `from`, from the text's start after its end; it
    /// becomes the current one.
    pub(crate) fn seek(&mut self, from: Pos) -> Option<(Pos, Pos)> {
        let i = self
            .matches
            .iter()
            .position(|m| m.0 >= from)
            .or_else(|| (!self.matches.is_empty()).then_some(0))?;
        self.current = Some(i);
        Some(self.matches[i])
    }

    /// The match after the primary caret's selection, or before it with `back`, around
    /// the text's ends; it becomes the current one.
    pub(crate) fn step(&mut self, doc: &Document, back: bool) -> Option<(Pos, Pos)> {
        if self.matches.is_empty() {
            self.current = None;
            return None;
        }
        let (a, b) = doc.primary().range();
        let i = if back {
            self.matches
                .iter()
                .rposition(|m| m.1 <= a)
                .unwrap_or(self.matches.len() - 1)
        } else {
            self.matches.iter().position(|m| m.0 >= b).unwrap_or(0)
        };
        self.current = Some(i);
        Some(self.matches[i])
    }

    /// The bar's count: `3 of 17`, `17 found` before a step, `No results`; nothing while
    /// nothing is typed.
    pub(crate) fn count(&self, out: &mut String) {
        use core::fmt::Write as _;
        out.clear();
        if self.query.is_empty() {
            return;
        }
        let _ = match (self.matches.len(), self.current) {
            (0, _) => write!(out, "No results"),
            (n, Some(i)) => write!(out, "{} of {n}", i + 1),
            (n, None) => write!(out, "{n} found"),
        };
    }
}

/// Whether every character of `s` has a lower case of its own length.
fn same_lengths(s: &str) -> bool {
    s.chars()
        .all(|c| c.to_lowercase().map(char::len_utf8).sum::<usize>() == c.len_utf8())
}

/// Whether `text[start..end]` is a whole word there: no letter, digit or `_` touches it.
fn whole_word(text: &str, start: usize, end: usize) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    !text[..start].chars().next_back().is_some_and(word)
        && !text[end..].chars().next().is_some_and(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_by_case_and_whole_words_and_steps_around_the_ends() {
        let doc = Document::new("local Door = door\ndoor.open = DOOR_OPEN\nindoor");
        let mut find = Find {
            query: String::from("door"),
            ..Find::default()
        };
        find.refresh(&doc);
        assert_eq!(find.matches.len(), 5, "any case, inside words too");
        find.case = true;
        find.refresh(&doc);
        assert_eq!(
            find.matches,
            [
                (Pos::new(0, 13), Pos::new(0, 17)),
                (Pos::new(1, 0), Pos::new(1, 4)),
                (Pos::new(2, 2), Pos::new(2, 6)),
            ]
        );
        find.case = false;
        find.word = true;
        find.refresh(&doc);
        assert_eq!(
            find.matches.len(),
            3,
            "`DOOR_OPEN` and `indoor` are other words"
        );
        // Steps from the caret, around the ends.
        let mut doc = doc;
        doc.place(Pos::new(0, 8), false, false);
        assert_eq!(find.step(&doc, false), Some(find.matches[1]));
        doc.select(find.matches[2].0, find.matches[2].1, false);
        assert_eq!(
            find.step(&doc, false),
            Some(find.matches[0]),
            "past the last: the first"
        );
        assert_eq!(find.step(&doc, true), Some(find.matches[1]));
        let mut count = String::new();
        find.count(&mut count);
        assert_eq!(count, "2 of 3");
        find.query = String::from("gate");
        find.refresh(&doc);
        find.count(&mut count);
        assert_eq!(count, "No results");
        // Seeking as the query is typed: the first match from the origin on.
        find.query = String::from("door");
        find.word = false;
        find.refresh(&doc);
        assert_eq!(
            find.seek(Pos::new(1, 2)),
            Some((Pos::new(1, 12), Pos::new(1, 16)))
        );
        assert_eq!(
            find.seek(Pos::new(2, 3)),
            Some(find.matches[0]),
            "around the end"
        );
    }
}
