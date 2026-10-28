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
//!   output list leaves those out), a rename carries its children along, a removed bus
//!   hands its children to the bus it fed.

use engine_app::{BusLayout, MixerFile};
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
    if name == MASTER {
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
    /// Where it goes: "→ master", "→ world"; the master's "→ speakers".
    pub(crate) output: String,
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
        if bus == MASTER {
            &Self::OF_MASTER
        } else {
            &Self::ALL
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
        edit(&mut self.layout);
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
        self.refresh();
        Ok(())
    }

    /// Saved: as its file now holds it.
    pub(crate) fn saved_now(&mut self) {
        self.saved.clone_from(&self.text);
        self.exists = true;
    }

    /// The strips again from the layout.
    fn refresh(&mut self) {
        self.strips.clear();
        let mut master = Strip {
            name: String::from(MASTER),
            depth: 0,
            gain: self.layout.master.gain,
            gain_text: String::new(),
            output: String::from("\u{2192} speakers"),
        };
        gain_text(&mut master.gain_text, master.gain);
        self.strips.push(master);
        let order = self.layout.order();
        let mut depths: Vec<(&str, u8)> = Vec::with_capacity(order.len());
        for placed in &order {
            let depth = placed
                .parent
                .and_then(|p| depths.iter().find(|(n, _)| *n == p))
                .map_or(1, |&(_, d)| d.saturating_add(1));
            depths.push((placed.name, depth));
            let mut strip = Strip {
                name: placed.name.to_owned(),
                depth,
                gain: placed.layout.gain,
                gain_text: String::new(),
                output: format!("\u{2192} {}", placed.parent.unwrap_or(MASTER)),
            };
            gain_text(&mut strip.gain_text, strip.gain);
            self.strips.push(strip);
        }
    }

    /// The strip of bus `bus`.
    pub(crate) fn strip(&self, bus: &str) -> Option<usize> {
        self.strips.iter().position(|s| s.name == bus)
    }

    /// Bus `bus`'s gain in dB (the master's for [`MASTER`]).
    pub(crate) fn gain(&self, bus: &str) -> Option<f32> {
        if bus == MASTER {
            return Some(self.layout.master.gain);
        }
        self.layout.buses.get(bus).map(|b| b.gain)
    }
}

/// Bus `bus`'s gain set to `db` (the master's for [`MASTER`]), within the fader's travel.
pub(crate) fn set_gain(layout: &mut MixerFile, bus: &str, db: f32) {
    let db = (10.0 * db.clamp(FLOOR, CEILING)).round() / 10.0;
    if bus == MASTER {
        layout.master.gain = db;
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
    }
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

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
            [MASTER, "effects", "music", "ui", "voice"]
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
