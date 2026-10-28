//! The mixer's layout as the editor holds it (ADR 0107 E11-1, ADR 0121): the project's
//! `main.mixer`, or the built-in buses until it is first saved. The Audio tab's strips
//! and the inspector's Bus section edit it; each edit is a file step of the one history
//! and goes to the player at once (`Player::set_mixer_layout`).
//!
//! - **Strips:** the master first, then the buses as the layout orders them (the tree
//!   depth first, siblings by name), each with its depth, fader and output.
//! - **The fader's travel** gives more room near 0 dB, as consoles do: +6 dB at the top,
//!   0 dB at four fifths, −10, −24 and −48 dB below, −80 dB (silence) at the bottom.
//! - **Edits** keep the layout whole: a bus never feeds itself or one of its own (the
//!   output list leaves those out), a rename carries its children, the sends into it and
//!   the sidechains it keys along, a removed bus hands its children to the bus it fed.
//! - **Effects and sends** (E11-2b, ADR 0121's version 2): a bus's chain edited effect by
//!   effect; the lists offer only the sends and sidechains that would not loop the sound
//!   back.

use engine_app::effects::{EFFECTS, EffectInfo, ParamInfo, Unit, effect_info};
use engine_app::{BusLayout, EffectLayout, MixerFile};
use engine_scene::AssetId;
use std::fmt::Write as _;
use std::path::Path;

/// The quietest a fader goes, in dB: silence to the ear.
pub(crate) const FLOOR: f32 = -80.0;
/// The loudest, in dB.
pub(crate) const CEILING: f32 = 6.0;
/// The marks beside a fader, in dB.
pub(crate) const TICKS: [f32; 5] = [0.0, -6.0, -12.0, -24.0, -48.0];
/// The master's name, in the panel and the inspector.
pub(crate) const MASTER: &str = "master";
/// The room's reverb's strip (E11-2c): the player's own bus, made and set by the room
/// around the listener; only its fader, `room.gain`, is the file's.
pub(crate) const ROOM: &str = "reverb";

/// The fader's travel: where each gain sits, 0 at the bottom, 1 at the top.
const TRAVEL: [(f32, f32); 6] = [
    (0.0, FLOOR),
    (0.2, -48.0),
    (0.4, -24.0),
    (0.6, -10.0),
    (0.8, 0.0),
    (1.0, CEILING),
];

/// Where gain `db` sits on a fader, 0 (the bottom) to 1 (the top).
pub(crate) fn fader_position(db: f32) -> f32 {
    let db = db.clamp(FLOOR, CEILING);
    for pair in TRAVEL.windows(2) {
        let ((p0, g0), (p1, g1)) = (pair[0], pair[1]);
        if db <= g1 {
            return p0 + (db - g0) / (g1 - g0) * (p1 - p0);
        }
    }
    1.0
}

/// The gain in dB at fader position `p` (0 to 1), to a tenth of a dB.
pub(crate) fn fader_gain(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    for pair in TRAVEL.windows(2) {
        let ((p0, g0), (p1, g1)) = (pair[0], pair[1]);
        if p <= p1 {
            return (10.0 * (g0 + (p - p0) / (p1 - p0) * (g1 - g0))).round() / 10.0;
        }
    }
    CEILING
}

/// Gain `db` as a strip shows it: "0.0", "−6.0" (with a minus sign), "−∞" at the floor.
pub(crate) fn gain_text(out: &mut String, db: f32) {
    out.clear();
    if db <= FLOOR {
        out.push_str("\u{2212}\u{221e}");
    } else if db < -0.05 {
        let _ = write!(out, "\u{2212}{:.1}", -db);
    } else {
        let _ = write!(out, "{:.1}", db.max(0.0));
    }
}

/// The quietest a meter shows, and the loudest, in dBFS.
pub(crate) const METER_FLOOR: f32 = -60.0;
pub(crate) const METER_TOP: f32 = 6.0;
/// How fast a meter falls back, in dB a second (a peak meter's usual ballistics).
const FALL: f32 = 24.0;

/// Where level `db` (dBFS) stands on a meter, 0 (the floor) to 1 (the top).
pub(crate) fn meter_position(db: f32) -> f32 {
    ((db - METER_FLOOR) / (METER_TOP - METER_FLOOR)).clamp(0.0, 1.0)
}

/// A strip's meter (E11-1b-2): each channel's level in dBFS falling back at [`FALL`], the
/// loudest peak since it was last cleared (as text: "−3.2", "+0.4"), and whether the bus
/// went over full scale (it may clip: a light until cleared).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Meter {
    pub(crate) name: String,
    pub(crate) level: [f32; 2],
    pub(crate) held: f32,
    pub(crate) clip: bool,
    pub(crate) text: String,
}

impl Meter {
    /// Bus `name`'s meter, silent.
    pub(crate) fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            level: [METER_FLOOR; 2],
            held: f32::NEG_INFINITY,
            clip: false,
            text: String::new(),
        }
    }

    /// The bus's peaks since the last look (linear, per channel), `dt` seconds after it.
    pub(crate) fn feed(&mut self, peak: [f32; 2], dt: f32) {
        let held = self.held;
        for (level, &p) in self.level.iter_mut().zip(&peak) {
            let db = if p > 0.0 {
                20.0 * p.log10()
            } else {
                f32::NEG_INFINITY
            };
            *level = db.max(*level - FALL * dt).max(METER_FLOOR);
            self.held = self.held.max(db);
            self.clip |= p > 1.0;
        }
        if self.held != held {
            self.write_held();
        }
    }

    /// Its held peak and clip light cleared.
    pub(crate) fn clear(&mut self) {
        self.held = f32::NEG_INFINITY;
        self.clip = false;
        self.text.clear();
    }

    fn write_held(&mut self) {
        self.text.clear();
        if !self.held.is_finite() {
            return;
        }
        let _ = if self.held >= 0.05 {
            write!(self.text, "+{:.1}", self.held)
        } else if self.held <= -0.05 {
            write!(self.text, "\u{2212}{:.1}", -self.held)
        } else {
            write!(self.text, "0.0")
        };
    }
}

/// Whether the listening aids silence bus `name` of `layout` (the player's rule,
/// E11-1b-2): muted, or while a bus is soloed, neither soloed nor on a soloed bus's way to
/// the master nor feeding one. The master never.
pub(crate) fn silenced(
    layout: &MixerFile,
    muted: &[String],
    soloed: &[String],
    name: &str,
) -> bool {
    // The room's reverb is never silenced (a soloed bus keeps its reverb, as a desk's
    // returns are solo safe).
    if name == MASTER || name == ROOM {
        return false;
    }
    if muted.iter().any(|m| m == name) {
        return true;
    }
    if soloed.is_empty() {
        return false;
    }
    !soloed
        .iter()
        .any(|s| s == name || feeds_into(layout, s, name) || feeds_into(layout, name, s))
}

/// A strip of the panel.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Strip {
    /// Its bus's name ([`MASTER`] for the master).
    pub(crate) name: String,
    /// How deep under the master: 0 the master, 1 the buses it is fed by.
    pub(crate) depth: u8,
    /// Its gain in dB, and as text.
    pub(crate) gain: f32,
    pub(crate) gain_text: String,
    /// Where it goes: "→ master", "→ world"; the master's "→ speakers"; with its sends'
    /// count ("→ master +1").
    pub(crate) output: String,
    /// Its effects: the one's label, or how many ("3 fx"); empty for none.
    pub(crate) effects: String,
    /// Every one of them bypassed (B lit).
    pub(crate) bypassed: bool,
    /// The chosen snapshot sets its fader (E11-5b).
    pub(crate) marked: bool,
}

/// A bus's rows in the inspector (E11-1b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BusRow {
    Name,
    Output,
    Gain,
    Balance,
    Voices,
    Reverb,
}

impl BusRow {
    /// A bus's rows.
    pub(crate) const ALL: [Self; 6] = [
        Self::Name,
        Self::Output,
        Self::Gain,
        Self::Balance,
        Self::Voices,
        Self::Reverb,
    ];
    /// The master's.
    pub(crate) const OF_MASTER: [Self; 2] = [Self::Gain, Self::Balance];
    /// The room's reverb's: its fader alone (E11-2c).
    pub(crate) const OF_ROOM: [Self; 1] = [Self::Gain];

    /// Its name in the inspector.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Output => "output",
            Self::Gain => "gain",
            Self::Balance => "balance",
            Self::Voices => "voices",
            Self::Reverb => "reverb",
        }
    }

    /// The rows of bus `bus`.
    pub(crate) fn of(bus: &str) -> &'static [Self] {
        match bus {
            MASTER => &Self::OF_MASTER,
            ROOM => &Self::OF_ROOM,
            _ => &Self::ALL,
        }
    }
}

/// The mixer's layout as the editor holds it.
pub(crate) struct MixerDoc {
    /// Its file's id (a new one until it is first saved).
    pub(crate) id: AssetId,
    /// Its path from the project's root.
    pub(crate) path: String,
    pub(crate) layout: MixerFile,
    /// Its text as the editor holds it, and as its file holds it (as the editor writes it):
    /// unsaved while they differ.
    pub(crate) text: String,
    pub(crate) saved: String,
    /// Its file is on disk.
    pub(crate) exists: bool,
    /// Given to the player since it last changed.
    pub(crate) given: bool,
    /// How many times it changed, the count its unsaved text was autosaved at, and when an
    /// autosave is due (E11-1b-2, as material files').
    pub(crate) revision: u64,
    pub(crate) autosaved: Option<u64>,
    pub(crate) due: Option<std::time::Instant>,
    /// The panel's strips.
    pub(crate) strips: Vec<Strip>,
    /// The snapshot the panel edits (E11-5b, ADR 0122): none for the mix itself; and the
    /// layout as heard with it alone at full weight (the mix itself without one), which the
    /// panel shows.
    pub(crate) chosen: Option<String>,
    pub(crate) view: MixerFile,
    /// Why the last edit was refused (a snapshot holds values only).
    pub(crate) refused: Option<String>,
}

impl MixerDoc {
    /// The layout at `path` under the project's `root`: its file, or the built-in buses
    /// with a new id until it is saved.
    ///
    /// # Errors
    ///
    /// A file that cannot be read or is not a mixer layout.
    pub(crate) fn open(root: &Path, path: &str) -> Result<Self, String> {
        let full = root.join(path);
        if !full.is_file() {
            let mut doc = Self::new(AssetId::new(), path, MixerFile::builtin());
            doc.exists = false;
            return Ok(doc);
        }
        let text = std::fs::read_to_string(&full).map_err(|e| format!("{path}: {e}"))?;
        let (layout, report) = MixerFile::parse(&text).map_err(|e| format!("{path}:{e}"))?;
        Ok(Self::new(
            report.id.unwrap_or_else(AssetId::new),
            path,
            layout,
        ))
    }

    /// Layout `layout` of file `id` at `path`, as saved: unsaved only once edited.
    pub(crate) fn new(id: AssetId, path: &str, layout: MixerFile) -> Self {
        let text = layout.to_text(id);
        let mut doc = Self {
            id,
            path: path.to_owned(),
            layout,
            saved: text.clone(),
            text,
            exists: true,
            given: false,
            revision: 0,
            autosaved: None,
            due: None,
            strips: Vec::new(),
            chosen: None,
            view: MixerFile::default(),
            refused: None,
        };
        doc.refresh();
        doc
    }

    /// Edited since it was saved.
    pub(crate) fn dirty(&self) -> bool {
        self.text != self.saved
    }

    /// `edit` applied to the layout: its text before and after, when it changed.
    pub(crate) fn edit(&mut self, edit: impl FnOnce(&mut MixerFile)) -> Option<(String, String)> {
        match self.chosen.clone() {
            None => edit(&mut self.layout),
            Some(name) => {
                // A snapshot chosen: the edit is made to what the panel shows, and what it
                // changes becomes the snapshot's values; the mix stays as it is.
                let mut edited = self.view.clone();
                edit(&mut edited);
                if values_aside(&edited) != values_aside(&self.view) {
                    self.refused = Some(format!(
                        "`{name}` holds values only: choose Mix to change the buses, effects \
                         and sends themselves"
                    ));
                    return None;
                }
                let k = self.layout.snapshots.iter().position(|s| s.name == name)?;
                for path in self.view.paths() {
                    let Some(t) = engine_app::snapshot_target(&path) else {
                        continue;
                    };
                    if let Some(v) = edited.value_at(t)
                        && self.view.value_at(t) != Some(v)
                    {
                        self.layout.snapshots[k].values.insert(path, v);
                    }
                }
            }
        }
        self.settle()
    }

    /// `edit` made to the mix itself, whatever snapshot is chosen: the snapshots' own list,
    /// names and fades.
    pub(crate) fn edit_mix(
        &mut self,
        edit: impl FnOnce(&mut MixerFile),
    ) -> Option<(String, String)> {
        edit(&mut self.layout);
        if self
            .chosen
            .as_ref()
            .is_some_and(|c| self.layout.snapshot(c).is_none())
        {
            self.chosen = None;
        }
        self.settle()
    }

    /// Value `path` taken out of the chosen snapshot (its ↺): the mix's value again.
    pub(crate) fn forget(&mut self, path: &str) -> Option<(String, String)> {
        let name = self.chosen.clone()?;
        let k = self.layout.snapshots.iter().position(|s| s.name == name)?;
        self.layout.snapshots[k].values.remove(path)?;
        self.settle()
    }

    /// Whether the chosen snapshot sets `<bus>/<rest…>` (each of `rest` a step), with
    /// nothing allocated: the inspector's marks a frame.
    pub(crate) fn sets_at(&self, bus: &str, rest: &[&str]) -> bool {
        let bus = if bus == ROOM { "room" } else { bus };
        self.chosen
            .as_deref()
            .and_then(|c| self.layout.snapshot(c))
            .is_some_and(|s| {
                s.values.keys().any(|key| {
                    let mut steps = key.split('/');
                    steps.next() == Some(bus) && steps.eq(rest.iter().copied())
                })
            })
    }

    /// Whether the chosen snapshot sets effect `k` of bus `bus`'s `what` (a parameter, or
    /// `bypass`), with nothing allocated.
    pub(crate) fn sets_effect(&self, bus: &str, k: usize, what: &str) -> bool {
        self.chosen
            .as_deref()
            .and_then(|c| self.layout.snapshot(c))
            .is_some_and(|s| {
                s.values.keys().any(|key| {
                    let mut steps = key.split('/');
                    steps.next() == Some(bus)
                        && steps.next() == Some("effects")
                        && steps.next().and_then(|i| i.parse::<usize>().ok()) == Some(k)
                        && steps.next() == Some(what)
                        && steps.next().is_none()
                })
            })
    }

    /// Whether the chosen snapshot sets `path`.
    pub(crate) fn sets(&self, path: &str) -> bool {
        self.chosen
            .as_deref()
            .and_then(|c| self.layout.snapshot(c))
            .is_some_and(|s| s.values.contains_key(path))
    }

    /// Snapshot `name` chosen for the panel's edits (none: the mix itself): what it shows
    /// is then heard with it alone.
    pub(crate) fn choose(&mut self, name: Option<&str>) {
        self.chosen = name
            .filter(|n| self.layout.snapshot(n).is_some())
            .map(str::to_owned);
        self.refresh();
    }

    /// After an edit: the text again, and whether it changed (its text before and after).
    fn settle(&mut self) -> Option<(String, String)> {
        let after = self.layout.to_text(self.id);
        if after == self.text {
            return None;
        }
        let before = core::mem::replace(&mut self.text, after.clone());
        self.given = false;
        self.revision += 1;
        self.refresh();
        Some((before, after))
    }

    /// Its text put back (undo, redo).
    ///
    /// # Errors
    ///
    /// A text that is not a mixer layout.
    pub(crate) fn put_text(&mut self, text: &str) -> Result<(), String> {
        let (layout, _) = MixerFile::parse(text).map_err(|e| format!("{}:{e}", self.path))?;
        self.layout = layout;
        self.text = text.to_owned();
        self.given = false;
        self.revision += 1;
        if self
            .chosen
            .as_ref()
            .is_some_and(|c| self.layout.snapshot(c).is_none())
        {
            self.chosen = None;
        }
        self.refresh();
        Ok(())
    }

    /// Saved: as its file now holds it.
    pub(crate) fn saved_now(&mut self) {
        self.saved.clone_from(&self.text);
        self.exists = true;
    }

    /// The view and the strips again from the layout.
    fn refresh(&mut self) {
        self.view = match &self.chosen {
            Some(name) => self.layout.with_snapshot(name),
            None => self.layout.clone(),
        };
        self.strips.clear();
        let mut master = Strip {
            name: String::from(MASTER),
            depth: 0,
            gain: self.view.master.gain,
            gain_text: String::new(),
            output: String::from("\u{2192} speakers"),
            effects: String::new(),
            bypassed: false,
            marked: false,
        };
        gain_text(&mut master.gain_text, master.gain);
        master.bypassed = effects_text(&mut master.effects, &self.view.master.effects);
        self.strips.push(master);
        let order = self.view.order();
        let mut depths: Vec<(&str, u8)> = Vec::with_capacity(order.len());
        for placed in &order {
            let depth = placed
                .parent
                .and_then(|p| depths.iter().find(|(n, _)| *n == p))
                .map_or(1, |&(_, d)| d.saturating_add(1));
            depths.push((placed.name, depth));
            let mut output = format!("\u{2192} {}", placed.parent.unwrap_or(MASTER));
            if !placed.layout.sends.is_empty() {
                let _ = write!(output, " +{}", placed.layout.sends.len());
            }
            let mut strip = Strip {
                name: placed.name.to_owned(),
                depth,
                gain: placed.layout.gain,
                gain_text: String::new(),
                output,
                effects: String::new(),
                bypassed: false,
                marked: false,
            };
            gain_text(&mut strip.gain_text, strip.gain);
            strip.bypassed = effects_text(&mut strip.effects, &placed.layout.effects);
            self.strips.push(strip);
        }
        // The room's reverb at the right end (E11-2c): its fader the file's trim.
        let mut room = Strip {
            name: String::from(ROOM),
            depth: 0,
            gain: self.view.room.gain,
            gain_text: String::new(),
            output: format!("\u{2192} {MASTER}"),
            effects: String::new(),
            bypassed: false,
            marked: false,
        };
        gain_text(&mut room.gain_text, room.gain);
        self.strips.push(room);
        // What the chosen snapshot sets of each fader.
        let mut marks: Vec<bool> = Vec::with_capacity(self.strips.len());
        for s in &self.strips {
            let path = if s.name == ROOM {
                String::from("room/gain")
            } else {
                format!("{}/gain", s.name)
            };
            marks.push(self.sets(&path));
        }
        for (s, marked) in self.strips.iter_mut().zip(marks) {
            s.marked = marked;
        }
    }

    /// The strip of bus `bus`.
    pub(crate) fn strip(&self, bus: &str) -> Option<usize> {
        self.strips.iter().position(|s| s.name == bus)
    }

    /// Bus `bus`'s gain in dB as the panel shows it (the master's for [`MASTER`]; the
    /// chosen snapshot's where it sets it).
    pub(crate) fn gain(&self, bus: &str) -> Option<f32> {
        match bus {
            MASTER => Some(self.view.master.gain),
            ROOM => Some(self.view.room.gain),
            _ => self.view.buses.get(bus).map(|b| b.gain),
        }
    }
}

/// `layout` with every value a snapshot can set put at zero: what is left is what only the
/// mix itself holds (buses, effects, sends, parents, voices, the snapshots' list).
fn values_aside(layout: &MixerFile) -> MixerFile {
    let mut aside = layout.clone();
    for path in layout.paths() {
        if let Some(t) = engine_app::snapshot_target(&path) {
            aside.set_value(t, 0.0);
        }
    }
    aside
}

/// A new snapshot at the end of the list, named `snapshot` (`snapshot 2`, ...): its name.
pub(crate) fn add_snapshot(layout: &mut MixerFile) -> String {
    let mut name = String::from("snapshot");
    let mut n = 1;
    while layout.snapshot(&name).is_some() {
        n += 1;
        name = format!("snapshot {n}");
    }
    layout
        .snapshots
        .push(engine_app::SnapshotLayout::new(&name));
    name
}

/// Snapshot `name` taken away.
pub(crate) fn remove_snapshot(layout: &mut MixerFile, name: &str) {
    layout.snapshots.retain(|s| s.name != name);
}

/// Snapshot `from` called `to`.
///
/// # Errors
///
/// An empty name, or one another snapshot has.
pub(crate) fn rename_snapshot(layout: &mut MixerFile, from: &str, to: &str) -> Result<(), String> {
    let to = to.trim();
    if to == from {
        return Ok(());
    }
    if to.is_empty() {
        return Err(String::from("a snapshot needs a name"));
    }
    if layout.snapshot(to).is_some() {
        return Err(format!("there is a snapshot `{to}` already"));
    }
    let s = layout
        .snapshots
        .iter_mut()
        .find(|s| s.name == from)
        .ok_or_else(|| format!("there is no snapshot `{from}`"))?;
    to.clone_into(&mut s.name);
    Ok(())
}

/// Snapshot `name`'s fade set to `seconds` (0 to 60, to the hundredth).
pub(crate) fn set_snapshot_fade(layout: &mut MixerFile, name: &str, seconds: f32) {
    if let Some(s) = layout.snapshots.iter_mut().find(|s| s.name == name) {
        s.fade = (100.0 * seconds.clamp(0.0, 60.0)).round() / 100.0;
    }
}

/// What a strip says of `chain`: the one effect's short name, or how many ("3 fx");
/// nothing for none. Whether every one of them is bypassed.
fn effects_text(out: &mut String, chain: &[EffectLayout]) -> bool {
    out.clear();
    match chain {
        [] => return false,
        [one] => out.push_str(short_name(&one.kind)),
        many => {
            let _ = write!(out, "{} fx", many.len());
        }
    }
    chain.iter().all(|e| e.bypass)
}

/// An effect kind's name as a strip, 84 px wide, has room for beside its gain reduction
/// and B (a desk's scribble strip; the inspector shows the whole name).
pub(crate) fn short_name(kind: &str) -> &str {
    match kind {
        "compressor" => "Comp",
        "limiter" => "Limit",
        "low_pass" => "LP",
        "high_pass" => "HP",
        "band_pass" => "BP",
        "notch" => "Notch",
        "peak" => "EQ",
        "low_shelf" => "LS",
        "high_shelf" => "HS",
        "all_pass" => "AP",
        "delay" => "Delay",
        "reverb" => "Reverb",
        other => other,
    }
}

/// A row of an effect's section in the inspector (E11-2b).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum EffectRow {
    /// A filter's shape: another of the filter kinds.
    Shape,
    /// Left out of the mix.
    Bypass,
    /// The bus keying it (the kinds a sidechain keys).
    Sidechain,
    /// One of its parameters.
    Param(&'static ParamInfo),
    /// A kind the engine does not have: its name.
    Unknown,
}

impl EffectRow {
    /// The rows of an effect of `kind`, in order.
    pub(crate) fn of(kind: &str) -> impl Iterator<Item = Self> {
        let info = effect_info(kind);
        let head = match info {
            None => [Some(Self::Unknown), None, None],
            Some(i) => [
                i.filter.then_some(Self::Shape),
                Some(Self::Bypass),
                i.sidechain.then_some(Self::Sidechain),
            ],
        };
        head.into_iter().flatten().chain(
            info.into_iter()
                .flat_map(|i| i.params.iter().map(Self::Param)),
        )
    }

    /// Row `i` of an effect of `kind`.
    pub(crate) fn at(kind: &str, i: usize) -> Option<Self> {
        Self::of(kind).nth(i)
    }
}

/// The filter kinds, in [`EFFECTS`]'s order: what a filter's shape row offers.
pub(crate) fn filters() -> impl Iterator<Item = &'static EffectInfo> {
    EFFECTS.iter().filter(|e| e.filter)
}

/// A parameter's value as the inspector shows it: a share as a percentage.
pub(crate) fn shown(p: &ParamInfo, value: f32) -> f32 {
    if p.unit == Unit::Share {
        value * 100.0
    } else {
        value
    }
}

/// A value the inspector was given back in the parameter's own unit.
pub(crate) fn stored(p: &ParamInfo, value: f32) -> f32 {
    if p.unit == Unit::Share {
        value / 100.0
    } else {
        value
    }
}

/// A parameter's value as text, with as many decimals as its unit needs.
pub(crate) fn param_text(out: &mut String, p: &ParamInfo, value: f32) {
    let v = shown(p, value);
    let _ = match p.unit {
        Unit::Hz | Unit::Share => write!(out, "{v:.0}"),
        Unit::Ms if v >= 100.0 => write!(out, "{v:.0}"),
        Unit::Seconds | Unit::Number => write!(out, "{v:.2}"),
        Unit::Db | Unit::Ms | Unit::Ratio => write!(out, "{v:.1}"),
    };
}

/// A parameter's name as the inspector shows it: its label and unit ("attack ms").
pub(crate) fn param_name(out: &mut String, p: &ParamInfo) {
    out.clear();
    out.extend(p.label.chars().flat_map(char::to_lowercase));
    let unit = match p.unit {
        Unit::Db => " dB",
        Unit::Hz => " Hz",
        Unit::Ms => " ms",
        Unit::Seconds => " s",
        Unit::Share => " %",
        Unit::Ratio | Unit::Number => "",
    };
    out.push_str(unit);
}

/// Bus `bus`'s chain (the master's for [`MASTER`]); empty for a bus the layout lacks.
pub(crate) fn chain<'a>(layout: &'a MixerFile, bus: &str) -> &'a [EffectLayout] {
    layout.effects(bus).unwrap_or(&[])
}

fn chain_mut<'a>(layout: &'a mut MixerFile, bus: &str) -> Option<&'a mut Vec<EffectLayout>> {
    if bus == MASTER {
        Some(&mut layout.master.effects)
    } else {
        layout.buses.get_mut(bus).map(|b| &mut b.effects)
    }
}

fn effect_mut<'a>(layout: &'a mut MixerFile, bus: &str, k: usize) -> Option<&'a mut EffectLayout> {
    chain_mut(layout, bus)?.get_mut(k)
}

/// An effect of `kind` at the end of bus `bus`'s chain, every parameter at its default,
/// written out: its place.
pub(crate) fn add_effect(layout: &mut MixerFile, bus: &str, kind: &str) -> Option<usize> {
    let chain = chain_mut(layout, bus)?;
    chain.push(EffectLayout::new(kind));
    Some(chain.len() - 1)
}

/// Effect `k` of bus `bus` taken away.
pub(crate) fn remove_effect(layout: &mut MixerFile, bus: &str, k: usize) {
    if let Some(chain) = chain_mut(layout, bus)
        && k < chain.len()
    {
        chain.remove(k);
        // The snapshots' values of it go; those of the effects after it follow them.
        renumber_effects(layout, bus, |j| match j.cmp(&k) {
            core::cmp::Ordering::Less => Some(j),
            core::cmp::Ordering::Equal => None,
            core::cmp::Ordering::Greater => Some(j - 1),
        });
    }
}

/// Effect `k` of bus `bus` moved to place `to` (past the end: last): its place now.
pub(crate) fn move_effect(layout: &mut MixerFile, bus: &str, k: usize, to: usize) -> Option<usize> {
    let chain = chain_mut(layout, bus)?;
    if k >= chain.len() {
        return None;
    }
    let to = to.min(chain.len() - 1);
    let effect = chain.remove(k);
    chain.insert(to, effect);
    // The snapshots' values follow the effects to their places.
    renumber_effects(layout, bus, |j| {
        Some(if j == k {
            to
        } else if k < to && (k + 1..=to).contains(&j) {
            j - 1
        } else if to < k && (to..k).contains(&j) {
            j + 1
        } else {
            j
        })
    });
    Some(to)
}

/// Each snapshot's paths rewritten by `f` (a path: its new one, or none to drop the value):
/// what renames, moves and removals do to the values snapshots set (ADR 0122).
fn retarget(layout: &mut MixerFile, mut f: impl FnMut(&str) -> Option<String>) {
    for s in &mut layout.snapshots {
        let old = core::mem::take(&mut s.values);
        for (path, value) in old {
            if let Some(path) = f(&path) {
                s.values.insert(path, value);
            }
        }
    }
}

/// The snapshots' values of bus `bus`'s effects moved to the places `place` gives each
/// one's old place (none: the effect is gone, and its values with it).
fn renumber_effects(layout: &mut MixerFile, bus: &str, place: impl Fn(usize) -> Option<usize>) {
    let head = format!("{bus}/effects/");
    retarget(layout, |path| {
        let Some(rest) = path.strip_prefix(&head) else {
            return Some(path.to_owned());
        };
        let (k, tail) = rest.split_once('/')?;
        let k: usize = k.parse().ok()?;
        place(k).map(|j| format!("{head}{j}/{tail}"))
    });
}

/// Parameter `name` of effect `k` of bus `bus` set to `value` (in its unit), within its
/// range.
pub(crate) fn set_param(layout: &mut MixerFile, bus: &str, k: usize, name: &str, value: f32) {
    if let Some(e) = effect_mut(layout, bus, k)
        && let Some(p) = effect_info(&e.kind).and_then(|i| i.param(name))
    {
        e.params.insert(name.to_owned(), p.clamp(value));
    }
}

/// Effect `k` of bus `bus` bypassed, or back.
pub(crate) fn toggle_bypass(layout: &mut MixerFile, bus: &str, k: usize) {
    if let Some(e) = effect_mut(layout, bus, k) {
        e.bypass = !e.bypass;
    }
}

/// B: every effect of bus `bus` bypassed, or (when every one is) every one back.
pub(crate) fn bypass_all(layout: &mut MixerFile, bus: &str) {
    if let Some(chain) = chain_mut(layout, bus) {
        let off = !chain.iter().all(|e| e.bypass);
        for e in chain {
            e.bypass = off;
        }
    }
}

/// Filter `k` of bus `bus` given the shape `kind`: the frequency, Q and gain it has kept
/// where the new shape has them, the new shape's defaults for the rest.
pub(crate) fn set_shape(layout: &mut MixerFile, bus: &str, k: usize, kind: &str) {
    let Some(info) = effect_info(kind).filter(|i| i.filter) else {
        return;
    };
    if let Some(e) = effect_mut(layout, bus, k)
        && effect_info(&e.kind).is_some_and(|i| i.filter)
    {
        let mut shaped = EffectLayout::new(kind);
        for p in info.params {
            if let Some(v) = e.param(p.name) {
                shaped.params.insert(p.name.to_owned(), v);
            }
        }
        shaped.bypass = e.bypass;
        *e = shaped;
    } else {
        return;
    }
    // The snapshots' values of parameters the new shape lacks go.
    let head = format!("{bus}/effects/{k}/");
    retarget(layout, |path| match path.strip_prefix(&head) {
        Some("bypass") | None => Some(path.to_owned()),
        Some(param) => info.param(param).map(|_| path.to_owned()),
    });
}

/// The buses that can key effect `k` of bus `bus` without looping the sound back, in the
/// layout's order.
pub(crate) fn sidechain_sources<'a>(layout: &'a MixerFile, bus: &str, k: usize) -> Vec<&'a str> {
    if chain(layout, bus).get(k).is_none() {
        return Vec::new();
    }
    // The layout without this sidechain: a source is offered when it is made and every
    // other route stays.
    let mut trial = layout.clone();
    set_sidechain(&mut trial, bus, k, "");
    let base = trial.routes();
    let (sends, sidechains) = (base.sends.len(), base.sidechains.len());
    layout
        .order()
        .into_iter()
        .map(|b| b.name)
        .filter(|&source| {
            if source == bus {
                return false;
            }
            set_sidechain(&mut trial, bus, k, source);
            let routes = trial.routes();
            routes.sidechains.contains(&(bus, k, source))
                && routes.sends.len() == sends
                && routes.sidechains.len() == sidechains + 1
        })
        .collect()
}

/// Effect `k` of bus `bus` keyed by bus `source` (empty: its own sound).
pub(crate) fn set_sidechain(layout: &mut MixerFile, bus: &str, k: usize, source: &str) {
    if let Some(e) = effect_mut(layout, bus, k) {
        source.clone_into(&mut e.sidechain);
    }
}

/// The buses bus `bus` can send into: not itself, not one it sends into already, none
/// that would loop the sound back; in the layout's order.
pub(crate) fn send_targets<'a>(layout: &'a MixerFile, bus: &str) -> Vec<&'a str> {
    let Some(sends) = layout.buses.get(bus).map(|b| &b.sends) else {
        return Vec::new();
    };
    // A target is offered when its send is made and every other route stays.
    let base = layout.routes();
    let (made, sidechains) = (base.sends.len(), base.sidechains.len());
    let mut trial = layout.clone();
    layout
        .order()
        .into_iter()
        .map(|b| b.name)
        .filter(|&to| {
            if to == bus || sends.contains_key(to) {
                return false;
            }
            add_send(&mut trial, bus, to);
            let routes = trial.routes();
            let fits = routes.sends.iter().any(|s| s.0 == bus && s.1 == to)
                && routes.sends.len() == made + 1
                && routes.sidechains.len() == sidechains;
            drop(routes);
            remove_send(&mut trial, bus, to);
            fits
        })
        .collect()
}

/// A send from bus `bus` into `to` at 0 dB, after the fader.
pub(crate) fn add_send(layout: &mut MixerFile, bus: &str, to: &str) {
    if let Some(b) = layout.buses.get_mut(bus) {
        b.sends.entry(to.to_owned()).or_default();
    }
}

/// Bus `bus`'s send into `to` taken away.
pub(crate) fn remove_send(layout: &mut MixerFile, bus: &str, to: &str) {
    if let Some(b) = layout.buses.get_mut(bus) {
        b.sends.remove(to);
    }
    let gone = format!("{bus}/sends/{to}");
    retarget(layout, |path| (path != gone).then(|| path.to_owned()));
}

/// Bus `bus`'s send into `to` at `db`, within a fader's travel.
pub(crate) fn set_send_gain(layout: &mut MixerFile, bus: &str, to: &str, db: f32) {
    if let Some(send) = layout.buses.get_mut(bus).and_then(|b| b.sends.get_mut(to)) {
        send.gain = db.clamp(FLOOR, CEILING);
    }
}

/// Bus `bus`'s send into `to` taken before its fader, or after.
pub(crate) fn toggle_send_tap(layout: &mut MixerFile, bus: &str, to: &str) {
    if let Some(send) = layout.buses.get_mut(bus).and_then(|b| b.sends.get_mut(to)) {
        send.pre_fader = !send.pre_fader;
    }
}

/// Whether bus `bus` (the master for [`MASTER`]) differs between layouts `before` and
/// `after`: its fader, balance, voices, reverb or output, or it came or went.
pub(crate) fn bus_changed(before: &MixerFile, after: &MixerFile, bus: &str) -> bool {
    match bus {
        MASTER => before.master != after.master,
        ROOM => before.room != after.room,
        _ => before.buses.get(bus) != after.buses.get(bus),
    }
}

/// How many buses (the master among them) differ between `before` and `after`, those that
/// came and went too.
pub(crate) fn changes(before: &MixerFile, after: &MixerFile) -> usize {
    let master =
        usize::from(before.master != after.master) + usize::from(before.room != after.room);
    let names = before.buses.keys().chain(
        after
            .buses
            .keys()
            .filter(|n| !before.buses.contains_key(*n)),
    );
    master + names.filter(|n| bus_changed(before, after, n)).count()
}

/// Bus `bus`'s gain set to `db` (the master's for [`MASTER`]), within the fader's travel.
pub(crate) fn set_gain(layout: &mut MixerFile, bus: &str, db: f32) {
    let db = (10.0 * db.clamp(FLOOR, CEILING)).round() / 10.0;
    if bus == MASTER {
        layout.master.gain = db;
    } else if bus == ROOM {
        layout.room.gain = db;
    } else if let Some(b) = layout.buses.get_mut(bus) {
        b.gain = db;
    }
}

/// Bus `bus`'s balance set to `v`, −1 (left) to 1 (right).
pub(crate) fn set_balance(layout: &mut MixerFile, bus: &str, v: f32) {
    let v = (100.0 * v.clamp(-1.0, 1.0)).round() / 100.0;
    if bus == MASTER {
        layout.master.balance = v;
    } else if let Some(b) = layout.buses.get_mut(bus) {
        b.balance = v;
    }
}

/// Bus `bus`'s voice limit set to `n` (0: none).
pub(crate) fn set_voices(layout: &mut MixerFile, bus: &str, n: u32) {
    if let Some(b) = layout.buses.get_mut(bus) {
        b.voices = n;
    }
}

/// Bus `bus`'s 3D sounds made to reverberate, or not.
pub(crate) fn toggle_reverb(layout: &mut MixerFile, bus: &str) {
    if let Some(b) = layout.buses.get_mut(bus) {
        b.reverb = !b.reverb;
    }
}

/// The buses `bus` can feed: the master first, then every bus but itself and those it is
/// fed by (that would close a loop), in the layout's order.
pub(crate) fn outputs<'a>(layout: &'a MixerFile, bus: &str) -> Vec<&'a str> {
    let mut out = vec![MASTER];
    out.extend(
        layout
            .order()
            .into_iter()
            .map(|b| b.name)
            .filter(|&b| b != bus && !feeds_into(layout, b, bus)),
    );
    out
}

/// Bus `bus` feeds `into`, through its parents.
fn feeds_into(layout: &MixerFile, bus: &str, into: &str) -> bool {
    let mut at = bus;
    for _ in 0..=layout.buses.len() {
        match layout.buses.get(at).map(|b| b.parent.as_str()) {
            Some(p) if p == into => return true,
            Some(p) if !p.is_empty() => at = p,
            _ => return false,
        }
    }
    false
}

/// Bus `bus` fed into `parent` ([`MASTER`] for the master).
///
/// # Errors
///
/// A parent the layout lacks, or one that would close a loop.
pub(crate) fn set_parent(layout: &mut MixerFile, bus: &str, parent: &str) -> Result<(), String> {
    if parent != MASTER && !layout.buses.contains_key(parent) {
        return Err(format!("there is no bus `{parent}`"));
    }
    if parent == bus || (parent != MASTER && feeds_into(layout, parent, bus)) {
        return Err(format!(
            "`{parent}` feeds `{bus}`: the sound would go round"
        ));
    }
    let Some(b) = layout.buses.get_mut(bus) else {
        return Err(format!("there is no bus `{bus}`"));
    };
    b.parent = if parent == MASTER {
        String::new()
    } else {
        parent.to_owned()
    };
    Ok(())
}

/// A new bus into the master: its name ("bus", or "bus 2" and on when taken).
pub(crate) fn add_bus(layout: &mut MixerFile) -> String {
    let name = (1..)
        .map(|n| {
            if n == 1 {
                String::from("bus")
            } else {
                format!("bus {n}")
            }
        })
        .find(|n| !layout.buses.contains_key(n))
        .unwrap_or_default();
    layout.buses.insert(name.clone(), BusLayout::new());
    name
}

/// Bus `bus` taken away: the buses it was fed by go to the bus it fed.
pub(crate) fn remove_bus(layout: &mut MixerFile, bus: &str) {
    let Some(gone) = layout.buses.remove(bus) else {
        return;
    };
    for b in layout.buses.values_mut() {
        if b.parent == bus {
            b.parent.clone_from(&gone.parent);
        }
        b.sends.remove(bus);
    }
    for e in every_effect(layout) {
        if e.sidechain == bus {
            e.sidechain.clear();
        }
    }
    // The snapshots' values of it, and of sends into it, go.
    let head = format!("{bus}/");
    retarget(layout, |path| {
        let into = path.split_once("/sends/").is_some_and(|(_, to)| to == bus);
        (!path.starts_with(&head) && !into).then(|| path.to_owned())
    });
}

/// Every effect of the layout, the master's among them.
fn every_effect(layout: &mut MixerFile) -> impl Iterator<Item = &mut EffectLayout> {
    layout
        .master
        .effects
        .iter_mut()
        .chain(layout.buses.values_mut().flat_map(|b| b.effects.iter_mut()))
}

/// Bus `from` called `to`, the buses it is fed by following.
///
/// # Errors
///
/// An empty name, one the player keeps for itself, or one another bus has.
pub(crate) fn rename_bus(layout: &mut MixerFile, from: &str, to: &str) -> Result<(), String> {
    let to = to.trim();
    if to == from {
        return Ok(());
    }
    if MixerFile::is_kept(to) {
        return Err(format!(
            "`{to}` is a name the player keeps for a bus of its own"
        ));
    }
    if to.contains('/') {
        return Err(String::from(
            "a bus's name cannot hold `/` (snapshots name values by paths)",
        ));
    }
    if layout.buses.contains_key(to) {
        return Err(format!("there is a bus `{to}` already"));
    }
    let Some(bus) = layout.buses.remove(from) else {
        return Err(format!("there is no bus `{from}`"));
    };
    layout.buses.insert(to.to_owned(), bus);
    for b in layout.buses.values_mut() {
        if b.parent == from {
            to.clone_into(&mut b.parent);
        }
        if let Some(send) = b.sends.remove(from) {
            b.sends.insert(to.to_owned(), send);
        }
    }
    for e in every_effect(layout) {
        if e.sidechain == from {
            to.clone_into(&mut e.sidechain);
        }
    }
    // The snapshots' values of it, and of sends into it, follow the name.
    let head = format!("{from}/");
    retarget(layout, |path| {
        let path = match path.strip_prefix(&head) {
            Some(rest) => format!("{to}/{rest}"),
            None => path.to_owned(),
        };
        Some(match path.split_once("/sends/") {
            Some((bus, into)) if into == from => format!("{bus}/sends/{to}"),
            _ => path,
        })
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_chosen_snapshot_takes_the_panels_value_edits_and_refuses_the_rest() {
        let mut layout = MixerFile::builtin();
        add_effect(&mut layout, "music", "low_pass");
        let mut doc = MixerDoc::new(AssetId::new(), "main.mixer", layout);
        let (_, _) = doc
            .edit_mix(|l| {
                add_snapshot(l);
            })
            .unwrap();
        doc.choose(Some("snapshot"));
        assert_eq!(doc.chosen.as_deref(), Some("snapshot"));
        // A fader and a parameter edited: the snapshot's values, the mix kept.
        doc.edit(|l| set_gain(l, "music", -12.0)).unwrap();
        doc.edit(|l| set_param(l, "music", 0, "frequency_hz", 800.0))
            .unwrap();
        let s = doc.layout.snapshot("snapshot").unwrap();
        assert_eq!(s.values.get("music/gain"), Some(&-12.0));
        assert_eq!(s.values.get("music/effects/0/frequency_hz"), Some(&800.0));
        assert_eq!(s.values.len(), 2, "only what changed: {:?}", s.values);
        assert_eq!(doc.layout.buses["music"].gain, 0.0, "the mix as it was");
        // What the panel shows is the snapshot over the mix: the strip marked.
        assert_eq!(doc.gain("music"), Some(-12.0));
        let music = doc.strips.iter().find(|s| s.name == "music").unwrap();
        assert!(music.marked && music.gain_text == "\u{2212}12.0");
        assert!(doc.sets("music/gain") && !doc.sets("ui/gain"));
        // Buses and effects are the mix's: refused while a snapshot is chosen.
        assert!(
            doc.edit(|l| {
                add_bus(l);
            })
            .is_none()
        );
        assert!(doc.refused.take().unwrap().contains("choose Mix"));
        assert!(
            doc.edit(|l| {
                add_effect(l, "ui", "delay");
            })
            .is_none()
        );
        // ↺ takes a value out.
        doc.forget("music/gain").unwrap();
        assert_eq!(doc.gain("music"), Some(0.0));
        assert!(!doc.sets("music/gain"));
        // The mix chosen again: edits go to it.
        doc.choose(None);
        doc.edit(|l| set_gain(l, "ui", -3.0)).unwrap();
        assert_eq!(doc.layout.buses["ui"].gain, -3.0);
        assert_eq!(doc.layout.snapshots[0].values.len(), 1);
        // Its list: renamed, its fade, taken away (the choice with it).
        doc.choose(Some("snapshot"));
        doc.edit_mix(|l| rename_snapshot(l, "snapshot", "dive").unwrap())
            .unwrap();
        assert!(doc.chosen.is_none(), "the old name is gone");
        doc.choose(Some("dive"));
        doc.edit_mix(|l| set_snapshot_fade(l, "dive", 1.234))
            .unwrap();
        assert_eq!(doc.layout.snapshot("dive").unwrap().fade, 1.23);
        assert!(rename_snapshot(&mut doc.layout.clone(), "dive", " ").is_err());
        doc.edit_mix(|l| remove_snapshot(l, "dive")).unwrap();
        assert!(doc.chosen.is_none() && doc.layout.snapshots.is_empty());
    }

    #[test]
    fn snapshots_values_follow_renames_moves_and_removals() {
        let mut l = MixerFile::builtin();
        add_effect(&mut l, "music", "compressor");
        add_effect(&mut l, "music", "low_pass");
        add_effect(&mut l, "music", "delay");
        add_send(&mut l, "effects", "music");
        l.snapshots.push(
            engine_app::SnapshotLayout::new("dive")
                .with("music/gain", -12.0)
                .with("music/effects/0/threshold_db", -30.0)
                .with("music/effects/1/frequency_hz", 600.0)
                .with("music/effects/2/time_ms", 300.0)
                .with("effects/sends/music", -6.0)
                .with("ui/gain", -3.0),
        );
        let paths = |l: &MixerFile| l.snapshots[0].values.keys().cloned().collect::<Vec<_>>();
        // The low-pass moved first: its values and the compressor's follow.
        move_effect(&mut l, "music", 1, 0);
        assert_eq!(
            paths(&l),
            [
                "effects/sends/music",
                "music/effects/0/frequency_hz",
                "music/effects/1/threshold_db",
                "music/effects/2/time_ms",
                "music/gain",
                "ui/gain",
            ]
        );
        // The compressor removed: its value goes, the delay's moves up.
        remove_effect(&mut l, "music", 1);
        assert!(paths(&l).contains(&String::from("music/effects/1/time_ms")));
        assert!(!paths(&l).iter().any(|p| p.contains("threshold_db")));
        // A shape without the parameter: its value goes.
        set_shape(&mut l, "music", 0, "high_pass");
        assert!(paths(&l).contains(&String::from("music/effects/0/frequency_hz")));
        // Music renamed: its values and the send into it follow.
        rename_bus(&mut l, "music", "score").unwrap();
        assert_eq!(
            paths(&l),
            [
                "effects/sends/score",
                "score/effects/0/frequency_hz",
                "score/effects/1/time_ms",
                "score/gain",
                "ui/gain",
            ]
        );
        assert!(
            rename_bus(&mut l, "score", "a/b").is_err(),
            "no `/` in names"
        );
        // The send removed, then the bus: their values go.
        remove_send(&mut l, "effects", "score");
        assert!(!paths(&l).contains(&String::from("effects/sends/score")));
        remove_bus(&mut l, "score");
        assert_eq!(paths(&l), ["ui/gain"]);
        assert!(l.problems().is_empty(), "{:?}", l.problems());
    }

    #[test]
    fn the_faders_travel_has_more_room_near_nothing() {
        assert_eq!(fader_position(0.0), 0.8);
        assert_eq!(fader_position(CEILING), 1.0);
        assert_eq!(fader_position(FLOOR), 0.0);
        assert_eq!(fader_position(-200.0), 0.0);
        for db in [-60.0, -24.0, -12.0, -6.0, -1.5, 0.0, 3.0] {
            assert!((fader_gain(fader_position(db)) - db).abs() < 0.11, "{db}");
        }
        // Six dB under nothing take a tenth of the travel's top fifth... more than six
        // dB at the bottom.
        let top = fader_position(0.0) - fader_position(-6.0);
        let bottom = fader_position(-54.0) - fader_position(-60.0);
        assert!(top > 2.0 * bottom, "{top} against {bottom}");
        let mut s = String::new();
        gain_text(&mut s, -6.04);
        assert_eq!(s, "\u{2212}6.0");
        gain_text(&mut s, 0.0);
        assert_eq!(s, "0.0");
        gain_text(&mut s, FLOOR);
        assert_eq!(s, "\u{2212}\u{221e}");
    }

    #[test]
    fn buses_are_added_renamed_moved_and_removed_keeping_the_layout_whole() {
        let mut layout = MixerFile::builtin();
        let first = add_bus(&mut layout);
        let second = add_bus(&mut layout);
        assert_eq!((first.as_str(), second.as_str()), ("bus", "bus 2"));
        // Under effects; effects cannot then feed it, nor it itself.
        set_parent(&mut layout, "bus", "effects").unwrap();
        assert!(set_parent(&mut layout, "effects", "bus").is_err());
        assert!(set_parent(&mut layout, "bus", "bus").is_err());
        assert!(set_parent(&mut layout, "bus", "nowhere").is_err());
        let outs = outputs(&layout, "effects");
        assert_eq!(outs[0], MASTER);
        assert!(
            !outs.contains(&"effects") && !outs.contains(&"bus"),
            "{outs:?}"
        );
        // Renamed, its children follow.
        set_parent(&mut layout, "bus 2", "bus").unwrap();
        rename_bus(&mut layout, "bus", "steps").unwrap();
        assert_eq!(layout.buses["bus 2"].parent, "steps");
        assert!(rename_bus(&mut layout, "steps", "music").is_err(), "taken");
        assert!(rename_bus(&mut layout, "steps", "reverb").is_err(), "kept");
        assert!(rename_bus(&mut layout, "steps", " ").is_err(), "empty");
        // Removed, its children go to the bus it fed.
        remove_bus(&mut layout, "steps");
        assert_eq!(layout.buses["bus 2"].parent, "effects");
        // Values within their ranges.
        set_gain(&mut layout, "music", -120.0);
        assert_eq!(layout.buses["music"].gain, FLOOR);
        set_gain(&mut layout, MASTER, -3.04);
        assert_eq!(layout.master.gain, -3.0);
        set_balance(&mut layout, "music", 2.0);
        assert_eq!(layout.buses["music"].balance, 1.0);
        toggle_reverb(&mut layout, "music");
        assert!(layout.buses["music"].reverb);
    }

    #[test]
    fn a_meter_falls_back_holds_its_peak_and_lights_at_full_scale() {
        let mut m = Meter::new("music");
        m.feed([0.5, 0.25], 0.0);
        assert!((m.level[0] + 6.02).abs() < 0.05 && (m.level[1] + 12.04).abs() < 0.05);
        assert_eq!(m.text, "\u{2212}6.0");
        // Silence: half a second later 12 dB lower, the peak held.
        m.feed([0.0, 0.0], 0.5);
        assert!((m.level[0] + 18.02).abs() < 0.05, "{:?}", m.level);
        assert_eq!(m.text, "\u{2212}6.0");
        assert!(!m.clip);
        m.feed([1.2, 0.1], 0.016);
        assert!(m.clip, "over full scale");
        assert_eq!(m.text, "+1.6");
        m.clear();
        assert!(!m.clip && m.text.is_empty());
        assert_eq!(meter_position(METER_TOP), 1.0);
        assert_eq!(meter_position(-100.0), 0.0);
    }

    #[test]
    fn soloing_keeps_the_way_to_the_master_and_what_feeds_it() {
        let mut layout = MixerFile::builtin();
        layout
            .buses
            .insert(String::from("steps"), BusLayout::new().under("effects"));
        let s = |n: &[&str]| n.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        let on = |muted: &[&str], soloed: &[&str], name: &str| {
            !silenced(&layout, &s(muted), &s(soloed), name)
        };
        assert!(on(&[], &["effects"], "steps") && on(&[], &["effects"], "effects"));
        assert!(!on(&[], &["effects"], "music"));
        assert!(on(&[], &["steps"], "effects"), "what it feeds");
        assert!(on(&["master"], &["music"], MASTER), "the master never");
        assert!(!on(&["voice"], &[], "voice"));
    }

    #[test]
    fn a_chain_is_edited_effect_by_effect() {
        let mut layout = MixerFile::builtin();
        assert_eq!(add_effect(&mut layout, "music", "compressor"), Some(0));
        assert_eq!(add_effect(&mut layout, "music", "low_pass"), Some(1));
        assert_eq!(add_effect(&mut layout, MASTER, "limiter"), Some(0));
        assert_eq!(add_effect(&mut layout, "nowhere", "delay"), None);
        // Every parameter written out at its default.
        let comp = &chain(&layout, "music")[0];
        assert_eq!(comp.params.len(), 6);
        assert_eq!(comp.params["ratio"], 4.0);
        // A parameter within its range; the filter moved first, then given a shelf's shape:
        // its frequency kept, a gain at its default.
        set_param(&mut layout, "music", 0, "ratio", 50.0);
        assert_eq!(chain(&layout, "music")[0].params["ratio"], 20.0);
        set_param(&mut layout, "music", 1, "frequency_hz", 500.0);
        assert_eq!(move_effect(&mut layout, "music", 1, 0), Some(0));
        set_shape(&mut layout, "music", 0, "low_shelf");
        let shelf = &chain(&layout, "music")[0];
        assert_eq!(shelf.kind, "low_shelf");
        assert_eq!(shelf.params["frequency_hz"], 500.0);
        assert_eq!(shelf.params["gain_db"], 0.0);
        set_shape(&mut layout, "music", 1, "peak");
        assert_eq!(
            chain(&layout, "music")[1].kind,
            "compressor",
            "not a filter"
        );
        // B: all off, then all back; one alone.
        bypass_all(&mut layout, "music");
        assert!(chain(&layout, "music").iter().all(|e| e.bypass));
        bypass_all(&mut layout, "music");
        assert!(chain(&layout, "music").iter().all(|e| !e.bypass));
        toggle_bypass(&mut layout, "music", 1);
        assert!(chain(&layout, "music")[1].bypass);
        remove_effect(&mut layout, "music", 0);
        assert_eq!(chain(&layout, "music").len(), 1);
        assert_eq!(chain(&layout, "music")[0].kind, "compressor");
        // The strips say so.
        let doc = MixerDoc::new(AssetId::new(), "main.mixer", layout);
        let music = &doc.strips[doc.strip("music").unwrap()];
        assert_eq!(music.effects, "Comp");
        assert!(music.bypassed);
        assert_eq!(doc.strips[0].effects, "Limit");
        // Every built-in kind has a short name.
        for info in EFFECTS {
            assert_ne!(short_name(info.kind), info.kind, "{}", info.kind);
        }
        assert!(doc.strips[doc.strip("ui").unwrap()].effects.is_empty());
    }

    #[test]
    fn sends_and_sidechains_are_offered_only_where_they_would_not_loop() {
        let mut layout = MixerFile::builtin();
        layout
            .buses
            .insert(String::from("steps"), BusLayout::new().under("effects"));
        // Effects can send anywhere but itself and steps (which feeds it).
        let targets = send_targets(&layout, "effects");
        assert_eq!(targets, ["music", "ui", "voice"]);
        add_send(&mut layout, "effects", "music");
        set_send_gain(&mut layout, "effects", "music", -120.0);
        assert_eq!(layout.buses["effects"].sends["music"].gain, FLOOR);
        toggle_send_tap(&mut layout, "effects", "music");
        assert!(layout.buses["effects"].sends["music"].pre_fader);
        // Music cannot send back into effects (effects sends into it), nor into steps
        // (which feeds effects); nor twice into one bus.
        let targets = send_targets(&layout, "music");
        assert!(!targets.contains(&"effects") && !targets.contains(&"steps"));
        assert!(targets.contains(&"ui"));
        add_send(&mut layout, "music", "ui");
        assert!(!send_targets(&layout, "music").contains(&"ui"));
        // A compressor on music: voice or effects can key it; ui, which music sends into,
        // would loop the sound back.
        add_effect(&mut layout, "music", "compressor");
        let sources = sidechain_sources(&layout, "music", 0);
        assert!(sources.contains(&"voice") && sources.contains(&"effects"));
        assert!(!sources.contains(&"ui") && !sources.contains(&"music"));
        set_sidechain(&mut layout, "music", 0, "voice");
        assert_eq!(chain(&layout, "music")[0].sidechain, "voice");
        // A rename follows into sends and sidechains; a removal takes them away.
        rename_bus(&mut layout, "voice", "dialogue").unwrap();
        assert_eq!(chain(&layout, "music")[0].sidechain, "dialogue");
        rename_bus(&mut layout, "music", "score").unwrap();
        assert!(layout.buses["effects"].sends.contains_key("score"));
        remove_bus(&mut layout, "dialogue");
        assert!(chain(&layout, "score")[0].sidechain.is_empty());
        remove_bus(&mut layout, "score");
        assert!(layout.buses["effects"].sends.is_empty());
        assert!(layout.buses["ui"].sends.is_empty());
        remove_send(&mut layout, "effects", "nowhere");
        let doc = MixerDoc::new(AssetId::new(), "main.mixer", {
            add_send(&mut layout, "effects", "ui");
            layout
        });
        assert_eq!(
            doc.strips[doc.strip("effects").unwrap()].output,
            "\u{2192} master +1"
        );
    }

    #[test]
    fn parameters_are_shown_in_their_units() {
        let comp = effect_info("compressor").unwrap();
        let delay = effect_info("delay").unwrap();
        let (mut name, mut text) = (String::new(), String::new());
        param_name(&mut name, comp.param("attack_ms").unwrap());
        assert_eq!(name, "attack ms");
        param_name(&mut name, delay.param("mix").unwrap());
        assert_eq!(name, "mix %");
        let mix = delay.param("mix").unwrap();
        param_text(&mut text, mix, 0.35);
        assert_eq!(text, "35");
        assert_eq!(stored(mix, 50.0), 0.5);
        let rows: Vec<EffectRow> = EffectRow::of("peak").collect();
        assert_eq!(rows[0], EffectRow::Shape);
        assert_eq!(rows[1], EffectRow::Bypass);
        assert_eq!(rows.len(), 2 + 3, "shape, bypass, frequency, q, gain");
        assert_eq!(EffectRow::at("compressor", 1), Some(EffectRow::Sidechain));
        assert_eq!(
            EffectRow::of("chorus").collect::<Vec<_>>(),
            [EffectRow::Unknown]
        );
        assert_eq!(filters().count(), 8);
    }

    #[test]
    fn changes_between_layouts_are_counted_bus_by_bus() {
        let before = MixerFile::builtin();
        let mut after = before.clone();
        assert_eq!(changes(&before, &after), 0);
        set_gain(&mut after, "music", -6.0);
        add_bus(&mut after);
        remove_bus(&mut after, "ui");
        set_gain(&mut after, MASTER, -1.0);
        assert!(bus_changed(&before, &after, "music") && bus_changed(&before, &after, "bus"));
        assert!(!bus_changed(&before, &after, "effects"));
        assert!(bus_changed(&before, &after, MASTER));
        assert_eq!(changes(&before, &after), 4, "music, bus, ui and the master");
    }

    #[test]
    fn the_doc_is_unsaved_once_edited_and_its_text_comes_back() {
        let dir = std::env::temp_dir().join(format!("ofelia-mixer-doc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No file: the built-in buses, not unsaved until edited.
        let mut doc = MixerDoc::open(&dir, "main.mixer").unwrap();
        assert!(!doc.exists && !doc.dirty());
        assert_eq!(
            doc.strips
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            [MASTER, "effects", "music", "ui", "voice", ROOM],
            "the room's reverb at the end"
        );
        let (before, after) = doc.edit(|l| set_gain(l, "music", -6.0)).unwrap();
        assert!(doc.dirty());
        assert_eq!(doc.strips[2].gain_text, "\u{2212}6.0");
        assert!(
            doc.edit(|l| set_gain(l, "music", -6.0)).is_none(),
            "no change"
        );
        doc.put_text(&before).unwrap();
        assert_eq!(doc.gain("music"), Some(0.0));
        doc.put_text(&after).unwrap();
        // Depths and outputs follow the tree.
        doc.edit(|l| {
            let world = add_bus(l);
            set_parent(l, "music", &world).unwrap();
        })
        .unwrap();
        let music = &doc.strips[doc.strip("music").unwrap()];
        assert_eq!((music.depth, music.output.as_str()), (2, "\u{2192} bus"));
        // Saved and opened again: its id and buses.
        std::fs::write(dir.join("main.mixer"), &doc.text).unwrap();
        doc.saved_now();
        let again = MixerDoc::open(&dir, "main.mixer").unwrap();
        assert_eq!((again.id, &again.layout), (doc.id, &doc.layout));
        assert!(!again.dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
