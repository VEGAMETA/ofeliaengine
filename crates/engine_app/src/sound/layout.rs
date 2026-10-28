//! The mixer's layout file (ADR 0121; team decision, 2026-10-28): `.mixer`, in the editor's
//! grammar (ADR 0108), named by [`AudioSettings::layout`](super::AudioSettings::layout)
//! (`main.mixer` under the project). Without the file, the built-in four buses play
//! ([`MixerFile::builtin`]).
//!
//! ```text
//! [mixer version=1 id=6b1f0c2e9d8a47b3a5c4e3f2d1c0b9a8]
//!
//! master.gain = 0
//! master.balance = 0
//! buses["effects"].parent = ""
//! buses["effects"].gain = -3
//! buses["effects"].balance = 0
//! buses["effects"].voices = 32
//! buses["effects"].reverb = true
//! buses["music"].parent = ""
//! buses["music"].gain = -8
//! buses["music"].balance = 0
//! buses["music"].voices = 0
//! buses["music"].reverb = false
//! ```
//!
//! - **A bus by its name,** under the master or the bus its `parent` names. Gains are in dB
//!   (0 passes the sound as it comes, −6 halves it), balances from −1 (left) to 1 (right);
//!   `voices` limits the voices mixed on it at once (0: no limit); `reverb` makes its 3D
//!   sounds reverberate with the room around the listener.
//! - **The order buses are made in** ([`MixerFile::order`]): each after its parent,
//!   siblings by name. A parent the file lacks, or parents that loop, put the bus under the
//!   master; [`MixerFile::problems`] says so, as it does for a name the player keeps for
//!   itself (`master`, `reverb`, `editor preview`, and names ending `.world`).
//! - **The game's volumes** (`Mixer::set_volume`) multiply the faders: an options menu at
//!   80 % keeps the mix's −6 dB.

use engine_reflect::Reflect;
use engine_scene::{AssetId, LoadError, ValueReport};
use std::collections::BTreeMap;
use std::path::Path;

/// The extension of mixer layout files.
pub const MIXER_FILE_EXTENSION: &str = "mixer";

/// Names the player gives buses of its own: a layout cannot use them.
const KEPT: [&str; 3] = ["master", "reverb", "editor preview"];

/// The master's fader and balance.
#[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
pub struct MasterLayout {
    /// Its gain in dB: 0 passes the sound as it comes.
    pub gain: f32,
    /// Its balance: −1 left, 0 the middle, 1 right.
    #[reflect(min = -1.0, max = 1.0)]
    pub balance: f32,
}

/// A bus of the layout (its name is its key in [`MixerFile::buses`]).
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct BusLayout {
    /// The bus it feeds, by name; empty: the master.
    pub parent: String,
    /// Its gain in dB: 0 passes the sound as it comes, −6 halves it.
    pub gain: f32,
    /// Its balance: −1 left, 0 the middle, 1 right.
    #[reflect(min = -1.0, max = 1.0)]
    pub balance: f32,
    /// Voices mixed on it at once, the least important made virtual beyond them; 0: no
    /// limit.
    pub voices: u32,
    /// Its 3D sounds reverberate with the room around the listener (effects and voices
    /// do; music and the UI do not). Its plain sounds never do.
    pub reverb: bool,
}

impl BusLayout {
    /// A bus at 0 dB into the master, dry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Its 3D sounds reverberate.
    pub fn reverberant(mut self) -> Self {
        self.reverb = true;
        self
    }

    /// Fed into the bus `parent`.
    pub fn under(mut self, parent: &str) -> Self {
        parent.clone_into(&mut self.parent);
        self
    }

    /// At `gain` dB.
    pub fn at(mut self, gain: f32) -> Self {
        self.gain = gain;
        self
    }
}

/// A mixer layout: the master's fader and the buses under it.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct MixerFile {
    /// The master's fader and balance.
    pub master: MasterLayout,
    /// The buses, by name.
    pub buses: BTreeMap<String, BusLayout>,
}

/// A bus where the layout makes it: its name, its layout, and the bus it feeds (`None`:
/// the master).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedBus<'a> {
    /// Its name.
    pub name: &'a str,
    /// What the file says of it.
    pub layout: &'a BusLayout,
    /// The bus it feeds as made (its parent, or the master when the parent is missing,
    /// loops, or is a name the player keeps).
    pub parent: Option<&'a str>,
}

impl MixerFile {
    /// The kind in the file's header.
    pub const KIND: &'static str = "mixer";
    /// The format's version.
    pub const VERSION: u32 = 1;

    /// What plays without a file: music, effects, voice and ui into the master at 0 dB,
    /// the 3D sounds of effects and voice reverberating.
    pub fn builtin() -> Self {
        let mut buses = BTreeMap::new();
        buses.insert(String::from("music"), BusLayout::new());
        buses.insert(String::from("effects"), BusLayout::new().reverberant());
        buses.insert(String::from("voice"), BusLayout::new().reverberant());
        buses.insert(String::from("ui"), BusLayout::new());
        Self {
            master: MasterLayout::default(),
            buses,
        }
    }

    /// The file's text, with its id.
    pub fn to_text(&self, id: AssetId) -> String {
        let mut out = String::new();
        engine_scene::write_value(Self::KIND, Self::VERSION, Some(id), self, None, &mut out);
        out
    }

    /// A layout from its file's text (a file without buses has none: the built-in four are
    /// only what plays without a file), and what reading it reported.
    ///
    /// # Errors
    ///
    /// Not a mixer file, a newer one, or a bad line (with its number).
    pub fn parse(text: &str) -> Result<(Self, ValueReport), LoadError> {
        let mut file = Self::default();
        let report = engine_scene::read_value(Self::KIND, Self::VERSION, text, &mut file)?;
        Ok((file, report))
    }

    /// Reads and parses the file at `path`.
    ///
    /// # Errors
    ///
    /// When it cannot be read or parsed, with its path.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text)
            .map(|(file, _)| file)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The buses in the order they are made, the tree depth first from the master (as the
    /// mixer panel's strips stand): each after the bus it feeds, siblings by name. A bus
    /// whose parent is missing, is a name the player keeps, or loops back to it, feeds the
    /// master; a bus with a name the player keeps is left out.
    pub fn order(&self) -> Vec<PlacedBus<'_>> {
        let feeds = self.feeds();
        // A bus's children, the last first (popped first-first).
        fn children<'a>(
            feeds: &BTreeMap<&'a str, Option<&'a str>>,
            of: Option<&str>,
        ) -> Vec<&'a str> {
            feeds
                .iter()
                .filter(|(_, parent)| **parent == of)
                .map(|(name, _)| *name)
                .rev()
                .collect()
        }
        let mut placed = Vec::with_capacity(feeds.len());
        let mut stack = children(&feeds, None);
        while let Some(name) = stack.pop() {
            placed.push(PlacedBus {
                name,
                layout: &self.buses[name],
                parent: feeds[name],
            });
            stack.extend(children(&feeds, Some(name)));
        }
        placed
    }

    /// Each bus the player makes and the bus it feeds as made (`None`: the master): its own
    /// parent when the file has it and it is not a name the player keeps; a loop of
    /// parents broken at its first bus by name.
    fn feeds(&self) -> BTreeMap<&str, Option<&str>> {
        let usable = |name: &str| self.buses.contains_key(name) && !kept(name);
        let mut feeds: BTreeMap<&str, Option<&str>> = self
            .buses
            .iter()
            .filter(|(name, _)| !kept(name))
            .map(|(name, bus)| {
                let parent = bus.parent.as_str();
                (name.as_str(), usable(parent).then_some(parent))
            })
            .collect();
        let names: Vec<&str> = feeds.keys().copied().collect();
        for name in names {
            let mut at = feeds[name];
            for _ in 0..feeds.len() {
                match at {
                    Some(parent) if parent == name => {
                        feeds.insert(name, None);
                        break;
                    }
                    Some(parent) => at = feeds.get(parent).copied().flatten(),
                    None => break,
                }
            }
        }
        feeds
    }

    /// What is wrong with the layout, for the editor and the log: names the player keeps,
    /// parents the file lacks or the player keeps, parents that loop.
    pub fn problems(&self) -> Vec<String> {
        let feeds = self.feeds();
        let mut out = Vec::new();
        for (name, bus) in &self.buses {
            let parent = bus.parent.as_str();
            if kept(name) {
                out.push(format!(
                    "`{name}` is a name the player keeps for a bus of its own"
                ));
            } else if parent.is_empty() {
            } else if !self.buses.contains_key(parent) {
                out.push(format!(
                    "`{name}` feeds `{parent}`, which the layout does not have: it feeds the master"
                ));
            } else if kept(parent) {
                out.push(format!(
                    "`{name}` feeds `{parent}`, a bus of the player's own: it feeds the master"
                ));
            } else if feeds.get(name.as_str()).copied().flatten().is_none() {
                out.push(format!(
                    "`{name}` feeds `{parent}`, which feeds it back: it feeds the master"
                ));
            }
        }
        out
    }
}

/// `name` is one the player keeps for a bus of its own.
fn kept(name: &str) -> bool {
    KEPT.contains(&name) || name.ends_with(".world") || name.is_empty()
}

/// A gain in dB as a linear factor.
pub(crate) fn linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn names(file: &MixerFile) -> Vec<(&str, Option<&str>)> {
        file.order().iter().map(|b| (b.name, b.parent)).collect()
    }

    #[test]
    fn a_layout_reads_back_as_it_was_written() {
        let mut file = MixerFile::builtin();
        file.master.gain = -1.5;
        file.buses.insert(
            String::from("steps"),
            BusLayout::new().under("effects").at(-6.0),
        );
        file.buses.get_mut("effects").unwrap().voices = 32;
        let id = AssetId::new();
        let text = file.to_text(id);
        assert!(
            text.starts_with(&format!("[mixer version=1 id={id}]")),
            "{text}"
        );
        assert!(
            text.contains("buses[\"steps\"].parent = \"effects\""),
            "{text}"
        );
        assert!(text.contains("buses[\"steps\"].gain = -6"), "{text}");
        let (back, report) = MixerFile::parse(&text).unwrap();
        assert_eq!(back, file);
        assert_eq!(report.id, Some(id));
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn a_file_names_its_buses_and_no_others() {
        // The built-in four are what plays without a file, not defaults a file adds to.
        let (file, _) = MixerFile::parse("[mixer version=1]\nbuses[\"sfx\"].gain = -3\n").unwrap();
        assert_eq!(file.buses.keys().collect::<Vec<_>>(), ["sfx"]);
        assert_eq!(file.buses["sfx"].gain, -3.0);
        assert!(MixerFile::parse("[material version=2]\n").is_err());
        assert!((linear(-6.0) - 0.501).abs() < 1e-3);
    }

    #[test]
    fn buses_are_made_after_their_parents_and_bad_parents_feed_the_master() {
        let mut file = MixerFile::default();
        let bus = |parent: &str| BusLayout::new().under(parent);
        file.buses.insert(String::from("world"), bus(""));
        file.buses.insert(String::from("music"), bus("world"));
        file.buses.insert(String::from("ambience"), bus("world"));
        file.buses.insert(String::from("birds"), bus("ambience"));
        file.buses.insert(String::from("ui"), bus(""));
        // A parent the file lacks, a loop, and a name the player keeps.
        file.buses.insert(String::from("lost"), bus("nowhere"));
        file.buses.insert(String::from("a"), bus("b"));
        file.buses.insert(String::from("b"), bus("a"));
        file.buses.insert(String::from("reverb"), bus(""));
        // Depth first, siblings by name; the loop broken at its first bus.
        assert_eq!(
            names(&file),
            [
                ("a", None),
                ("b", Some("a")),
                ("lost", None),
                ("ui", None),
                ("world", None),
                ("ambience", Some("world")),
                ("birds", Some("ambience")),
                ("music", Some("world")),
            ]
        );
        let problems = file.problems();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`lost` feeds `nowhere`")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`a` feeds `b`, which feeds it back")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("`reverb` is a name")),
            "{problems:?}"
        );
        assert!(MixerFile::builtin().problems().is_empty());
        assert_eq!(
            names(&MixerFile::builtin()),
            [
                ("effects", None),
                ("music", None),
                ("ui", None),
                ("voice", None)
            ]
        );
    }
}
