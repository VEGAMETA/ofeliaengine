//! A script open in the code editor (ADR 0107 E8, `docs/research/ux/code-editor.md`): its
//! text as lines, carets with their selections (several at once, VS Code's way), and edits
//! that undo and redo as steps, typing merged into one step a word at a time.
//!
//! Places are a line and a byte offset in it, always on a character boundary. The view
//! (E8-1b) draws the lines; nothing here knows about fonts or pixels.

/// A place in the text: a line, and a byte offset in it on a character boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) struct Pos {
    pub(crate) line: usize,
    pub(crate) col: usize,
}

impl Pos {
    pub(crate) const fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// A caret, and where its selection started (none when the two are the same).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Caret {
    pub(crate) anchor: Pos,
    pub(crate) head: Pos,
    /// The column (in characters) moving up and down keeps, across shorter lines.
    goal: Option<usize>,
}

impl Caret {
    pub(crate) const fn at(pos: Pos) -> Self {
        Self {
            anchor: pos,
            head: pos,
            goal: None,
        }
    }

    /// Its selection's start and end.
    pub(crate) fn range(&self) -> (Pos, Pos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether it selects nothing.
    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// One replacement: `removed` taken out at `start`, `inserted` put there.
#[derive(Debug, Clone, PartialEq)]
struct Change {
    start: Pos,
    removed: String,
    inserted: String,
}

/// What made a step, for merging: typing and deleting merge with what came just before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Typing,
    Deleting,
    Other,
}

/// An undo step: its changes in the order they were made, and the carets around them.
#[derive(Debug, Clone)]
struct Step {
    changes: Vec<Change>,
    before: Vec<Caret>,
    after: Vec<Caret>,
    kind: Kind,
}

/// A text open for editing.
pub(crate) struct Document {
    lines: Vec<String>,
    /// The carets, in the text's order, their selections apart; the one moved last is the
    /// primary (it scrolls into view).
    carets: Vec<Caret>,
    primary: usize,
    undo: Vec<Step>,
    redo: Vec<Step>,
    /// The undo depth when last saved (none: never this history's state).
    saved: Option<usize>,
    /// Raised by every change: what caches of the lines (highlighting) compare.
    version: u64,
    /// The next step does not merge with the last (a caret moved meanwhile).
    sealed: bool,
    /// What the last copy or cut took as whole lines (carets without selections): pasted
    /// back, it goes in above the carets' lines, as VS Code does.
    lines_copied: Option<String>,
    /// The first line changed since [`Self::take_changed`] last asked (highlighting's).
    changed: Option<usize>,
}

/// What an indentation level is.
pub(crate) const INDENT: &str = "    ";

impl Document {
    /// A document of `text` (`\r\n` read as `\n`), the caret at its start.
    pub(crate) fn new(text: &str) -> Self {
        let lines = text
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned())
            .collect();
        Self {
            lines,
            carets: vec![Caret::default()],
            primary: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            saved: Some(0),
            version: 0,
            sealed: true,
            lines_copied: None,
            changed: None,
        }
    }

    /// The first line changed since this was last asked: the lines from it on hold other
    /// text (and maybe other places) than they did.
    pub(crate) fn take_changed(&mut self) -> Option<usize> {
        self.changed.take()
    }

    /// The text as it is now is not what its file holds (unsaved changes brought back from
    /// an autosave).
    pub(crate) fn mark_unsaved(&mut self) {
        self.saved = None;
    }

    /// The text, lines joined by `\n`.
    pub(crate) fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub(crate) fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub(crate) fn line(&self, i: usize) -> &str {
        self.lines.get(i).map_or("", String::as_str)
    }

    pub(crate) fn carets(&self) -> &[Caret] {
        &self.carets
    }

    /// The caret moved last.
    pub(crate) fn primary(&self) -> Caret {
        self.carets[self.primary.min(self.carets.len() - 1)]
    }

    pub(crate) fn version(&self) -> u64 {
        self.version
    }

    /// Whether the text differs from what was saved last.
    pub(crate) fn is_dirty(&self) -> bool {
        self.saved != Some(self.undo.len())
    }

    /// The text as it is now was saved.
    pub(crate) fn mark_saved(&mut self) {
        self.saved = Some(self.undo.len());
        self.sealed = true;
    }

    /// The text between `a` and `b` (`a` first).
    fn slice(&self, a: Pos, b: Pos) -> String {
        if a.line == b.line {
            return self.lines[a.line][a.col..b.col].to_owned();
        }
        let mut out = String::from(&self.lines[a.line][a.col..]);
        for l in &self.lines[a.line + 1..b.line] {
            out.push('\n');
            out.push_str(l);
        }
        out.push('\n');
        out.push_str(&self.lines[b.line][..b.col]);
        out
    }

    /// Puts `text` in place of `a..b`; where its end lands.
    fn replace(&mut self, a: Pos, b: Pos, text: &str) -> Pos {
        let tail = self.lines[b.line][b.col..].to_owned();
        let head = self.lines[a.line][..a.col].to_owned();
        let mut new: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let last = new.len() - 1;
        let end = Pos::new(
            a.line + last,
            if last == 0 { a.col } else { 0 } + new[last].len(),
        );
        new[0].insert_str(0, &head);
        new[last].push_str(&tail);
        self.lines.splice(a.line..=b.line, new);
        self.version += 1;
        self.changed = Some(self.changed.map_or(a.line, |c| c.min(a.line)));
        end
    }

    /// Where `p` goes after the text up to `b` became text ending at `end`.
    fn shift(p: Pos, b: Pos, end: Pos) -> Pos {
        if p < b {
            return p;
        }
        if p.line == b.line {
            Pos::new(end.line, end.col + (p.col - b.col))
        } else {
            Pos::new(p.line + end.line - b.line, p.col)
        }
    }

    /// Applies one change for each caret, `edit(range, caret)` giving the range to replace
    /// and its text, from the last caret back so the earlier ones' places stay; one undo
    /// step of `kind`.
    fn edit(
        &mut self,
        kind: Kind,
        mut edit: impl FnMut(&Self, &Caret) -> Option<(Pos, Pos, String)>,
    ) {
        let before = self.carets.clone();
        let mut changes = Vec::new();
        let mut carets = self.carets.clone();
        for i in (0..carets.len()).rev() {
            let Some((a, b, text)) = edit(self, &carets[i]) else {
                continue;
            };
            let removed = self.slice(a, b);
            let end = self.replace(a, b, &text);
            carets[i] = Caret::at(end);
            // The carets after this one (already done) move with the text.
            for c in carets.iter_mut().skip(i + 1) {
                c.anchor = Self::shift(c.anchor, b, end);
                c.head = Self::shift(c.head, b, end);
            }
            changes.push(Change {
                start: a,
                removed,
                inserted: text,
            });
        }
        if changes.is_empty() {
            return;
        }
        self.carets = carets;
        self.merge_carets();
        let after = self.carets.clone();
        self.redo.clear();
        let merges = !self.sealed
            && kind != Kind::Other
            && self
                .undo
                .last()
                .is_some_and(|s| s.kind == kind && s.after == before)
            && self.saved != Some(self.undo.len());
        if merges && let Some(last) = self.undo.last_mut() {
            last.changes.extend(changes);
            last.after = after;
        } else {
            self.undo.push(Step {
                changes,
                before,
                after,
                kind,
            });
        }
        self.sealed = false;
    }

    /// Types `text` at every caret, in place of its selection. A space or a line's end
    /// ends the step being typed (VS Code's word-wise undo).
    pub(crate) fn type_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.edit(Kind::Typing, |_, c| {
            let (a, b) = c.range();
            Some((a, b, text.to_owned()))
        });
        if text.contains([' ', '\t', '\n']) {
            self.sealed = true;
        }
    }

    /// A new line at every caret, keeping the line's indentation, one level more after a
    /// line opening a block (`then`, `do`, `function (...)`, `{`, `(`).
    pub(crate) fn newline(&mut self) {
        self.edit(Kind::Other, |doc, c| {
            let (a, b) = c.range();
            let line = &doc.lines[a.line];
            let indent: String = line
                .chars()
                .take_while(|ch| *ch == ' ' || *ch == '\t')
                .collect();
            let before = line[..a.col].trim_end();
            let opens = before.ends_with(" then")
                || before.ends_with(" do")
                || before == "do"
                || before.ends_with(" else")
                || before == "else"
                || before.ends_with('{')
                || before.ends_with('(')
                || (before.contains("function") && before.ends_with(')'))
                || before.ends_with(" repeat")
                || before == "repeat";
            let more = if opens { INDENT } else { "" };
            Some((a, b, format!("\n{indent}{more}")))
        });
        self.sealed = true;
    }

    /// Backspace: each caret's selection, else the character before it (a whole level of
    /// spaces in the indentation).
    pub(crate) fn backspace(&mut self) {
        self.edit(Kind::Deleting, |doc, c| {
            let (a, b) = c.range();
            if a != b {
                return Some((a, b, String::new()));
            }
            if a.col == 0 {
                return (a.line > 0).then(|| {
                    let prev = Pos::new(a.line - 1, doc.lines[a.line - 1].len());
                    (prev, a, String::new())
                });
            }
            let line = &doc.lines[a.line];
            let spaces = line[..a.col]
                .bytes()
                .rev()
                .take_while(|&b| b == b' ')
                .count();
            let level = if spaces == a.col && spaces > 0 {
                (spaces - 1) % INDENT.len() + 1
            } else {
                0
            };
            let from = if level > 0 {
                a.col - level
            } else {
                prev_char(line, a.col)
            };
            Some((Pos::new(a.line, from), a, String::new()))
        });
    }

    /// Ctrl+Backspace (`back`) and Ctrl+Delete: each caret's selection, else up to the
    /// start of the word before it or the end of the word after it.
    pub(crate) fn delete_word(&mut self, back: bool) {
        self.edit(Kind::Deleting, |doc, c| {
            let (a, b) = c.range();
            if a != b {
                return Some((a, b, String::new()));
            }
            let motion = if back {
                Motion::WordLeft
            } else {
                Motion::WordRight
            };
            let to = motion.from(&doc.lines, a, &mut None);
            (to != a).then(|| (a.min(to), a.max(to), String::new()))
        });
    }

    /// Ctrl+Shift+K: the lines the carets touch taken out, each caret on the line that
    /// came up in their place.
    pub(crate) fn delete_lines(&mut self) {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for l in self.caret_lines() {
            match runs.last_mut() {
                Some((_, last)) if *last + 1 == l => *last = l,
                _ => runs.push((l, l)),
            }
        }
        // A caret a run of lines, so each goes once; undo brings the carets back as they
        // were.
        let before = self.carets.clone();
        self.carets = runs
            .iter()
            .map(|&(first, last)| Caret {
                anchor: Pos::new(first, 0),
                head: Pos::new(last, 0),
                goal: None,
            })
            .collect();
        self.edit(Kind::Other, |doc, c| {
            let (first, last) = (c.range().0.line, c.range().1.line);
            let end = |l: usize| Pos::new(l, doc.lines[l].len());
            Some(if last + 1 < doc.lines.len() {
                (Pos::new(first, 0), Pos::new(last + 1, 0), String::new())
            } else if first > 0 {
                (end(first - 1), end(last), String::new())
            } else {
                (Pos::new(0, 0), end(last), String::new())
            })
        });
        if let Some(step) = self.undo.last_mut() {
            step.before = before;
        }
        self.sealed = true;
    }

    /// Delete: each caret's selection, else the character after it.
    pub(crate) fn delete(&mut self) {
        self.edit(Kind::Deleting, |doc, c| {
            let (a, b) = c.range();
            if a != b {
                return Some((a, b, String::new()));
            }
            let line = &doc.lines[a.line];
            if a.col < line.len() {
                return Some((a, Pos::new(a.line, next_char(line, a.col)), String::new()));
            }
            (a.line + 1 < doc.lines.len()).then(|| (a, Pos::new(a.line + 1, 0), String::new()))
        });
    }

    /// The selections' text, one caret's a line (what copying takes); a caret without a
    /// selection takes its whole line, as VS Code does.
    pub(crate) fn selected_text(&self) -> String {
        let mut out = Vec::with_capacity(self.carets.len());
        for c in &self.carets {
            let (a, b) = c.range();
            if a == b {
                out.push(format!("{}\n", self.lines[a.line]));
            } else {
                out.push(self.slice(a, b));
            }
        }
        out.join("\n")
    }

    /// Copy: [`Self::selected_text`], remembered when it is whole lines.
    pub(crate) fn copy(&mut self) -> String {
        let text = self.selected_text();
        self.lines_copied = self
            .carets
            .iter()
            .all(Caret::is_empty)
            .then(|| text.clone());
        text
    }

    /// Cut: the selections taken out (a caret without one: its line).
    pub(crate) fn cut(&mut self) -> String {
        let text = self.copy();
        let count = self.lines.len();
        self.edit(Kind::Other, |doc, c| {
            let (a, b) = c.range();
            if a != b {
                return Some((a, b, String::new()));
            }
            if a.line + 1 < count {
                Some((Pos::new(a.line, 0), Pos::new(a.line + 1, 0), String::new()))
            } else if a.line > 0 {
                let prev = Pos::new(a.line - 1, doc.lines[a.line - 1].len());
                Some((
                    prev,
                    Pos::new(a.line, doc.lines[a.line].len()),
                    String::new(),
                ))
            } else {
                Some((
                    Pos::new(0, 0),
                    Pos::new(0, doc.lines[0].len()),
                    String::new(),
                ))
            }
        });
        self.sealed = true;
        text
    }

    /// Paste: with as many lines as carets, a line at each; else the whole text at each.
    /// Whole lines copied without a selection go in above each caret's line instead, the
    /// caret staying in its own text.
    pub(crate) fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n");
        if self.lines_copied.as_deref() == Some(text.as_str())
            && self.carets.iter().all(Caret::is_empty)
        {
            let columns: Vec<usize> = self.carets.iter().map(|c| c.head.col).collect();
            self.edit(Kind::Other, |_, c| {
                let start = Pos::new(c.head.line, 0);
                Some((start, start, text.clone()))
            });
            // Each caret ended at its line's start, after the lines put in above it.
            if self.carets.len() == columns.len() {
                for (c, col) in self.carets.iter_mut().zip(columns) {
                    *c = Caret::at(Pos::new(c.head.line, col));
                }
                if let Some(step) = self.undo.last_mut() {
                    step.after.clone_from(&self.carets);
                }
            }
            self.sealed = true;
            return;
        }
        let parts: Vec<&str> = text.split('\n').collect();
        let per_caret = self.carets.len() > 1 && parts.len() == self.carets.len();
        let order: Vec<Caret> = self.carets.clone();
        self.edit(Kind::Other, |_, c| {
            let (a, b) = c.range();
            let piece = if per_caret {
                let i = order.iter().position(|o| o == c).unwrap_or(0);
                parts[i].to_owned()
            } else {
                text.clone()
            };
            Some((a, b, piece))
        });
        self.sealed = true;
    }

    /// Takes back the last step; whether there was one.
    pub(crate) fn undo(&mut self) -> bool {
        let Some(step) = self.undo.pop() else {
            return false;
        };
        // The changes were made from the last caret back: undone from the first made, last.
        for change in step.changes.iter().rev() {
            let end = end_of(change.start, &change.inserted);
            self.replace(change.start, end, &change.removed);
        }
        self.carets = step.before.clone();
        self.primary = self.carets.len() - 1;
        self.redo.push(step);
        self.sealed = true;
        true
    }

    /// Does the last step taken back again; whether there was one.
    pub(crate) fn redo(&mut self) -> bool {
        let Some(step) = self.redo.pop() else {
            return false;
        };
        for change in &step.changes {
            let end = end_of(change.start, &change.removed);
            self.replace(change.start, end, &change.inserted);
        }
        self.carets = step.after.clone();
        self.primary = self.carets.len() - 1;
        self.undo.push(step);
        self.sealed = true;
        true
    }

    /// Moves every caret by `motion`, keeping its selection's start with `extend` (Shift),
    /// else dropping it; a selection collapsed by Left or Right goes to its own side.
    pub(crate) fn move_carets(&mut self, motion: Motion, extend: bool) {
        let lines = &self.lines;
        for c in &mut self.carets {
            let (a, b) = c.range();
            let collapse = !extend && !c.is_empty();
            let to = match motion {
                Motion::Left if collapse => a,
                Motion::Right if collapse => b,
                _ => motion.from(lines, c.head, &mut c.goal),
            };
            if !matches!(
                motion,
                Motion::Up | Motion::Down | Motion::PageUp(_) | Motion::PageDown(_)
            ) {
                c.goal = None;
            }
            c.head = to;
            if !extend {
                c.anchor = to;
            }
        }
        self.merge_carets();
        self.sealed = true;
    }

    /// Everything selected, one caret.
    pub(crate) fn select_all(&mut self) {
        let last = self.lines.len() - 1;
        self.carets = vec![Caret {
            anchor: Pos::default(),
            head: Pos::new(last, self.lines[last].len()),
            goal: None,
        }];
        self.primary = 0;
        self.sealed = true;
    }

    /// One caret at `pos` (a click), or a selection to it from where the caret was
    /// (Shift+click); `add` keeps the others (Alt+click).
    pub(crate) fn place(&mut self, pos: Pos, extend: bool, add: bool) {
        let pos = self.clamp(pos);
        if add {
            self.carets.push(Caret::at(pos));
            self.primary = self.carets.len() - 1;
        } else {
            let mut c = self.primary();
            c.head = pos;
            if !extend {
                c.anchor = pos;
            }
            c.goal = None;
            self.carets = vec![c];
            self.primary = 0;
        }
        self.merge_carets();
        self.sealed = true;
    }

    /// A caret more on the line below the last one (`down`) or above the first, at the same
    /// column (Ctrl+Alt+Down, Ctrl+Alt+Up).
    pub(crate) fn add_caret(&mut self, down: bool) {
        let edge = if down {
            self.carets.iter().map(|c| c.head).max()
        } else {
            self.carets.iter().map(|c| c.head).min()
        };
        let Some(edge) = edge else {
            return;
        };
        let line = if down {
            edge.line + 1
        } else {
            match edge.line.checked_sub(1) {
                Some(l) => l,
                None => return,
            }
        };
        if line >= self.lines.len() {
            return;
        }
        let column = char_column(&self.lines[edge.line], edge.col);
        let pos = Pos::new(line, col_at(&self.lines[line], column));
        self.carets.push(Caret::at(pos));
        self.primary = self.carets.len() - 1;
        self.merge_carets();
        self.sealed = true;
    }

    /// Ctrl+D: the word under the primary caret selected, or, with a selection, the next
    /// place holding the same text selected too (from the text's start after its end).
    pub(crate) fn select_next_match(&mut self) {
        let c = self.primary();
        if c.is_empty() {
            let line = &self.lines[c.head.line];
            let (a, b) = word_at(line, c.head.col);
            if a == b {
                return;
            }
            let i = self.primary;
            self.carets[i] = Caret {
                anchor: Pos::new(c.head.line, a),
                head: Pos::new(c.head.line, b),
                goal: None,
            };
            self.sealed = true;
            return;
        }
        let (a, b) = c.range();
        let needle = self.slice(a, b);
        if needle.contains('\n') {
            return;
        }
        let taken = |p: Pos, carets: &[Caret]| carets.iter().any(|c| c.range().0 == p);
        let n = self.lines.len();
        for k in 0..=n {
            let line_index = (b.line + k) % n;
            let line = &self.lines[line_index];
            let from = if k == 0 { b.col } else { 0 };
            let mut search = from;
            while let Some(found) = line[search..].find(&needle) {
                let start = Pos::new(line_index, search + found);
                if !taken(start, &self.carets) {
                    self.carets.push(Caret {
                        anchor: start,
                        head: Pos::new(line_index, start.col + needle.len()),
                        goal: None,
                    });
                    self.primary = self.carets.len() - 1;
                    self.merge_carets();
                    self.sealed = true;
                    return;
                }
                search += found + needle.len().max(1);
                if search > line.len() {
                    break;
                }
            }
        }
    }

    /// One caret selecting from `anchor` to `head` (a double or triple click), or one more
    /// beside the others with `add`.
    pub(crate) fn select(&mut self, anchor: Pos, head: Pos, add: bool) {
        let c = Caret {
            anchor: self.clamp(anchor),
            head: self.clamp(head),
            goal: None,
        };
        if add {
            self.carets.push(c);
            self.primary = self.carets.len() - 1;
        } else {
            self.carets = vec![c];
            self.primary = 0;
        }
        self.merge_carets();
        self.sealed = true;
    }

    /// The carets selecting each of `ranges` (starts and ends, in the text's order), the
    /// one at `primary` the primary: every match found at once.
    pub(crate) fn select_ranges(&mut self, ranges: &[(Pos, Pos)], primary: usize) {
        if ranges.is_empty() {
            return;
        }
        self.carets = ranges
            .iter()
            .map(|&(a, b)| Caret {
                anchor: self.clamp(a),
                head: self.clamp(b),
                goal: None,
            })
            .collect();
        self.primary = primary.min(self.carets.len() - 1);
        self.merge_carets();
        self.sealed = true;
    }

    /// `text` in place of each caret's selection, one undo step of its own (a
    /// replacement).
    pub(crate) fn insert(&mut self, text: &str) {
        self.sealed = true;
        self.edit(Kind::Other, |_, c| {
            let (a, b) = c.range();
            Some((a, b, text.to_owned()))
        });
        self.sealed = true;
    }

    /// The word at `pos`: its start and end (both `pos` off a word).
    pub(crate) fn word_range(&self, pos: Pos) -> (Pos, Pos) {
        let pos = self.clamp(pos);
        let (a, b) = word_at(&self.lines[pos.line], pos.col);
        (Pos::new(pos.line, a), Pos::new(pos.line, b))
    }

    /// The primary caret's end moved to `pos`, its start and the other carets kept: the
    /// mouse dragged.
    pub(crate) fn drag_to(&mut self, pos: Pos) {
        let pos = self.clamp(pos);
        let i = self.primary.min(self.carets.len() - 1);
        self.carets[i].head = pos;
        self.carets[i].goal = None;
        self.merge_carets();
        self.sealed = true;
    }

    /// Escape: the primary caret alone, its selection kept.
    pub(crate) fn single_caret(&mut self) {
        let c = self.primary();
        self.carets = vec![c];
        self.primary = 0;
    }

    /// The lines the carets touch, each once, in order.
    fn caret_lines(&self) -> Vec<usize> {
        let mut lines: Vec<usize> = Vec::new();
        for c in &self.carets {
            let (a, b) = c.range();
            // A selection ending at a line's start leaves that line out.
            let last = if b.col == 0 && b.line > a.line {
                b.line - 1
            } else {
                b.line
            };
            for l in a.line..=last {
                if !lines.contains(&l) {
                    lines.push(l);
                }
            }
        }
        lines.sort_unstable();
        lines
    }

    /// Tab: with a selection across lines, those lines a level in; else spaces up to the
    /// next level at each caret.
    pub(crate) fn indent(&mut self) {
        let across = self
            .carets
            .iter()
            .any(|c| c.range().0.line != c.range().1.line);
        if !across {
            self.edit(Kind::Typing, |doc, c| {
                let (a, b) = c.range();
                let column = char_column(&doc.lines[a.line], a.col);
                let n = INDENT.len() - column % INDENT.len();
                Some((a, b, " ".repeat(n)))
            });
            self.sealed = true;
            return;
        }
        self.indent_lines();
    }

    /// Ctrl+]: the carets' lines a level in.
    pub(crate) fn indent_lines(&mut self) {
        self.each_line(|line| format!("{INDENT}{line}"));
    }

    /// Shift+Tab: the carets' lines a level out.
    pub(crate) fn outdent(&mut self) {
        self.each_line(|line| {
            let spaces = line
                .bytes()
                .take_while(|&b| b == b' ')
                .count()
                .min(INDENT.len());
            let cut = if spaces == 0 && line.starts_with('\t') {
                1
            } else {
                spaces
            };
            line[cut..].to_owned()
        });
    }

    /// Ctrl+/: the carets' lines commented out with `-- `, or back when all are.
    pub(crate) fn toggle_comment(&mut self) {
        let lines = self.caret_lines();
        let all = lines.iter().all(|&l| {
            self.lines[l].trim_start().starts_with("--") || self.lines[l].trim().is_empty()
        });
        if all {
            self.each_line(|line| {
                let at = line.len() - line.trim_start().len();
                let rest = &line[at..];
                let rest = rest
                    .strip_prefix("-- ")
                    .or_else(|| rest.strip_prefix("--"))
                    .unwrap_or(rest);
                format!("{}{rest}", &line[..at])
            });
        } else {
            let depth = lines
                .iter()
                .filter(|&&l| !self.lines[l].trim().is_empty())
                .map(|&l| self.lines[l].len() - self.lines[l].trim_start().len())
                .min()
                .unwrap_or(0);
            self.each_line(|line| {
                if line.trim().is_empty() {
                    line.to_owned()
                } else {
                    format!("{}-- {}", &line[..depth], &line[depth..])
                }
            });
        }
    }

    /// Rewrites each line the carets touch with `f`, one step, the carets kept on their
    /// lines (their columns moved with the line's start).
    fn each_line(&mut self, mut f: impl FnMut(&str) -> String) {
        let lines = self.caret_lines();
        let before = self.carets.clone();
        let mut changes = Vec::new();
        for &l in lines.iter().rev() {
            let old = self.lines[l].clone();
            let new = f(&old);
            if new == old {
                continue;
            }
            let delta = new.len() as isize - old.len() as isize;
            self.replace(Pos::new(l, 0), Pos::new(l, old.len()), &new);
            for c in &mut self.carets {
                for p in [&mut c.anchor, &mut c.head] {
                    if p.line == l {
                        p.col = ((p.col as isize + delta).max(0) as usize).min(new.len());
                        while !new.is_char_boundary(p.col) {
                            p.col -= 1;
                        }
                    }
                }
            }
            changes.push(Change {
                start: Pos::new(l, 0),
                removed: old,
                inserted: new,
            });
        }
        if changes.is_empty() {
            return;
        }
        self.redo.clear();
        let after = self.carets.clone();
        self.undo.push(Step {
            changes,
            before,
            after,
            kind: Kind::Other,
        });
        self.sealed = true;
    }

    /// Alt+Up and Alt+Down: the carets' lines moved past the line above or below.
    pub(crate) fn move_lines(&mut self, down: bool) {
        let lines = self.caret_lines();
        let (Some(&first), Some(&last)) = (lines.first(), lines.last()) else {
            return;
        };
        if (down && last + 1 >= self.lines.len()) || (!down && first == 0) {
            return;
        }
        let before = self.carets.clone();
        let (a, b) = if down {
            (
                Pos::new(first, 0),
                Pos::new(last + 1, self.lines[last + 1].len()),
            )
        } else {
            (
                Pos::new(first - 1, 0),
                Pos::new(last, self.lines[last].len()),
            )
        };
        let removed = self.slice(a, b);
        let block: Vec<String> = self.lines[first..=last].to_vec();
        let other = if down {
            self.lines[last + 1].clone()
        } else {
            self.lines[first - 1].clone()
        };
        let inserted = if down {
            format!("{other}\n{}", block.join("\n"))
        } else {
            format!("{}\n{other}", block.join("\n"))
        };
        self.replace(a, b, &inserted);
        for c in &mut self.carets {
            for p in [&mut c.anchor, &mut c.head] {
                if down {
                    p.line += 1;
                } else {
                    p.line -= 1;
                }
            }
        }
        self.redo.clear();
        let after = self.carets.clone();
        self.undo.push(Step {
            changes: vec![Change {
                start: a,
                removed,
                inserted,
            }],
            before,
            after,
            kind: Kind::Other,
        });
        self.sealed = true;
    }

    /// `pos` within the text: its line clamped, its column to the line's end and a
    /// character's boundary.
    pub(crate) fn clamp(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.lines.len() - 1);
        let text = &self.lines[line];
        let mut col = pos.col.min(text.len());
        while !text.is_char_boundary(col) {
            col -= 1;
        }
        Pos::new(line, col)
    }

    /// The carets in the text's order, those that overlap or meet merged into one.
    fn merge_carets(&mut self) {
        let primary = self.carets.get(self.primary).copied();
        self.carets.sort_by_key(|c| c.range().0);
        let mut merged: Vec<Caret> = Vec::with_capacity(self.carets.len());
        for c in self.carets.drain(..) {
            if let Some(last) = merged.last_mut() {
                let (la, lb) = last.range();
                let (ca, cb) = c.range();
                if ca < lb || (ca == lb && (c.is_empty() || last.is_empty())) {
                    let (a, b) = (la.min(ca), lb.max(cb));
                    let forward = last.head >= last.anchor;
                    *last = if forward {
                        Caret {
                            anchor: a,
                            head: b,
                            goal: None,
                        }
                    } else {
                        Caret {
                            anchor: b,
                            head: a,
                            goal: None,
                        }
                    };
                    continue;
                }
            }
            merged.push(c);
        }
        self.carets = merged;
        self.primary = primary
            .and_then(|p| {
                self.carets
                    .iter()
                    .position(|c| c.range().0 <= p.head && p.head <= c.range().1)
            })
            .unwrap_or(self.carets.len() - 1);
    }
}

/// How a caret moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Motion {
    Left,
    Right,
    /// To the start of the word before (Ctrl+Left).
    WordLeft,
    /// To the end of the word after (Ctrl+Right).
    WordRight,
    Up,
    Down,
    /// Home: the line's first character that is not a space, then its start.
    Home,
    End,
    PageUp(usize),
    PageDown(usize),
    /// Ctrl+Home, Ctrl+End.
    Start,
    Finish,
}

impl Motion {
    /// Where a caret at `p` goes; `goal` is the column kept up and down.
    fn from(self, lines: &[String], p: Pos, goal: &mut Option<usize>) -> Pos {
        let line = &lines[p.line];
        let vertical = |to: usize, goal: &mut Option<usize>| {
            let column = *goal.get_or_insert_with(|| char_column(line, p.col));
            Pos::new(to, col_at(&lines[to], column))
        };
        match self {
            Self::Left if p.col > 0 => Pos::new(p.line, prev_char(line, p.col)),
            Self::Left if p.line > 0 => Pos::new(p.line - 1, lines[p.line - 1].len()),
            Self::Left => p,
            Self::Right if p.col < line.len() => Pos::new(p.line, next_char(line, p.col)),
            Self::Right if p.line + 1 < lines.len() => Pos::new(p.line + 1, 0),
            Self::Right => p,
            Self::WordLeft if p.col == 0 && p.line > 0 => {
                Pos::new(p.line - 1, lines[p.line - 1].len())
            }
            Self::WordLeft => Pos::new(p.line, word_left(line, p.col)),
            Self::WordRight if p.col == line.len() && p.line + 1 < lines.len() => {
                Pos::new(p.line + 1, 0)
            }
            Self::WordRight => Pos::new(p.line, word_right(line, p.col)),
            Self::Up if p.line > 0 => vertical(p.line - 1, goal),
            Self::Up => Pos::new(0, 0),
            Self::Down if p.line + 1 < lines.len() => vertical(p.line + 1, goal),
            Self::Down => Pos::new(p.line, line.len()),
            Self::PageUp(n) => vertical(p.line.saturating_sub(n), goal),
            Self::PageDown(n) => vertical((p.line + n).min(lines.len() - 1), goal),
            Self::Home => {
                let first = line.len() - line.trim_start().len();
                Pos::new(p.line, if p.col == first { 0 } else { first })
            }
            Self::End => Pos::new(p.line, line.len()),
            Self::Start => Pos::new(0, 0),
            Self::Finish => Pos::new(lines.len() - 1, lines[lines.len() - 1].len()),
        }
    }
}

/// Where `text` ends when put at `start`.
fn end_of(start: Pos, text: &str) -> Pos {
    match text.rfind('\n') {
        None => Pos::new(start.line, start.col + text.len()),
        Some(i) => Pos::new(start.line + text.matches('\n').count(), text.len() - i - 1),
    }
}

fn prev_char(s: &str, col: usize) -> usize {
    s[..col].char_indices().next_back().map_or(0, |(i, _)| i)
}

fn next_char(s: &str, col: usize) -> usize {
    s[col..].chars().next().map_or(col, |c| col + c.len_utf8())
}

/// The columns a tab reaches the next multiple of, on screen (VS Code's default).
pub(crate) const TAB: usize = 4;

/// The column where `c` ends when it starts at `column`: a tab's at the next multiple of
/// [`TAB`], another character's one further.
pub(crate) fn next_column(column: usize, c: char) -> usize {
    if c == '\t' {
        (column / TAB + 1) * TAB
    } else {
        column + 1
    }
}

/// The column on screen of byte offset `col`: a column a character, a tab up to the next
/// multiple of [`TAB`].
pub(crate) fn char_column(s: &str, col: usize) -> usize {
    s[..col].chars().fold(0, next_column)
}

/// The byte offset at column `column` on screen: the character there (a tab spanning it:
/// the tab), the line's end past it.
pub(crate) fn col_at(s: &str, column: usize) -> usize {
    let mut at = 0;
    for (i, c) in s.char_indices() {
        let next = next_column(at, c);
        if next > column {
            return i;
        }
        at = next;
    }
    s.len()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The word at `col`: its start and end (both `col` without one).
fn word_at(s: &str, col: usize) -> (usize, usize) {
    let start = s[..col]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_word(c))
        .last()
        .map_or(col, |(i, _)| i);
    let end = s[col..]
        .char_indices()
        .take_while(|&(_, c)| is_word(c))
        .last()
        .map_or(col, |(i, c)| col + i + c.len_utf8());
    (start, end)
}

/// The start of the word before `col` (spaces skipped first).
fn word_left(s: &str, col: usize) -> usize {
    let before: Vec<(usize, char)> = s[..col].char_indices().collect();
    let mut k = before.len();
    while k > 0 && before[k - 1].1.is_whitespace() {
        k -= 1;
    }
    let word = k > 0 && is_word(before[k - 1].1);
    while k > 0 && !before[k - 1].1.is_whitespace() && is_word(before[k - 1].1) == word {
        k -= 1;
    }
    before
        .get(k)
        .map_or(if k == 0 { 0 } else { col }, |&(i, _)| i)
}

/// The end of the word after `col` (spaces skipped first).
fn word_right(s: &str, col: usize) -> usize {
    let after: Vec<(usize, char)> = s[col..].char_indices().collect();
    let mut k = 0;
    while k < after.len() && after[k].1.is_whitespace() {
        k += 1;
    }
    let word = k < after.len() && is_word(after[k].1);
    while k < after.len() && !after[k].1.is_whitespace() && is_word(after[k].1) == word {
        k += 1;
    }
    after.get(k).map_or(s.len(), |&(i, _)| col + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(doc: &mut Document, line: usize, col: usize) {
        doc.place(Pos::new(line, col), false, false);
    }

    #[test]
    fn typing_merges_word_by_word_and_undo_takes_it_back() {
        let mut doc = Document::new("local x = 1\r\nreturn x");
        assert_eq!(doc.line_count(), 2);
        assert_eq!(doc.line(0), "local x = 1", "CRLF read as LF");
        assert!(!doc.is_dirty());
        at(&mut doc, 1, 8);
        for ch in ["+", " ", "2"] {
            doc.type_text(ch);
        }
        assert_eq!(doc.line(1), "return x+ 2");
        assert!(doc.is_dirty());
        // The space ended the first word's step.
        assert!(doc.undo());
        assert_eq!(doc.line(1), "return x+ ");
        assert!(doc.undo());
        assert_eq!(doc.line(1), "return x");
        assert!(!doc.is_dirty(), "back to what was saved");
        assert!(doc.redo());
        assert!(doc.redo());
        assert_eq!(doc.line(1), "return x+ 2");
        doc.mark_saved();
        assert!(!doc.is_dirty());
        // A move between keys starts a new step.
        doc.type_text("3");
        doc.move_carets(Motion::Left, false);
        doc.type_text("4");
        assert_eq!(doc.line(1), "return x+ 243");
        assert!(doc.undo());
        assert_eq!(doc.line(1), "return x+ 23");
    }

    #[test]
    fn newlines_keep_and_open_indentation_and_backspace_takes_a_level() {
        let mut doc = Document::new("if near then");
        at(&mut doc, 0, 12);
        doc.newline();
        assert_eq!(doc.line(1), INDENT);
        doc.type_text("open()");
        doc.newline();
        assert_eq!(doc.line(2), INDENT, "the same depth again");
        doc.backspace();
        assert_eq!(doc.line(2), "", "a whole level at once");
        doc.backspace();
        assert_eq!(doc.line_count(), 2, "then the line break");
        assert_eq!(doc.primary().head, Pos::new(1, 10));
        doc.delete();
        assert_eq!(doc.line_count(), 2, "nothing after the end");
        assert_eq!(doc.text(), "if near then\n    open()");
    }

    #[test]
    fn carets_move_by_characters_words_and_lines() {
        let mut doc = Document::new("local speed = 2.5\nx\nfunction Door.on_tick(self, dt)");
        at(&mut doc, 0, 0);
        doc.move_carets(Motion::WordRight, false);
        assert_eq!(doc.primary().head, Pos::new(0, 5));
        doc.move_carets(Motion::WordRight, false);
        assert_eq!(doc.primary().head, Pos::new(0, 11));
        doc.move_carets(Motion::End, false);
        doc.move_carets(Motion::Down, false);
        assert_eq!(
            doc.primary().head,
            Pos::new(1, 1),
            "a shorter line: its end"
        );
        assert_eq!(doc.take_changed(), None, "moving changes no line");
        doc.move_carets(Motion::Down, false);
        assert_eq!(
            doc.primary().head,
            Pos::new(2, 17),
            "the column kept from above"
        );
        doc.move_carets(Motion::WordLeft, true);
        let c = doc.primary();
        assert_eq!((c.anchor, c.head), (Pos::new(2, 17), Pos::new(2, 14)));
        doc.move_carets(Motion::Left, false);
        assert_eq!(
            doc.primary().head,
            Pos::new(2, 14),
            "a selection collapses to its side"
        );
        // Home: the first character, then the line's start.
        let mut doc = Document::new("    return 1");
        at(&mut doc, 0, 8);
        doc.move_carets(Motion::Home, false);
        assert_eq!(doc.primary().head, Pos::new(0, 4));
        doc.move_carets(Motion::Home, false);
        assert_eq!(doc.primary().head, Pos::new(0, 0));
        doc.move_carets(Motion::Finish, false);
        assert_eq!(doc.primary().head, Pos::new(0, 12));
        // Up and down keep the column on screen, a tab to the next multiple of four.
        let mut doc = Document::new("\tx = 1\n        y");
        at(&mut doc, 0, 1);
        doc.move_carets(Motion::Down, false);
        assert_eq!(doc.primary().head, Pos::new(1, 4));
        assert_eq!(char_column("a\tb", 2), 4);
        assert_eq!(col_at("a\tb", 4), 2);
        assert_eq!(col_at("a\tb", 3), 1, "inside the tab: before it");
        // Characters of several bytes are stepped over whole.
        let mut doc = Document::new("жзи");
        at(&mut doc, 0, 0);
        doc.move_carets(Motion::Right, false);
        assert_eq!(doc.primary().head, Pos::new(0, 2));
        doc.backspace();
        assert_eq!(doc.line(0), "зи");
    }

    #[test]
    fn several_carets_edit_together_and_undo_as_one() {
        let mut doc = Document::new("door.open\ndoor.close\ndoor.lock");
        at(&mut doc, 0, 0);
        doc.select_next_match();
        assert_eq!(doc.selected_text(), "door");
        doc.select_next_match();
        doc.select_next_match();
        assert_eq!(doc.carets().len(), 3);
        doc.type_text("gate");
        assert_eq!(doc.text(), "gate.open\ngate.close\ngate.lock");
        assert_eq!(doc.take_changed(), Some(0), "the first line edited");
        assert_eq!(doc.take_changed(), None);
        assert!(doc.undo());
        assert_eq!(doc.text(), "door.open\ndoor.close\ndoor.lock", "one step");
        // Carets added below type at the same column.
        let mut doc = Document::new("a = 1\nb = 2\nc = 3");
        at(&mut doc, 0, 1);
        doc.add_caret(true);
        doc.add_caret(true);
        doc.type_text("x");
        assert_eq!(doc.text(), "ax = 1\nbx = 2\ncx = 3");
        doc.single_caret();
        assert_eq!(doc.carets().len(), 1);
        // Copy and paste a line each.
        let mut doc = Document::new("one\ntwo");
        at(&mut doc, 0, 3);
        doc.add_caret(true);
        doc.paste("1\n2");
        assert_eq!(doc.text(), "one1\ntwo2");
    }

    #[test]
    fn lines_indent_comment_move_and_cut() {
        let mut doc = Document::new("a()\nb()\nc()");
        doc.select_all();
        doc.indent();
        assert_eq!(doc.text(), "    a()\n    b()\n    c()");
        doc.outdent();
        assert_eq!(doc.text(), "a()\nb()\nc()");
        doc.toggle_comment();
        assert_eq!(doc.text(), "-- a()\n-- b()\n-- c()");
        doc.toggle_comment();
        assert_eq!(doc.text(), "a()\nb()\nc()");
        // Tab without a selection: spaces to the next level.
        at(&mut doc, 0, 1);
        doc.indent();
        assert_eq!(doc.line(0), "a   ()");
        assert!(doc.undo());
        // Alt+Down moves the line, the caret with it.
        at(&mut doc, 0, 1);
        doc.move_lines(true);
        assert_eq!(doc.text(), "b()\na()\nc()");
        assert_eq!(doc.primary().head, Pos::new(1, 1));
        assert!(doc.undo());
        assert_eq!(doc.text(), "a()\nb()\nc()");
        // Cut without a selection takes the line; pasted, it goes back above the caret's.
        at(&mut doc, 1, 0);
        let cut = doc.cut();
        assert_eq!(cut, "b()\n");
        assert_eq!(doc.text(), "a()\nc()");
        at(&mut doc, 1, 2);
        doc.paste(&cut);
        assert_eq!(doc.text(), "a()\nb()\nc()");
        assert_eq!(
            doc.primary().head,
            Pos::new(2, 2),
            "the caret stays in its text"
        );
        // Text copied from a selection goes in at the caret.
        doc.select(Pos::new(0, 0), Pos::new(0, 1), false);
        let a = doc.copy();
        at(&mut doc, 2, 3);
        doc.paste(&a);
        assert_eq!(doc.line(2), "c()a");
    }

    #[test]
    fn every_match_replaced_at_once_is_one_step() {
        let mut doc = Document::new("door = door + door");
        let ranges = [
            (Pos::new(0, 0), Pos::new(0, 4)),
            (Pos::new(0, 7), Pos::new(0, 11)),
            (Pos::new(0, 14), Pos::new(0, 18)),
        ];
        doc.select_ranges(&ranges, 1);
        assert_eq!(doc.carets().len(), 3);
        assert_eq!(doc.primary().range(), ranges[1]);
        doc.insert("gate");
        assert_eq!(doc.text(), "gate = gate + gate");
        assert!(doc.undo());
        assert_eq!(doc.text(), "door = door + door", "one step");
    }

    #[test]
    fn whole_lines_and_words_go_at_once() {
        let mut doc = Document::new("one\ntwo\nthree\nfour");
        doc.select(Pos::new(1, 1), Pos::new(2, 2), false);
        doc.delete_lines();
        assert_eq!(doc.text(), "one\nfour");
        assert_eq!(doc.primary().head, Pos::new(1, 0));
        assert!(doc.undo());
        assert_eq!(doc.text(), "one\ntwo\nthree\nfour");
        let c = doc.primary();
        assert_eq!(
            (c.anchor, c.head),
            (Pos::new(1, 1), Pos::new(2, 2)),
            "the selection back as it was"
        );
        // The last line takes the line break before it.
        at(&mut doc, 3, 1);
        doc.delete_lines();
        assert_eq!(doc.text(), "one\ntwo\nthree");
        // Ctrl+Backspace and Ctrl+Delete by words.
        let mut doc = Document::new("local door_speed = 4");
        at(&mut doc, 0, 16);
        doc.delete_word(true);
        assert_eq!(doc.line(0), "local  = 4");
        doc.delete_word(false);
        assert_eq!(doc.line(0), "local  4", "the space and the `=` after it");
        // A double click's word, a drag's selection.
        let (a, b) = doc.word_range(Pos::new(0, 2));
        assert_eq!((a, b), (Pos::new(0, 0), Pos::new(0, 5)));
        doc.select(a, b, false);
        doc.drag_to(Pos::new(0, 8));
        assert_eq!(doc.selected_text(), "local  4");
    }

    /// The line array against ADR 0107's question (a rope or a gap buffer, by
    /// measurement): `cargo test --release -p engine_editor --lib measure_long_files --
    /// --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    #[allow(clippy::print_stdout)]
    fn measure_long_files() {
        use std::time::Instant;
        for lines in [10_000, 100_000] {
            let text: String = (0..lines)
                .map(|i| format!("    local value_{i} = compute(self, {i}) * 2 -- a comment\n"))
                .collect();
            let t = Instant::now();
            let mut doc = Document::new(&text);
            let read = t.elapsed();
            at(&mut doc, lines / 2, 10);
            let t = Instant::now();
            for _ in 0..1000 {
                doc.type_text("x");
            }
            let typing = t.elapsed() / 1000;
            let t = Instant::now();
            for _ in 0..1000 {
                doc.newline();
            }
            let newline = t.elapsed() / 1000;
            let t = Instant::now();
            while doc.undo() {}
            let undo = t.elapsed();
            let t = Instant::now();
            let saved = doc.text();
            let join = t.elapsed();
            assert_eq!(saved.len(), text.len());
            println!(
                "{lines} lines ({} KB): read {read:?}, a key {typing:?}, a new line \
                 {newline:?}, undo of 2000 edits {undo:?}, the text for saving {join:?}",
                text.len() / 1024
            );
        }
    }
}
