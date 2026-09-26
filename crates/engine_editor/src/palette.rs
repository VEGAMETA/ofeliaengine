//! Commands, the command palette's search, and rebindable shortcuts (CLAUDE.md §12;
//! research `docs/research/ux/command-palette.md`).
//!
//! - [`Commands`] registers every editor action once, with a stable id
//!   (`"scene.duplicate"`), a category and a title. The palette, menus and shortcuts all
//!   refer to commands by [`CommandId`].
//! - [`Commands::search`] ranks commands for a query: the query must be a subsequence of
//!   "Category: Title"; matches at word starts, camel humps and consecutive characters
//!   score higher, gaps lower, matches in the category less than in the title, and
//!   recently run commands rank higher among similar scores. Searching allocates nothing
//!   once the result buffers have grown.
//! - [`Keymap`] maps key chords to commands per [`KeyContext`]. Binding a chord that
//!   another command already uses in the same context is refused with the other command,
//!   so the settings UI can show the conflict; [`Keymap::conflicts`] lists all of them
//!   (for keymaps loaded from text).
//!
//! ```
//! use engine_editor::{Chord, Commands, KeyContext, Keymap};
//!
//! let mut commands = Commands::new();
//! let dup = commands.register("scene.duplicate", "Scene", "Duplicate Selection");
//! let save = commands.register("file.save", "File", "Save Scene");
//! let mut keymap = Keymap::new();
//! keymap.bind(Chord::parse("Ctrl+D").unwrap(), KeyContext::Viewport, dup).unwrap();
//!
//! let mut results = Vec::new();
//! commands.search("dup sel", &mut results);
//! assert_eq!(results[0].id, dup);
//! commands.search("sv", &mut results);
//! assert_eq!(results[0].id, save);
//! assert_eq!(keymap.lookup(Chord::parse("Ctrl+D").unwrap(), &[KeyContext::Viewport]), Some(dup));
//! ```

use engine_input::Key;
use std::collections::HashMap;

/// A registered command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandId(u32);

/// A command's description.
#[derive(Debug, Clone)]
pub struct CommandInfo {
    /// Stable id used in keymaps and scripts (`"scene.duplicate"`).
    pub name: String,
    /// Shown dimmed before the title.
    pub category: String,
    /// What the user reads.
    pub title: String,
    /// "Category: Title", the searched text.
    label: String,
    /// The label's characters in lowercase (precomputed: searching allocates nothing).
    lower: Vec<char>,
    /// Whether each character starts a word (after a separator, or a camel hump).
    starts: Vec<bool>,
    /// Index of the title's first character in `lower`.
    title_char: usize,
}

impl CommandInfo {
    /// "Category: Title".
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// A search result: the command, its score, and which characters of its label matched
/// (in [`Commands::highlights`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// The command.
    pub id: CommandId,
    /// Higher is better.
    pub score: i32,
    /// Range of this match's positions in the highlight buffer.
    highlights: (u32, u32),
}

/// Every editor command.
#[derive(Default)]
pub struct Commands {
    list: Vec<CommandInfo>,
    by_name: HashMap<String, CommandId>,
    /// Last run "time" per command (0: never).
    last_run: Vec<u64>,
    clock: u64,
    /// Matched character indices of the last search's results.
    highlights: Vec<u16>,
    /// Scratch for one candidate's positions.
    scratch: Vec<u16>,
    query: Vec<char>,
}

impl Commands {
    /// No commands.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a command (or returns the existing one with this `name`).
    pub fn register(&mut self, name: &str, category: &str, title: &str) -> CommandId {
        if let Some(&id) = self.by_name.get(name) {
            return id;
        }
        let id = CommandId(self.list.len() as u32);
        let label = if category.is_empty() {
            title.to_owned()
        } else {
            format!("{category}: {title}")
        };
        let chars: Vec<char> = label.chars().collect();
        let lower = chars
            .iter()
            .map(|&c| c.to_lowercase().next().unwrap_or(c))
            .collect();
        let starts = (0..chars.len())
            .map(|i| {
                i == 0
                    || is_separator(chars[i - 1])
                    || (chars[i - 1].is_lowercase() && chars[i].is_uppercase())
            })
            .collect();
        let title_char = chars.len() - title.chars().count();
        self.list.push(CommandInfo {
            name: name.to_owned(),
            category: category.to_owned(),
            title: title.to_owned(),
            label,
            lower,
            starts,
            title_char,
        });
        self.by_name.insert(name.to_owned(), id);
        self.last_run.push(0);
        id
    }

    /// The command with this stable id.
    pub fn find(&self, name: &str) -> Option<CommandId> {
        self.by_name.get(name).copied()
    }

    /// A command's description.
    pub fn info(&self, id: CommandId) -> &CommandInfo {
        &self.list[id.0 as usize]
    }

    /// Number of commands.
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Records that a command ran (for recency ranking).
    pub fn ran(&mut self, id: CommandId) {
        self.clock += 1;
        self.last_run[id.0 as usize] = self.clock;
    }

    /// Ranks commands for `query` into `out` (best first). An empty query lists
    /// recently run commands first, then the rest by label.
    pub fn search(&mut self, query: &str, out: &mut Vec<Match>) {
        engine_profiler::zone!("editor::Commands::search");
        out.clear();
        self.highlights.clear();
        self.query.clear();
        self.query.extend(
            query
                .chars()
                .filter(|c| !c.is_whitespace())
                .flat_map(char::to_lowercase),
        );
        for (i, info) in self.list.iter().enumerate() {
            let recency = self.recency_bonus(i);
            if self.query.is_empty() {
                out.push(Match {
                    id: CommandId(i as u32),
                    score: recency,
                    highlights: (0, 0),
                });
                continue;
            }
            if let Some(score) = score(&self.query, info, &mut self.scratch) {
                let start = self.highlights.len() as u32;
                self.highlights.extend_from_slice(&self.scratch);
                out.push(Match {
                    id: CommandId(i as u32),
                    score: score + recency,
                    highlights: (start, self.highlights.len() as u32),
                });
            }
        }
        let list = &self.list;
        out.sort_unstable_by(|a, b| {
            b.score.cmp(&a.score).then_with(|| {
                list[a.id.0 as usize]
                    .label
                    .cmp(&list[b.id.0 as usize].label)
            })
        });
    }

    /// Character indices (in the label's chars) that matched, for highlighting.
    pub fn highlights(&self, m: &Match) -> &[u16] {
        &self.highlights[m.highlights.0 as usize..m.highlights.1 as usize]
    }

    fn recency_bonus(&self, i: usize) -> i32 {
        let t = self.last_run[i];
        if t == 0 {
            return 0;
        }
        // The most recent command gets 30, older ones less, down to 0.
        let age = self.clock - t;
        (30 - (age.min(30) as i32)).max(0)
    }
}

fn is_separator(c: char) -> bool {
    matches!(c, ' ' | '_' | '-' | '.' | ':' | '/' | '\\' | '(' | ')')
}

/// Scores `query` (lowercase, no whitespace) against a command's label; writes the
/// matched char indices to `positions`. `None` if the query is not a subsequence.
fn score(query: &[char], info: &CommandInfo, positions: &mut Vec<u16>) -> Option<i32> {
    positions.clear();
    let lower = &info.lower;
    let word_start = |i: usize| info.starts[i];
    let mut qi = 0;
    let mut i = 0;
    while qi < query.len() && i < lower.len() {
        if lower[i] == query[qi] {
            // Look ahead for a word start with the same character before committing to a
            // mid-word match ("dup sel" should hit "Selection", not "Duplicate"'s 'l').
            let mut pick = i;
            if !word_start(i) {
                let mut j = i + 1;
                while j < lower.len() {
                    if lower[j] == query[qi] && word_start(j) {
                        let rest_fits = is_subsequence(&query[qi + 1..], &lower[j + 1..]);
                        if rest_fits {
                            pick = j;
                        }
                        break;
                    }
                    j += 1;
                }
            }
            positions.push(pick as u16);
            qi += 1;
            i = pick + 1;
        } else {
            i += 1;
        }
    }
    if qi < query.len() {
        return None;
    }
    let title_start_char = info.title_char;
    let mut total = 0i32;
    let mut prev: Option<usize> = None;
    for &p in positions.iter() {
        let p = p as usize;
        let mut s = 16;
        if word_start(p) {
            s += 24;
        }
        if let Some(q) = prev {
            if p == q + 1 {
                s += 12;
            } else {
                s -= ((p - q - 1) as i32).min(12);
            }
        }
        if p < title_start_char {
            s -= 10;
        }
        total += s;
        prev = Some(p);
    }
    // A late first match costs a little; shorter labels win ties.
    let first = positions[0] as usize;
    let first_in_title = first.saturating_sub(title_start_char);
    total -= (first_in_title as i32).min(15);
    total -= (lower.len() as i32 / 8).min(8);
    Some(total)
}

fn is_subsequence(needle: &[char], hay: &[char]) -> bool {
    let mut it = hay.iter();
    needle.iter().all(|c| it.any(|h| h == c))
}

/// Where a shortcut applies. Specific contexts win over [`KeyContext::Global`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyContext {
    /// Everywhere.
    Global,
    /// A scene viewport has focus.
    Viewport,
    /// A text field has focus.
    TextField,
    /// A panel-specific context.
    Panel(u16),
}

impl KeyContext {
    fn name(self) -> String {
        match self {
            Self::Global => "global".into(),
            Self::Viewport => "viewport".into(),
            Self::TextField => "text".into(),
            Self::Panel(n) => format!("panel{n}"),
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "global" => Self::Global,
            "viewport" => Self::Viewport,
            "text" => Self::TextField,
            _ => Self::Panel(s.strip_prefix("panel")?.parse().ok()?),
        })
    }
}

/// A key with modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Chord {
    /// Control held.
    pub ctrl: bool,
    /// Shift held.
    pub shift: bool,
    /// Alt held.
    pub alt: bool,
    /// The key (physical position, ADR 0021).
    pub key: Key,
}

impl Chord {
    /// A chord without modifiers.
    pub const fn key(key: Key) -> Self {
        Self {
            ctrl: false,
            shift: false,
            alt: false,
            key,
        }
    }

    /// Parses `"Ctrl+Shift+P"`, `"Alt+F4"`, `"Delete"`. Letters and digits may be written
    /// bare (`P`, `1`) or as key names (`KeyP`, `Digit1`).
    pub fn parse(s: &str) -> Option<Self> {
        let mut chord = Self::key(Key::Escape);
        let mut key = None;
        for part in s.split('+').map(str::trim) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => chord.ctrl = true,
                "shift" => chord.shift = true,
                "alt" => chord.alt = true,
                _ => {
                    let k = Key::from_name(part)
                        .or_else(|| Key::from_name(&format!("Key{}", part.to_ascii_uppercase())))
                        .or_else(|| Key::from_name(&format!("Digit{part}")))?;
                    if key.replace(k).is_some() {
                        return None;
                    }
                }
            }
        }
        chord.key = key?;
        Some(chord)
    }
}

impl core::fmt::Display for Chord {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.shift {
            f.write_str("Shift+")?;
        }
        if self.alt {
            f.write_str("Alt+")?;
        }
        let name = self.key.name();
        let short = name
            .strip_prefix("Key")
            .or_else(|| name.strip_prefix("Digit"))
            .filter(|s| s.len() == 1)
            .unwrap_or(name);
        f.write_str(short)
    }
}

/// A chord bound to two commands in one context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict {
    /// The chord.
    pub chord: Chord,
    /// Where.
    pub context: KeyContext,
    /// The command that has it.
    pub existing: CommandId,
    /// The command that asked for it.
    pub wanted: CommandId,
}

/// Shortcuts: chords to commands, per context.
#[derive(Debug, Default, Clone)]
pub struct Keymap {
    bindings: Vec<(Chord, KeyContext, CommandId)>,
}

impl Keymap {
    /// No bindings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds `chord` in `context` to `command`. Refused (with the command that already
    /// has it) if another command uses the chord in the same context.
    pub fn bind(
        &mut self,
        chord: Chord,
        context: KeyContext,
        command: CommandId,
    ) -> Result<(), Conflict> {
        if let Some(&(_, _, existing)) = self
            .bindings
            .iter()
            .find(|(c, ctx, _)| *c == chord && *ctx == context)
        {
            if existing == command {
                return Ok(());
            }
            return Err(Conflict {
                chord,
                context,
                existing,
                wanted: command,
            });
        }
        self.bindings.push((chord, context, command));
        Ok(())
    }

    /// Removes every binding of `command` (or only `chord`'s).
    pub fn unbind(&mut self, command: CommandId, chord: Option<Chord>) {
        self.bindings
            .retain(|(c, _, id)| !(*id == command && chord.is_none_or(|x| x == *c)));
    }

    /// The command for `chord`, trying `active` contexts in order (most specific
    /// first), then [`KeyContext::Global`].
    pub fn lookup(&self, chord: Chord, active: &[KeyContext]) -> Option<CommandId> {
        active
            .iter()
            .chain(core::iter::once(&KeyContext::Global))
            .find_map(|ctx| {
                self.bindings
                    .iter()
                    .find(|(c, x, _)| *c == chord && x == ctx)
                    .map(|&(_, _, id)| id)
            })
    }

    /// The chords bound to `command` (for showing next to it in the palette).
    pub fn chords_of(&self, command: CommandId) -> impl Iterator<Item = Chord> + '_ {
        self.bindings
            .iter()
            .filter(move |b| b.2 == command)
            .map(|b| b.0)
    }

    /// Every chord that more than one command uses in the same context.
    pub fn conflicts(&self) -> Vec<Conflict> {
        let mut out = Vec::new();
        for (i, a) in self.bindings.iter().enumerate() {
            for b in &self.bindings[i + 1..] {
                if a.0 == b.0 && a.1 == b.1 && a.2 != b.2 {
                    out.push(Conflict {
                        chord: a.0,
                        context: a.1,
                        existing: a.2,
                        wanted: b.2,
                    });
                }
            }
        }
        out
    }

    /// Text form, one binding per line: `scene.duplicate = Ctrl+D @viewport`, sorted.
    pub fn to_text(&self, commands: &Commands) -> String {
        let mut lines: Vec<String> = self
            .bindings
            .iter()
            .map(|&(chord, ctx, id)| {
                let name = &commands.info(id).name;
                if ctx == KeyContext::Global {
                    format!("{name} = {chord}")
                } else {
                    format!("{name} = {chord} @{}", ctx.name())
                }
            })
            .collect();
        lines.sort();
        let mut s = lines.join("\n");
        s.push('\n');
        s
    }

    /// Reads [`Keymap::to_text`]'s format. Conflicting lines are kept (and reported by
    /// [`Keymap::conflicts`]) so a hand-edited file loses nothing; unknown commands and
    /// unreadable chords are returned as `(line, message)`.
    pub fn from_text(text: &str, commands: &Commands) -> (Self, Vec<(usize, String)>) {
        let mut map = Self::new();
        let mut errors = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, rest)) = line.split_once('=') else {
                errors.push((i + 1, "expected `command = chord`".into()));
                continue;
            };
            let (chord_text, ctx) = match rest.split_once('@') {
                Some((c, x)) => (c.trim(), KeyContext::parse(x.trim())),
                None => (rest.trim(), Some(KeyContext::Global)),
            };
            let Some(ctx) = ctx else {
                errors.push((i + 1, format!("unknown context in `{rest}`")));
                continue;
            };
            let Some(id) = commands.find(name.trim()) else {
                errors.push((i + 1, format!("unknown command `{}`", name.trim())));
                continue;
            };
            let Some(chord) = Chord::parse(chord_text) else {
                errors.push((i + 1, format!("cannot read `{chord_text}`")));
                continue;
            };
            map.bindings.push((chord, ctx, id));
        }
        (map, errors)
    }
}
