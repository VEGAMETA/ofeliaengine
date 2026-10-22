//! The Output panel's log (ADR 0107 E6, `docs/research/ux/output.md`): the records the
//! engine's logger takes (`engine_log::LogReader`) kept as rows, the same message again
//! folded into its row with a count (Unity's Collapse), a line where each Play starts;
//! shown filtered by level and by text. A row's text may name its cause (E6-2): an id, a
//! file and line, or a file, which a click shows.

use engine_log::log::Level;
use std::collections::VecDeque;

/// The rows kept; older ones go.
pub(crate) const KEPT: usize = 10_000;
/// How many rows back the same message folds into its row (not past a marking line).
const FOLD_BACK: usize = 64;
/// The rows shown at most, the newest last.
pub(crate) const SHOWN: usize = 500;

/// A level's place among the panel's toggles: errors, warnings, information, the rest
/// (debugging and tracing).
pub(crate) fn level_index(level: Level) -> usize {
    match level {
        Level::Error => 0,
        Level::Warn => 1,
        Level::Info => 2,
        Level::Debug | Level::Trace => 3,
    }
}

/// What a row's text names, which a click shows (E6-2).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cause {
    /// 32 hex digits: a scene entity's id, else an asset's.
    Id(u128),
    /// A file and a line in it, and a column: opened in VS Code.
    Place {
        path: String,
        line: u32,
        column: u32,
    },
    /// A file: a project asset shows in the FileSystem dock, another file opens.
    Path(String),
}

/// The first cause in `message`, word by word: 32 hex digits, `path:line[:column]`, or a
/// path with an extension (a folder in it, or a project file's extension).
pub(crate) fn cause_of(message: &str) -> Option<Cause> {
    const KNOWN: [&str; 14] = [
        "scene", "material", "glb", "gltf", "png", "jpg", "hdr", "exr", "ogg", "wav", "luau", "rs",
        "slang", "toml",
    ];
    let words = message.split(|c: char| c.is_whitespace() || "()[]{},;\"'`<>=".contains(c));
    for word in words {
        // A sentence's end, a label's colon.
        let word = word.trim_end_matches(['.', ':', '!', '?']);
        if word.len() == 32
            && let Ok(bits) = u128::from_str_radix(word, 16)
        {
            return Some(Cause::Id(bits));
        }
        let path_like = |p: &str| {
            let name = p.rsplit(['/', '\\']).next().unwrap_or(p);
            let Some((stem, ext)) = name.rsplit_once('.') else {
                return false;
            };
            !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= 8
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && (p.contains(['/', '\\']) || KNOWN.contains(&ext.to_ascii_lowercase().as_str()))
        };
        // `path:line[:column]`, a Windows drive's colon in the path.
        let mut parts = word.rsplitn(3, ':');
        let (last, mid, first) = (parts.next(), parts.next(), parts.next());
        let number = |s: Option<&str>| s.and_then(|s| s.parse::<u32>().ok());
        let place = match (number(mid), number(last), first) {
            (Some(line), Some(column), Some(path)) if path_like(path) => Some((path, line, column)),
            _ => match (number(last), word.rsplit_once(':')) {
                (Some(line), Some((path, _))) if path_like(path) => Some((path, line, 1)),
                _ => None,
            },
        };
        if let Some((path, line, column)) = place {
            return Some(Cause::Place {
                path: path.to_owned(),
                line,
                column,
            });
        }
        if path_like(word) {
            return Some(Cause::Path(word.to_owned()));
        }
    }
    None
}

/// A row of the Output.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    /// Its number among the rows ever made: a click names the row by it.
    pub(crate) serial: u64,
    /// What its text names, if anything (E6-2).
    pub(crate) cause: Option<Cause>,
    /// Its level's place ([`level_index`]); none for a line marking an event (a Play).
    pub(crate) level: Option<usize>,
    pub(crate) target: String,
    pub(crate) message: String,
    /// How many times it came, folded into one row.
    pub(crate) count: u32,
    /// When it last came, as shown: seconds since the logger started (`12.345`).
    pub(crate) time: String,
}

/// The log as the panel shows it.
pub(crate) struct Output {
    reader: Option<engine_log::LogReader>,
    rows: VecDeque<Row>,
    /// The records of each level so far, folded ones too: what the toggles say.
    pub(crate) counts: [usize; 4],
    /// The levels shown.
    pub(crate) levels: [bool; 4],
    /// Shown only: rows whose target or message holds it, in any case.
    pub(crate) filter: String,
    /// The newest row is kept in view as rows come.
    pub(crate) follow: bool,
    /// Records the ring overwrote before the panel read them.
    pub(crate) lost: u64,
    serials: u64,
}

impl Output {
    /// The log from the oldest record still in the engine's ring: what the start said too.
    pub(crate) fn new() -> Self {
        Self {
            reader: Some(engine_log::LogReader::from_oldest()),
            ..Self::detached()
        }
    }

    /// An Output that reads no logger: rows come only through [`Self::push`] (tests).
    pub(crate) fn detached() -> Self {
        Self {
            reader: None,
            rows: VecDeque::new(),
            counts: [0; 4],
            levels: [true, true, true, false],
            filter: String::new(),
            follow: true,
            lost: 0,
            serials: 0,
        }
    }

    /// Row `serial`, if it is still kept.
    pub(crate) fn row(&self, serial: u64) -> Option<&Row> {
        self.rows.iter().rev().find(|r| r.serial == serial)
    }

    /// Takes the records logged since the last call. Whether any came.
    pub(crate) fn poll(&mut self) -> bool {
        let Some(reader) = self.reader.as_mut() else {
            return false;
        };
        let mut came = Vec::new();
        self.lost += reader.read(|r| {
            came.push((r.time, r.level, r.target.to_owned(), r.message.to_owned()));
        });
        let any = !came.is_empty();
        for (time, level, target, message) in came {
            self.push(time, level, &target, &message);
        }
        any
    }

    /// A record logged `nanos` after the logger started: folded into the same message's row
    /// among the last ones (moved last, its count up), else a new row.
    pub(crate) fn push(&mut self, nanos: u64, level: Level, target: &str, message: &str) {
        let level = level_index(level);
        self.counts[level] += 1;
        let time = format!("{:.3}", nanos as f64 / 1e9);
        let back = self
            .rows
            .iter()
            .rev()
            .take(FOLD_BACK)
            .take_while(|r| r.level.is_some())
            .position(|r| r.level == Some(level) && r.target == target && r.message == message);
        if let Some(back) = back {
            let at = self.rows.len() - 1 - back;
            if let Some(mut row) = self.rows.remove(at) {
                row.count += 1;
                row.time = time;
                self.rows.push_back(row);
            }
            return;
        }
        self.serials += 1;
        self.rows.push_back(Row {
            serial: self.serials,
            cause: cause_of(message),
            level: Some(level),
            target: target.to_owned(),
            message: message.to_owned(),
            count: 1,
            time,
        });
        if self.rows.len() > KEPT {
            self.rows.pop_front();
        }
    }

    /// A line marking an event (`Play`): rows do not fold across it.
    pub(crate) fn mark(&mut self, text: &str) {
        self.serials += 1;
        self.rows.push_back(Row {
            serial: self.serials,
            cause: None,
            level: None,
            target: String::new(),
            message: text.to_owned(),
            count: 1,
            time: String::new(),
        });
        if self.rows.len() > KEPT {
            self.rows.pop_front();
        }
    }

    /// Every row gone, and the counts.
    pub(crate) fn clear(&mut self) {
        self.rows.clear();
        self.counts = [0; 4];
        self.lost = 0;
    }

    /// The rows shown, oldest first: those of the levels on (marking lines always) holding
    /// the filter, the last [`SHOWN`].
    pub(crate) fn shown(&self) -> Vec<&Row> {
        let filter = self.filter.trim().to_lowercase();
        let matches = |r: &&Row| {
            r.level.is_none_or(|l| self.levels[l])
                && (filter.is_empty()
                    || r.message.to_lowercase().contains(&filter)
                    || r.target.to_lowercase().contains(&filter))
        };
        let mut shown: Vec<&Row> = self.rows.iter().rev().filter(matches).take(SHOWN).collect();
        shown.reverse();
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_fold_into_their_row_and_filters_narrow_what_shows() {
        let mut out = Output::detached();
        out.push(1_000_000_000, Level::Info, "game", "started");
        for i in 0..5 {
            out.push(
                2_000_000_000 + i,
                Level::Error,
                "assets",
                "props/lamp.scene: no part",
            );
            out.push(
                2_000_000_000 + i,
                Level::Warn,
                "physics",
                "a body fell through",
            );
        }
        // Each repeat folded into its row, moved last: three rows, the counts five.
        let shown = out.shown();
        assert_eq!(shown.len(), 3);
        assert_eq!(shown[0].message, "started");
        assert_eq!(
            (shown[1].message.as_str(), shown[1].count),
            ("props/lamp.scene: no part", 5)
        );
        assert_eq!(
            (shown[2].message.as_str(), shown[2].count),
            ("a body fell through", 5)
        );
        assert_eq!(out.counts, [5, 5, 1, 0]);
        assert_eq!(shown[1].time, "2.000");
        // Not across a Play's line.
        out.mark("Play");
        out.push(3_000_000_000, Level::Warn, "physics", "a body fell through");
        let shown = out.shown();
        assert_eq!(shown.len(), 5);
        assert_eq!(shown[3].level, None);
        assert_eq!(shown[4].count, 1);
        // Warnings off: the errors, the information and the line.
        out.levels[1] = false;
        assert_eq!(out.shown().len(), 3);
        // A filter by text, in the message or the target, any case.
        out.levels[1] = true;
        out.filter = String::from("LAMP");
        let shown = out.shown();
        assert!(shown.iter().any(|r| r.message.contains("lamp")));
        assert!(
            shown
                .iter()
                .all(|r| r.level.is_none() || r.message.contains("lamp"))
        );
        out.filter = String::from("physics");
        assert_eq!(out.shown().iter().filter(|r| r.level.is_some()).count(), 2);
        // Debugging is off by default; cleared, nothing.
        out.filter.clear();
        out.push(4, Level::Debug, "render", "a pass");
        assert!(out.shown().iter().all(|r| r.message != "a pass"));
        out.clear();
        assert!(out.shown().is_empty());
        assert_eq!(out.counts, [0; 4]);
    }

    #[test]
    fn a_rows_text_names_its_cause() {
        let id = "6c1f0a3e9d2b47c88e51f0d4a7b3c912";
        assert_eq!(
            cause_of(&format!("[entity {id}] has no parent")),
            Some(Cause::Id(u128::from_str_radix(id, 16).unwrap()))
        );
        assert_eq!(
            cause_of("props/lamp.scene: the file has no part named Shade"),
            Some(Cause::Path(String::from("props/lamp.scene")))
        );
        assert_eq!(
            cause_of("src/main.rs:12:5: expected `;`"),
            Some(Cause::Place {
                path: String::from("src/main.rs"),
                line: 12,
                column: 5
            })
        );
        assert_eq!(
            cause_of(r"C:\game\scripts\door.luau:7 attempt to index nil"),
            Some(Cause::Place {
                path: String::from(r"C:\game\scripts\door.luau"),
                line: 7,
                column: 1
            })
        );
        assert_eq!(
            cause_of("lamp.scene saved"),
            Some(Cause::Path(String::from("lamp.scene"))),
            "a project file's extension without a folder"
        );
        // Words that only look a little like paths: nothing.
        for plain in [
            "the level loaded in 12.5 ms",
            "and/or",
            "version 1.2.3 found",
            "e.g. this",
        ] {
            assert_eq!(cause_of(plain), None, "{plain}");
        }
        // Pushed rows keep theirs, and a serial to be found by.
        let mut out = Output::detached();
        out.push(1, Level::Error, "assets", "props/lamp.scene: no part");
        let serial = out.shown()[0].serial;
        assert_eq!(
            out.row(serial).and_then(|r| r.cause.clone()),
            Some(Cause::Path(String::from("props/lamp.scene")))
        );
    }

    #[test]
    fn the_oldest_rows_go_past_the_limit() {
        let mut out = Output::detached();
        out.levels = [true; 4];
        for i in 0..(KEPT + 10) {
            out.push(i as u64, Level::Info, "t", &format!("line {i}"));
        }
        let shown = out.shown();
        assert_eq!(shown.len(), SHOWN);
        assert_eq!(shown[SHOWN - 1].message, format!("line {}", KEPT + 9));
        assert_eq!(out.rows.len(), KEPT);
        assert_eq!(out.rows[0].message, "line 10");
    }
}
