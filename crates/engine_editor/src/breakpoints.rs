//! The script debugger's breakpoints as the editor keeps them (ADR 0119 E9-1a-2,
//! `docs/research/ux/script-debugger.md` §1, §2 and §8): on lines of the project's
//! scripts, following their lines as the text is edited, kept in the session (never in
//! scripts or scenes), and given to the player's script host on the lines of the code that
//! runs: a script's unsaved edits move its marks, not the code the host has.

use crate::code::{Document, Move, follow_start};
use engine_app::scripts::{
    BreakAction, BreakState, Breakpoint, ErrorStops, Hits, ScriptDebugger, module_name,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// What a breakpoint does when a script reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Act {
    /// Stops the script at its line, the game in the middle of its tick, until F5 (●).
    Stop,
    /// Logs its text and the script goes on: a logpoint (◆).
    Log,
    /// Pauses the game after the tick it is reached in: a pause point (‖).
    Pause,
}

/// A breakpoint on a line of a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mark {
    /// Its line in the editor's text (from 0).
    pub(crate) line: usize,
    pub(crate) act: Act,
    /// A logpoint's text, with Luau's interpolation (`speed = {self.speed}`).
    pub(crate) log: String,
    /// A Luau expression in the line's scope: acted on only when it is true. Empty: none.
    pub(crate) condition: String,
    /// The hits it acts on, as typed: empty for every one, `10` (the tenth), `>= 10`,
    /// `% 64` (every 64th).
    pub(crate) hits: String,
    /// Off: kept, never reached.
    pub(crate) enabled: bool,
}

impl Mark {
    /// A breakpoint on `line` that does `act`, on, with no text, condition or hit rule.
    pub(crate) fn new(line: usize, act: Act) -> Self {
        Self {
            line,
            act,
            log: String::new(),
            condition: String::new(),
            hits: String::new(),
            enabled: true,
        }
    }

    /// Whether a condition or a hit rule limits it: the bar in its mark.
    pub(crate) fn limited(&self) -> bool {
        !self.condition.trim().is_empty() || !self.hits.trim().is_empty()
    }

    /// The breakpoint the script host is given, on line `line` (from 1) of the code that
    /// runs.
    ///
    /// # Errors
    ///
    /// Its hit rule does not read ([`Hits::parse`]).
    pub(crate) fn to_host(&self, line: u32) -> Result<Breakpoint, String> {
        let condition = self.condition.trim();
        Ok(Breakpoint {
            line,
            action: match self.act {
                Act::Stop => BreakAction::Stop,
                Act::Log => BreakAction::Log(self.log.clone()),
                Act::Pause => BreakAction::Pause,
            },
            condition: (!condition.is_empty()).then(|| condition.to_owned()),
            hits: Hits::parse(&self.hits)?,
            enabled: self.enabled,
        })
    }
}

/// Every script's breakpoints.
#[derive(Debug, Default)]
pub(crate) struct Breakpoints {
    /// By script file (relative to the project); each file's by line, one a line.
    files: BTreeMap<PathBuf, Vec<Mark>>,
    /// Files whose breakpoints the host does not have as they are now.
    stale: Vec<PathBuf>,
    /// What "Remove All Breakpoints" took, for "Restore Removed Breakpoints".
    removed: Vec<(PathBuf, Vec<Mark>)>,
    /// Raised at every change: what the Debugger panel's list is made again for.
    revision: u64,
}

impl Breakpoints {
    /// File `file`'s breakpoints, by line.
    pub(crate) fn of(&self, file: &Path) -> &[Mark] {
        self.files.get(file).map_or(&[], Vec::as_slice)
    }

    /// The breakpoint on line `line` of `file`.
    pub(crate) fn at(&self, file: &Path, line: usize) -> Option<&Mark> {
        self.of(file).iter().find(|m| m.line == line)
    }

    /// Every file that has breakpoints, and theirs.
    pub(crate) fn files(&self) -> impl Iterator<Item = (&Path, &[Mark])> {
        self.files.iter().map(|(f, m)| (f.as_path(), m.as_slice()))
    }

    /// Puts `mark` in `file`, in place of the one on its line.
    pub(crate) fn put(&mut self, file: &Path, mark: Mark) {
        let marks = self.files.entry(file.to_path_buf()).or_default();
        match marks.binary_search_by_key(&mark.line, |m| m.line) {
            Ok(k) => marks[k] = mark,
            Err(k) => marks.insert(k, mark),
        }
        self.touch(file);
    }

    /// Takes the breakpoint on line `line` of `file` away.
    pub(crate) fn remove(&mut self, file: &Path, line: usize) -> Option<Mark> {
        let marks = self.files.get_mut(file)?;
        let k = marks.iter().position(|m| m.line == line)?;
        let mark = marks.remove(k);
        if marks.is_empty() {
            self.files.remove(file);
        }
        self.touch(file);
        Some(mark)
    }

    /// Every breakpoint taken away, kept for [`Self::restore_removed`]: how many.
    pub(crate) fn remove_all(&mut self) -> usize {
        let files = std::mem::take(&mut self.files);
        let n = files.values().map(Vec::len).sum();
        for file in files.keys() {
            self.touch(file);
        }
        self.removed = files.into_iter().collect();
        n
    }

    /// What [`Self::remove_all`] took, back where it was (beside any set since): how
    /// many.
    pub(crate) fn restore_removed(&mut self) -> usize {
        let removed = std::mem::take(&mut self.removed);
        let mut n = 0;
        for (file, marks) in removed {
            for mark in marks {
                if self.at(&file, mark.line).is_none() {
                    self.put(&file, mark);
                    n += 1;
                }
            }
        }
        n
    }

    /// `file`'s breakpoints moved with its text's edits (`Document::take_moves`). Two
    /// brought to one line: the one whose text is there stays, not the one whose line was
    /// taken out.
    pub(crate) fn follow(&mut self, file: &Path, moves: &[Move]) {
        let Some(marks) = self.files.get_mut(file) else {
            return;
        };
        let mut moved = false;
        for &m in moves {
            let mut followed: Vec<(bool, Mark)> = marks
                .drain(..)
                .map(|mark| {
                    let (line, gone) = follow_start(mark.line, m);
                    moved |= line != mark.line;
                    (gone, Mark { line, ..mark })
                })
                .collect();
            // Lines only keep their order or meet: the one that kept its text first.
            followed.sort_by_key(|(gone, mark)| (mark.line, *gone));
            followed.dedup_by(|b, a| a.1.line == b.1.line);
            marks.extend(followed.into_iter().map(|(_, mark)| mark));
        }
        if marks.is_empty() {
            self.files.remove(file);
        }
        if moved {
            self.touch(file);
        }
    }

    /// `file`'s breakpoints moved to the lines `line_of` gives them, those it gives none
    /// dropped: a script closed without its unsaved edits goes back to its file's lines.
    pub(crate) fn remap(&mut self, file: &Path, line_of: impl Fn(usize) -> Option<usize>) {
        let Some(marks) = self.files.remove(file) else {
            return;
        };
        for mut mark in marks {
            if let Some(line) = line_of(mark.line) {
                mark.line = line;
                if self.at(file, line).is_none() {
                    self.put(file, mark);
                }
            }
        }
        self.touch(file);
    }

    /// Raised at every change.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// `file`'s breakpoints for the host from now on: given at its next turn.
    pub(crate) fn touch(&mut self, file: &Path) {
        self.revision += 1;
        if !self.stale.iter().any(|f| f == file) {
            self.stale.push(file.to_path_buf());
        }
    }

    /// Every file's breakpoints for the host again (a new script host, a new Play).
    pub(crate) fn touch_all(&mut self) {
        let files: Vec<PathBuf> = self.files.keys().cloned().collect();
        for file in files {
            self.touch(&file);
        }
    }

    /// Whether the host has breakpoints to be given.
    pub(crate) fn has_stale(&self) -> bool {
        !self.stale.is_empty()
    }

    /// The files whose breakpoints the host is to be given now.
    pub(crate) fn take_stale(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.stale)
    }

    /// `file`'s breakpoints as the host is given them: on the lines of the code that runs,
    /// which is the text as last saved where the script is open (`doc`) with unsaved
    /// edits. One on a line made since waits for the save; one whose hit rule does not read
    /// is left out.
    pub(crate) fn for_host(&self, file: &Path, doc: Option<&Document>) -> Vec<Breakpoint> {
        self.of(file)
            .iter()
            .filter_map(|m| {
                let line = match doc {
                    Some(doc) => doc.saved_line(m.line)?,
                    None => m.line,
                };
                m.to_host(u32::try_from(line + 1).ok()?).ok()
            })
            .collect()
    }

    /// A line of `file`'s code where the host placed a breakpoint asked for another: a
    /// comment's or a blank line's goes to the next line with code (Luau's rule), and its
    /// mark with it. `asked` and `placed` are lines of the editor's text (from 0).
    pub(crate) fn placed_on(&mut self, file: &Path, asked: usize, placed: usize) {
        if asked == placed || self.at(file, placed).is_some() {
            return;
        }
        if let Some(mut mark) = self.remove(file, asked) {
            mark.line = placed;
            self.put(file, mark);
        }
    }
}

/// A breakpoint's options being edited under its line (§8): what it does, a logpoint's
/// text, its condition and hit rule. Enter keeps them, Escape drops them.
#[derive(Debug, Clone)]
pub(crate) struct BreakEdit {
    pub(crate) file: PathBuf,
    pub(crate) line: usize,
    pub(crate) act: Act,
    pub(crate) log: String,
    pub(crate) condition: String,
    pub(crate) hits: String,
    enabled: bool,
    /// No breakpoint was there: dropped, none is left.
    pub(crate) new: bool,
    /// The field given the keys at the next frame: 0 the text, 1 the condition, 2 the hits.
    pub(crate) focus: Option<usize>,
    /// Why Enter did not keep it; empty otherwise.
    pub(crate) error: String,
}

impl BreakEdit {
    /// `mark`'s options in `file`, the keys to the text of a logpoint, else to the
    /// condition. `new`: no breakpoint is there yet.
    pub(crate) fn of(file: &Path, mark: &Mark, new: bool) -> Self {
        Self {
            file: file.to_path_buf(),
            line: mark.line,
            act: mark.act,
            log: mark.log.clone(),
            condition: mark.condition.clone(),
            hits: mark.hits.clone(),
            enabled: mark.enabled,
            new,
            focus: Some(if mark.act == Act::Log { 0 } else { 1 }),
            error: String::new(),
        }
    }

    /// The breakpoint its fields make.
    ///
    /// # Errors
    ///
    /// The hit rule does not read.
    pub(crate) fn mark(&self) -> Result<Mark, String> {
        Hits::parse(&self.hits)?;
        Ok(Mark {
            line: self.line,
            act: self.act,
            log: self.log.clone(),
            condition: self.condition.trim().to_owned(),
            hits: self.hits.trim().to_owned(),
            enabled: self.enabled,
        })
    }
}

/// What the player's script host was given and says of the breakpoints: their lines in
/// the order given, their states, read again only when the host's version moves. It is
/// reached through the host's debugger, which works while a stage is stopped too.
#[derive(Debug, Default)]
pub(crate) struct HostView {
    /// Each file's breakpoints' lines (from 1), in the order the host was given them.
    given: BTreeMap<PathBuf, Vec<u32>>,
    /// What the host says of each, in that order.
    states: BTreeMap<PathBuf, Vec<BreakState>>,
    /// The files whose module the host has loaded.
    loaded: BTreeSet<PathBuf>,
    /// The host's version when the states were read.
    version: Option<u64>,
    /// A host was there at the last turn: a new one is given every breakpoint.
    seen: bool,
    /// Which errors stop, as the host was last given them.
    errors: Option<ErrorStops>,
}

impl HostView {
    /// The host's turn, once a frame: the breakpoints that changed given to it, and which
    /// errors stop where that changed, and what it says of them read when that moved. A
    /// comment's or a blank line's breakpoint goes to the line the host placed it on, where
    /// the script's text is as saved. Nothing is made while nothing changed.
    pub(crate) fn sync<'a>(
        &mut self,
        marks: &mut Breakpoints,
        errors: ErrorStops,
        scripts: Option<&ScriptDebugger>,
        doc_of: impl Fn(&Path) -> Option<&'a Document>,
    ) {
        let Some(scripts) = scripts else {
            self.seen = false;
            return;
        };
        if !self.seen {
            self.seen = true;
            self.errors = None;
            marks.touch_all();
        }
        if self.errors != Some(errors) {
            self.errors = Some(errors);
            scripts.set_error_stops(errors);
        }
        if marks.has_stale() {
            for file in marks.take_stale() {
                let Some(module) = module_name(&file) else {
                    continue;
                };
                let given = marks.for_host(&file, doc_of(&file));
                if given.is_empty() {
                    self.given.remove(&file);
                    self.states.remove(&file);
                } else {
                    self.given
                        .insert(file.clone(), given.iter().map(|b| b.line).collect());
                }
                scripts.set_breakpoints(&module, given);
            }
        }
        // Without breakpoints there is nothing to read.
        if self.given.is_empty() {
            self.states.clear();
            return;
        }
        let version = scripts.breakpoints_version();
        if self.version == Some(version) {
            return;
        }
        self.version = Some(version);
        self.loaded.clear();
        for (file, lines) in &self.given {
            let Some(module) = module_name(file) else {
                continue;
            };
            if scripts.is_loaded(&module) {
                self.loaded.insert(file.clone());
            }
            let states = scripts.breakpoint_states(&module);
            for (state, &asked) in states.iter().zip(lines) {
                let clean = doc_of(file).is_none_or(|d| !d.is_dirty());
                if let Some(placed) = state.placed
                    && placed != asked
                    && clean
                {
                    marks.placed_on(file, asked as usize - 1, placed as usize - 1);
                }
            }
            self.states.insert(file.clone(), states);
        }
    }

    /// What the host says of the breakpoint on line `line` (from 0) of `file`, whose text
    /// is `doc` where it is open; none while it waits for a save, or before the host has it.
    pub(crate) fn state(
        &self,
        file: &Path,
        doc: Option<&Document>,
        line: usize,
    ) -> Option<&BreakState> {
        let line = match doc {
            Some(doc) => doc.saved_line(line)?,
            None => line,
        };
        let k = self
            .given
            .get(file)?
            .iter()
            .position(|&l| l as usize == line + 1)?;
        self.states.get(file)?.get(k)
    }

    /// Whether the host has `file`'s module loaded: a breakpoint it did not place has no
    /// code from its line on.
    pub(crate) fn loaded(&self, file: &Path) -> bool {
        self.loaded.contains(file)
    }

    /// The states read again at the next turn (a new Play reset them).
    pub(crate) fn refresh(&mut self) {
        self.version = None;
    }

    /// The host's version when its states were last read.
    pub(crate) fn version(&self) -> Option<u64> {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::Pos;

    fn log(line: usize, text: &str) -> Mark {
        Mark {
            log: text.to_owned(),
            ..Mark::new(line, Act::Log)
        }
    }

    #[test]
    fn breakpoints_follow_their_lines_and_the_host_gets_the_saved_ones() {
        let file = Path::new("scripts/door.luau");
        let mut b = Breakpoints::default();
        b.put(file, log(2, "c {x}"));
        b.put(
            file,
            Mark {
                condition: String::from("x > 1"),
                hits: String::from("% 2"),
                ..Mark::new(4, Act::Pause)
            },
        );
        assert_eq!(b.take_stale(), [file.to_path_buf()]);
        let mut doc = Document::new("a\nb\nc\nd\ne");
        // A line put in above both: they go down with their text.
        doc.place(Pos::new(1, 0), false, false);
        doc.newline();
        b.follow(file, &doc.take_moves());
        let lines: Vec<usize> = b.of(file).iter().map(|m| m.line).collect();
        assert_eq!(lines, [3, 5]);
        assert!(b.has_stale(), "moved: given again");
        // Unsaved, the host keeps the lines its code has (from 1).
        let given = b.for_host(file, Some(&doc));
        assert_eq!((given[0].line, given[1].line), (3, 5));
        assert_eq!(given[1].condition.as_deref(), Some("x > 1"));
        assert_eq!(given[1].hits, Hits::EveryNth(2));
        // One set on the new line waits for the save; saved, the lines are the text's.
        b.put(file, log(1, "new"));
        assert_eq!(b.for_host(file, Some(&doc)).len(), 2);
        doc.mark_saved();
        let lines: Vec<u32> = b
            .for_host(file, Some(&doc))
            .iter()
            .map(|g| g.line)
            .collect();
        assert_eq!(lines, [2, 4, 6]);
        // Closed, its lines are the file's.
        assert_eq!(b.for_host(file, None).len(), 3);
        // `c` and `d` taken out: the logpoint on `c` lands on `e`'s line, where the pause
        // point came with its text; the pause point stays.
        doc.select(Pos::new(3, 0), Pos::new(5, 0), false);
        doc.delete();
        assert_eq!(doc.line(3), "e");
        b.follow(file, &doc.take_moves());
        let lines: Vec<usize> = b.of(file).iter().map(|m| m.line).collect();
        assert_eq!(lines, [1, 3]);
        assert_eq!(
            b.at(file, 3).map(|m| m.act),
            Some(Act::Pause),
            "its text is there"
        );
        // Placed by the host on the next line with code: the mark goes there.
        b.placed_on(file, 1, 2);
        assert!(b.at(file, 1).is_none() && b.at(file, 2).is_some());
    }

    #[test]
    fn removing_all_can_be_taken_back() {
        let (door, coin) = (
            Path::new("scripts/door.luau"),
            Path::new("scripts/coin.luau"),
        );
        let mut b = Breakpoints::default();
        b.put(door, log(2, "a"));
        b.put(coin, log(7, "b"));
        let _ = b.take_stale();
        assert_eq!(b.remove_all(), 2);
        assert!(b.files().next().is_none());
        assert_eq!(b.take_stale().len(), 2, "the host loses them");
        b.put(door, log(2, "set since"));
        assert_eq!(b.restore_removed(), 1, "the line set since is kept");
        assert_eq!(b.at(door, 2).map(|m| m.log.as_str()), Some("set since"));
        assert!(b.at(coin, 7).is_some());
        assert!(b.remove(coin, 7).is_some());
        assert!(b.files().all(|(f, _)| f == door));
    }
}
