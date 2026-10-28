//! The mixer's layout file (ADR 0121; team decision, 2026-10-28): `.mixer`, in the editor's
//! grammar (ADR 0108), named by [`AudioSettings::layout`](super::AudioSettings::layout)
//! (`main.mixer` under the project). Without the file, the built-in four buses play
//! ([`MixerFile::builtin`]).
//!
//! ```text
//! [mixer version=4 id=6b1f0c2e9d8a47b3a5c4e3f2d1c0b9a8]
//!
//! master.gain = 0
//! master.balance = 0
//! room.gain = -3
//! buses["effects"].parent = ""
//! buses["effects"].gain = -3
//! buses["effects"].balance = 0
//! buses["effects"].voices = 32
//! buses["effects"].reverb = true
//! buses["effects"].sends["canyon"].gain = -12
//! buses["effects"].sends["canyon"].pre_fader = false
//! buses["music"].parent = ""
//! buses["music"].gain = -8
//! buses["music"].balance = 0
//! buses["music"].voices = 0
//! buses["music"].reverb = false
//! buses["music"].effects.0.kind = "compressor"
//! buses["music"].effects.0.bypass = false
//! buses["music"].effects.0.sidechain = "voice"
//! buses["music"].effects.0.params["ratio"] = 4
//! buses["music"].effects.0.params["threshold_db"] = -24
//! snapshots.0.name = "underwater"
//! snapshots.0.fade = 0.5
//! snapshots.0.values["music/gain"] = -14
//! snapshots.0.values["music/effects/0/threshold_db"] = -30
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
//! - **Effects** (version 2), in the order the sound goes through them: a kind of
//!   [`engine_audio::dsp::EFFECTS`] by its name, bypassed or not, the bus keying it (a
//!   compressor on music keyed by the voice ducks the music under dialogue), and its
//!   parameters by their names, in their units (those not named keep their defaults, values
//!   beyond their range play at its end). The master has effects too.
//! - **Sends** (version 2), by the bus they feed: a level in dB, before or after the bus's
//!   fader. A send or a sidechain that would loop the sound back is left out
//!   ([`MixerFile::routes`]).
//! - **The room's reverb** (version 3): the player makes it and its parameters follow the
//!   room around the listener (the walls, a reverb zone); `room.gain` is its fader, a trim
//!   in dB on what it returns.
//! - **Snapshots** (version 4, ADR 0122): named sets of values by path
//!   ([`snapshot_target`]: `music/gain`, `music/balance`, `music/sends/canyon`,
//!   `music/effects/0/<parameter>`, `music/effects/0/bypass`, `master/...`, `room/gain`),
//!   layered by weight in the list's order, the later winning where they meet; a fade in
//!   seconds for a blend that names none.
//! - **The game's volumes** (`Mixer::set_volume`) multiply the faders: an options menu at
//!   80 % keeps the mix's −6 dB.

use engine_audio::dsp::effect_info;
use engine_reflect::Reflect;
use engine_scene::{AssetId, LoadError, ValueReport};
use std::collections::BTreeMap;
use std::path::Path;

/// The extension of mixer layout files.
pub const MIXER_FILE_EXTENSION: &str = "mixer";

/// Names the player gives buses of its own (and `room`, the reverb's in snapshots' paths):
/// a layout cannot use them.
const KEPT: [&str; 4] = ["master", "reverb", "room", "editor preview"];

/// The master's fader, balance and effects.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct MasterLayout {
    /// Its gain in dB: 0 passes the sound as it comes.
    pub gain: f32,
    /// Its balance: −1 left, 0 the middle, 1 right.
    #[reflect(min = -1.0, max = 1.0)]
    pub balance: f32,
    /// Its effects, in the order the sound goes through them (a limiter last, often).
    pub effects: Vec<EffectLayout>,
}

/// The room's reverb (version 3): made by the player, its parameters following the room
/// around the listener; only its fader is the file's.
#[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
pub struct RoomLayout {
    /// Its fader in dB: a trim on what the reverb returns (0 as the room gives it).
    pub gain: f32,
}

/// A snapshot (version 4, ADR 0122): the values it sets of the mix, by path, and the fade
/// a blend takes when it names none. What it does not name keeps the mix's value.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct SnapshotLayout {
    /// What `Mixer::blend` and `audio.blend` call it.
    pub name: String,
    /// Seconds a blend takes when it names none.
    #[reflect(min = 0.0)]
    pub fade: f32,
    /// The values it sets, by path ([`snapshot_target`]).
    pub values: BTreeMap<String, f32>,
}

impl Default for SnapshotLayout {
    fn default() -> Self {
        Self {
            name: String::new(),
            fade: 0.5,
            values: BTreeMap::new(),
        }
    }
}

impl SnapshotLayout {
    /// A snapshot called `name`, setting nothing yet.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }

    /// It sets `path` to `value`.
    #[must_use]
    pub fn with(mut self, path: &str, value: f32) -> Self {
        self.values.insert(path.to_owned(), value);
        self
    }
}

/// What a snapshot's path names (ADR 0122), by bus name (`master` for the master's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotTarget<'a> {
    /// A fader, in dB: `music/gain`, `master/gain`.
    Gain(&'a str),
    /// A balance, −1 to 1: `music/balance`.
    Balance(&'a str),
    /// The room's reverb's trim, in dB: `room/gain`.
    Room,
    /// A send's level, in dB: `music/sends/canyon`.
    Send {
        /// The bus it is taken from.
        bus: &'a str,
        /// The bus it feeds.
        to: &'a str,
    },
    /// An effect's parameter, in its unit: `music/effects/0/frequency_hz`.
    Param {
        /// The effect's bus.
        bus: &'a str,
        /// Its place in the chain.
        effect: usize,
        /// The parameter's name.
        param: &'a str,
    },
    /// An effect bypassed (1) or not (0): `music/effects/0/bypass`.
    Bypass {
        /// The effect's bus.
        bus: &'a str,
        /// Its place in the chain.
        effect: usize,
    },
}

/// What snapshot path `path` names, if it is one (ADR 0122).
pub fn snapshot_target(path: &str) -> Option<SnapshotTarget<'_>> {
    let mut parts = path.split('/');
    let bus = parts.next().filter(|b| !b.is_empty())?;
    let what = parts.next()?;
    let (third, fourth, rest) = (parts.next(), parts.next(), parts.next());
    if rest.is_some() {
        return None;
    }
    match (bus, what, third, fourth) {
        ("room", "gain", None, None) => Some(SnapshotTarget::Room),
        (_, "gain", None, None) => Some(SnapshotTarget::Gain(bus)),
        (_, "balance", None, None) => Some(SnapshotTarget::Balance(bus)),
        (_, "sends", Some(to), None) if !to.is_empty() => Some(SnapshotTarget::Send { bus, to }),
        (_, "effects", Some(k), Some("bypass")) => Some(SnapshotTarget::Bypass {
            bus,
            effect: k.parse().ok()?,
        }),
        (_, "effects", Some(k), Some(param)) if !param.is_empty() => Some(SnapshotTarget::Param {
            bus,
            effect: k.parse().ok()?,
            param,
        }),
        _ => None,
    }
}

/// An effect of a bus.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct EffectLayout {
    /// What it is, by its name in files: `compressor`, `limiter`, `delay`, `reverb`, or a
    /// filter's shape (`low_pass`, `peak`, ...; [`engine_audio::dsp::EFFECTS`]).
    pub kind: String,
    /// Left out of the mix: it costs nothing, and its tail starts from silence when it
    /// comes back.
    pub bypass: bool,
    /// The bus whose sound keys it (a compressor on music keyed by the voice ducks the
    /// music under dialogue); empty: its own sound.
    pub sidechain: String,
    /// Its parameters by their names (`threshold_db`), in their units; those not named
    /// keep their defaults.
    pub params: BTreeMap<String, f32>,
}

impl EffectLayout {
    /// An effect of `kind` with every parameter at its default, written out (an unknown
    /// kind has none).
    pub fn new(kind: &str) -> Self {
        let params = effect_info(kind)
            .map(|info| {
                info.params
                    .iter()
                    .map(|p| (p.name.to_owned(), p.default))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            kind: kind.to_owned(),
            params,
            ..Self::default()
        }
    }

    /// Parameter `name`'s value as it plays: the file's within range, or the default
    /// (`None`: the kind does not have it).
    pub fn param(&self, name: &str) -> Option<f32> {
        let info = effect_info(&self.kind)?.param(name)?;
        Some(
            self.params
                .get(name)
                .map_or(info.default, |&v| info.clamp(v)),
        )
    }
}

/// A send from a bus into another (an echo bus that several feed), keyed by the bus it
/// feeds.
#[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
pub struct SendLayout {
    /// Its level in dB: 0 sends the sound as it comes.
    pub gain: f32,
    /// Taken before the bus's fader, so the fader does not move it.
    pub pre_fader: bool,
}

/// The sends and sidechains the player makes (by bus names), and those it leaves out
/// because they would loop the sound back ([`MixerFile::routes`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Routes<'a> {
    /// The sends: from, to, and the send.
    pub sends: Vec<(&'a str, &'a str, &'a SendLayout)>,
    /// The sidechains: the keyed bus (`master` for the master), the effect's place in its
    /// chain, and the bus keying it.
    pub sidechains: Vec<(&'a str, usize, &'a str)>,
    /// What is left out, and why.
    pub refused: Vec<String>,
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
    /// Its effects, in the order the sound goes through them.
    pub effects: Vec<EffectLayout>,
    /// Its sends, by the bus they feed.
    pub sends: BTreeMap<String, SendLayout>,
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
    /// The room's reverb's fader (version 3).
    pub room: RoomLayout,
    /// The buses, by name.
    pub buses: BTreeMap<String, BusLayout>,
    /// The snapshots, in the order their layers stack (version 4, ADR 0122).
    pub snapshots: Vec<SnapshotLayout>,
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
    /// The format's version: 2 adds effects and sends, 3 the room's fader, 4 snapshots (an
    /// older file reads as one without them).
    pub const VERSION: u32 = 4;

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
            room: RoomLayout::default(),
            buses,
            snapshots: Vec::new(),
        }
    }

    /// The snapshot called `name`.
    pub fn snapshot(&self, name: &str) -> Option<&SnapshotLayout> {
        self.snapshots.iter().find(|s| s.name == name)
    }

    /// The mix's value of what `target` names (ADR 0122): an effect's parameter at its
    /// default when the file leaves it out; none for what the layout lacks.
    pub fn value_at(&self, target: SnapshotTarget<'_>) -> Option<f32> {
        match target {
            SnapshotTarget::Gain("master") => Some(self.master.gain),
            SnapshotTarget::Gain(bus) => self.buses.get(bus).map(|b| b.gain),
            SnapshotTarget::Balance("master") => Some(self.master.balance),
            SnapshotTarget::Balance(bus) => self.buses.get(bus).map(|b| b.balance),
            SnapshotTarget::Room => Some(self.room.gain),
            SnapshotTarget::Send { bus, to } => self.buses.get(bus)?.sends.get(to).map(|s| s.gain),
            SnapshotTarget::Param { bus, effect, param } => {
                self.effects(bus)?.get(effect)?.param(param)
            }
            SnapshotTarget::Bypass { bus, effect } => self
                .effects(bus)?
                .get(effect)
                .map(|e| if e.bypass { 1.0 } else { 0.0 }),
        }
    }

    /// What `target` names set to `value` (a parameter within its range, a bypass on from
    /// 0.5): whether the layout has it.
    pub fn set_value(&mut self, target: SnapshotTarget<'_>, value: f32) -> bool {
        match target {
            SnapshotTarget::Gain("master") => self.master.gain = value,
            SnapshotTarget::Balance("master") => self.master.balance = value.clamp(-1.0, 1.0),
            SnapshotTarget::Gain(bus) => match self.buses.get_mut(bus) {
                Some(b) => b.gain = value,
                None => return false,
            },
            SnapshotTarget::Balance(bus) => match self.buses.get_mut(bus) {
                Some(b) => b.balance = value.clamp(-1.0, 1.0),
                None => return false,
            },
            SnapshotTarget::Room => self.room.gain = value,
            SnapshotTarget::Send { bus, to } => {
                match self.buses.get_mut(bus).and_then(|b| b.sends.get_mut(to)) {
                    Some(s) => s.gain = value,
                    None => return false,
                }
            }
            SnapshotTarget::Param { bus, effect, param } => {
                let Some(e) = self.effect_mut(bus, effect) else {
                    return false;
                };
                let Some(info) = effect_info(&e.kind).and_then(|i| i.param(param)) else {
                    return false;
                };
                let value = info.clamp(value);
                match e.params.get_mut(param) {
                    Some(v) => *v = value,
                    None => {
                        e.params.insert(param.to_owned(), value);
                    }
                }
            }
            SnapshotTarget::Bypass { bus, effect } => match self.effect_mut(bus, effect) {
                Some(e) => e.bypass = value >= 0.5,
                None => return false,
            },
        }
        true
    }

    fn effect_mut(&mut self, bus: &str, k: usize) -> Option<&mut EffectLayout> {
        if bus == "master" {
            self.master.effects.get_mut(k)
        } else {
            self.buses.get_mut(bus)?.effects.get_mut(k)
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

    /// A bus's effects by its name (`master` for the master's).
    pub fn effects(&self, bus: &str) -> Option<&[EffectLayout]> {
        if bus == "master" {
            Some(&self.master.effects)
        } else {
            self.buses.get(bus).map(|b| b.effects.as_slice())
        }
    }

    /// The sends and sidechains the player makes, by bus names: those to buses the layout
    /// has (not the player's own), in the buses' order (sends by the bus they feed, then the
    /// master's sidechains and each bus's, effect by effect). One that would loop the sound
    /// back to where it comes from is left out, with why.
    pub fn routes(&self) -> Routes<'_> {
        let order = self.order();
        let names: Vec<&str> = order.iter().map(|b| b.name).collect();
        let known = |name: &str| names.contains(&name);
        // Bus `a` feeds bus `b` (directly or not): edges from feeds, then from routes.
        let mut edges: Vec<(&str, &str)> = order
            .iter()
            .map(|b| (b.name, b.parent.unwrap_or("master")))
            .collect();
        fn reaches(edges: &[(&str, &str)], from: &str, to: &str) -> bool {
            let mut stack = vec![from];
            let mut seen: Vec<&str> = Vec::new();
            while let Some(at) = stack.pop() {
                if at == to {
                    return true;
                }
                if !seen.contains(&at) {
                    seen.push(at);
                    stack.extend(edges.iter().filter(|(a, _)| *a == at).map(|(_, b)| *b));
                }
            }
            false
        }
        let mut routes = Routes::default();
        for placed in &order {
            for (to, send) in &placed.layout.sends {
                let to = to.as_str();
                if !known(to) {
                    routes.refused.push(format!(
                        "`{}` sends into `{to}`, which the layout does not have: left out",
                        placed.name
                    ));
                } else if to == placed.name || reaches(&edges, to, placed.name) {
                    routes.refused.push(format!(
                        "`{}`'s send into `{to}` would loop the sound back: left out",
                        placed.name
                    ));
                } else {
                    edges.push((placed.name, to));
                    routes.sends.push((placed.name, to, send));
                }
            }
        }
        let chains = std::iter::once(("master", self.master.effects.as_slice()))
            .chain(order.iter().map(|b| (b.name, b.layout.effects.as_slice())));
        for (bus, effects) in chains {
            for (k, effect) in effects.iter().enumerate() {
                let source = effect.sidechain.as_str();
                if source.is_empty() || source == bus {
                    continue;
                }
                if !known(source) {
                    routes.refused.push(format!(
                        "{}'s sidechain is `{source}`, which the layout does not have: left out",
                        effect_name(bus, k, effect)
                    ));
                } else if reaches(&edges, bus, source) {
                    routes.refused.push(format!(
                        "{}'s sidechain `{source}` would loop the sound back: left out",
                        effect_name(bus, k, effect)
                    ));
                } else {
                    edges.push((source, bus));
                    routes.sidechains.push((bus, k, source));
                }
            }
        }
        routes
    }

    /// Whether `name` is one the player keeps for a bus of its own (`master`, `reverb`,
    /// `editor preview`, names ending `.world`), or empty: no bus of a layout is called so.
    pub fn is_kept(name: &str) -> bool {
        kept(name.trim())
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
        let chains = std::iter::once(("master", self.master.effects.as_slice())).chain(
            self.buses
                .iter()
                .filter(|(name, _)| !kept(name))
                .map(|(name, b)| (name.as_str(), b.effects.as_slice())),
        );
        for (bus, effects) in chains {
            for (k, effect) in effects.iter().enumerate() {
                let Some(info) = effect_info(&effect.kind) else {
                    out.push(format!(
                        "{} is `{}`, which the engine does not have: left out",
                        effect_name(bus, k, effect),
                        effect.kind
                    ));
                    continue;
                };
                for (param, &value) in &effect.params {
                    match info.param(param) {
                        None => out.push(format!(
                            "{} has no `{param}`: left out",
                            effect_name(bus, k, effect)
                        )),
                        Some(p) if p.clamp(value) != value => out.push(format!(
                            "{}'s `{param}` is {value}, beyond {} to {}: it plays at {}",
                            effect_name(bus, k, effect),
                            p.min,
                            p.max,
                            p.clamp(value)
                        )),
                        Some(_) => {}
                    }
                }
            }
        }
        out.extend(self.routes().refused);
        // The snapshots (version 4).
        for name in self.buses.keys().filter(|n| n.contains('/')) {
            out.push(format!(
                "`{name}` has a `/` in its name: snapshots cannot reach it"
            ));
        }
        for (i, s) in self.snapshots.iter().enumerate() {
            if s.name.is_empty() {
                out.push(format!(
                    "snapshot {} has no name: no blend reaches it",
                    i + 1
                ));
            } else if self.snapshots[..i].iter().any(|o| o.name == s.name) {
                out.push(format!(
                    "two snapshots are called `{}`: the first is blended",
                    s.name
                ));
            }
            for (path, &value) in &s.values {
                let Some(target) = snapshot_target(path) else {
                    out.push(format!(
                        "snapshot `{}`: `{path}` is not a path a snapshot sets: left out",
                        s.name
                    ));
                    continue;
                };
                if self.value_at(target).is_none() {
                    out.push(format!(
                        "snapshot `{}`: `{path}` names what the layout does not have: left out",
                        s.name
                    ));
                    continue;
                }
                if let SnapshotTarget::Param { bus, effect, param } = target
                    && let Some(p) = self
                        .effects(bus)
                        .and_then(|c| c.get(effect))
                        .and_then(|e| effect_info(&e.kind))
                        .and_then(|i| i.param(param))
                    && p.clamp(value) != value
                {
                    out.push(format!(
                        "snapshot `{}`: `{path}` is {value}, beyond {} to {}: it plays at {}",
                        s.name,
                        p.min,
                        p.max,
                        p.clamp(value)
                    ));
                }
            }
        }
        out
    }
}

/// An effect as problems name it: "`music`'s effect 2 (compressor)".
fn effect_name(bus: &str, k: usize, effect: &EffectLayout) -> String {
    let kind = effect_info(&effect.kind).map_or(effect.kind.as_str(), |i| i.label);
    if bus == "master" {
        format!("the master's effect {} ({kind})", k + 1)
    } else {
        format!("`{bus}`'s effect {} ({kind})", k + 1)
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
    fn snapshots_set_values_by_path_and_say_what_they_cannot_reach() {
        let mut file = MixerFile::builtin();
        file.buses
            .get_mut("music")
            .unwrap()
            .effects
            .push(EffectLayout::new("low_pass"));
        file.buses
            .get_mut("effects")
            .unwrap()
            .sends
            .insert(String::from("music"), SendLayout::default());
        file.snapshots.push(
            SnapshotLayout::new("underwater")
                .with("music/gain", -14.0)
                .with("music/effects/0/frequency_hz", 600.0)
                .with("music/effects/0/bypass", 1.0)
                .with("effects/sends/music", -6.0)
                .with("master/balance", 0.25)
                .with("room/gain", -3.0),
        );
        // Written as the grammar writes maps, read back the same.
        let text = file.to_text(AssetId::new());
        assert!(text.contains("snapshots.0.name = \"underwater\""), "{text}");
        assert!(
            text.contains("snapshots.0.values[\"music/gain\"] = -14"),
            "{text}"
        );
        let (back, _) = MixerFile::parse(&text).unwrap();
        assert_eq!(back, file);
        assert!(file.problems().is_empty(), "{:?}", file.problems());
        // Each path names a value of the mix, and sets it.
        let paths = [
            ("music/gain", 0.0, -14.0),
            ("music/effects/0/frequency_hz", 2_000.0, 600.0),
            ("music/effects/0/bypass", 0.0, 1.0),
            ("effects/sends/music", 0.0, -6.0),
            ("master/balance", 0.0, 0.25),
            ("room/gain", 0.0, -3.0),
        ];
        let mut set = file.clone();
        for (path, before, after) in paths {
            let target = snapshot_target(path).unwrap();
            assert_eq!(file.value_at(target), Some(before), "{path}");
            assert!(set.set_value(target, after), "{path}");
            assert_eq!(set.value_at(target), Some(after), "{path}");
        }
        assert!(set.buses["music"].effects[0].bypass);
        // A bypass blended to under a half is off; a parameter stays in its range.
        let bypass = snapshot_target("music/effects/0/bypass").unwrap();
        set.set_value(bypass, 0.4);
        assert!(!set.buses["music"].effects[0].bypass);
        let frequency = snapshot_target("music/effects/0/frequency_hz").unwrap();
        set.set_value(frequency, 1e9);
        assert!(set.buses["music"].effects[0].params["frequency_hz"] <= 20_000.0);
        // Not paths, and paths to what the layout lacks.
        for bad in [
            "music",
            "music/volume",
            "music/effects/x/gain",
            "/gain",
            "a/b/c/d/e",
        ] {
            assert!(snapshot_target(bad).is_none(), "{bad}");
        }
        let mut odd = file.clone();
        odd.snapshots.push(
            SnapshotLayout::new("underwater")
                .with("drums/gain", -3.0)
                .with("music/volume", 1.0)
                .with("music/effects/0/frequency_hz", 1e9),
        );
        odd.snapshots.push(SnapshotLayout::new(""));
        let problems = odd.problems().join("\n");
        for said in [
            "two snapshots are called `underwater`",
            "`drums/gain` names what the layout does not have",
            "`music/volume` is not a path",
            "`music/effects/0/frequency_hz` is 1000000000",
            "snapshot 3 has no name",
        ] {
            assert!(problems.contains(said), "{said}: {problems}");
        }
        // `room` is the reverb's in paths: a bus cannot be called it.
        let mut kept = MixerFile::builtin();
        kept.buses.insert(String::from("room"), BusLayout::new());
        assert!(
            kept.problems()
                .iter()
                .any(|p| p.contains("`room` is a name the player keeps"))
        );
        // A version 3 file reads with none.
        let (old, _) = MixerFile::parse("[mixer version=3]\nroom.gain = -2\n").unwrap();
        assert!(old.snapshots.is_empty());
    }

    #[test]
    fn the_rooms_fader_is_kept_in_the_file() {
        let mut file = MixerFile::builtin();
        file.room.gain = -4.5;
        let id = AssetId::new();
        let text = file.to_text(id);
        assert!(text.contains("room.gain = -4.5"), "{text}");
        let (back, _) = MixerFile::parse(&text).unwrap();
        assert_eq!(back, file);
        // A version 2 file reads with the room as the player gives it.
        let (old, _) = MixerFile::parse("[mixer version=2]\nmaster.gain = -1\n").unwrap();
        assert_eq!(old.room.gain, 0.0);
        assert_eq!(old.master.gain, -1.0);
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
            text.starts_with(&format!("[mixer version=4 id={id}]")),
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
    fn effects_and_sends_read_back_and_a_version_1_file_has_none() {
        let mut file = MixerFile::builtin();
        let mut duck = EffectLayout::new("compressor");
        duck.sidechain = String::from("voice");
        duck.params.insert(String::from("threshold_db"), -24.0);
        let music = file.buses.get_mut("music").unwrap();
        music.effects.push(duck);
        music.effects.push(EffectLayout {
            bypass: true,
            ..EffectLayout::new("low_pass")
        });
        file.buses.insert(String::from("canyon"), BusLayout::new());
        file.buses.get_mut("effects").unwrap().sends.insert(
            String::from("canyon"),
            SendLayout {
                gain: -12.0,
                pre_fader: false,
            },
        );
        file.master.effects.push(EffectLayout::new("limiter"));
        let text = file.to_text(AssetId::new());
        assert!(
            text.contains("buses[\"music\"].effects.0.kind = \"compressor\""),
            "{text}"
        );
        assert!(
            text.contains("buses[\"music\"].effects.0.params[\"threshold_db\"] = -24"),
            "{text}"
        );
        assert!(
            text.contains("buses[\"effects\"].sends[\"canyon\"].gain = -12"),
            "{text}"
        );
        assert!(
            text.contains("master.effects.0.kind = \"limiter\""),
            "{text}"
        );
        let (back, report) = MixerFile::parse(&text).unwrap();
        assert_eq!(back, file);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(file.problems().is_empty(), "{:?}", file.problems());
        let routes = file.routes();
        assert_eq!(routes.sends.len(), 1);
        assert_eq!(routes.sidechains, [("music", 0, "voice")]);
        // A version 1 file reads as one without effects and sends.
        let (old, _) = MixerFile::parse("[mixer version=1]\nbuses[\"sfx\"].gain = -3\n").unwrap();
        assert!(old.buses["sfx"].effects.is_empty() && old.buses["sfx"].sends.is_empty());
        // A parameter not named plays at its default; one beyond its range at its end.
        let comp = &back.buses["music"].effects[0];
        assert_eq!(comp.param("threshold_db"), Some(-24.0));
        assert_eq!(comp.param("nothing"), None);
        let mut loud = EffectLayout::new("compressor");
        loud.params.remove("ratio");
        loud.params.insert(String::from("knee_db"), 99.0);
        assert_eq!(loud.param("ratio"), Some(4.0));
        assert_eq!(loud.param("knee_db"), Some(24.0));
    }

    #[test]
    fn routes_that_would_loop_and_unknown_effects_are_left_out_and_said() {
        let mut file = MixerFile::builtin();
        let send = SendLayout::default();
        // music feeds ui, and ui sends back into music: the second loops.
        file.buses
            .get_mut("music")
            .unwrap()
            .sends
            .insert(String::from("ui"), send);
        file.buses
            .get_mut("ui")
            .unwrap()
            .sends
            .insert(String::from("music"), send);
        // A send to nowhere and to a bus of the player's own.
        let voice = file.buses.get_mut("voice").unwrap();
        voice.sends.insert(String::from("nowhere"), send);
        voice.sends.insert(String::from("reverb"), send);
        // A sidechain keyed by the bus it feeds: music feeds ui, so ui keyed by... music is
        // fine; music keyed by ui loops (ui gets music's sound first).
        let mut keyed = EffectLayout::new("compressor");
        keyed.sidechain = String::from("ui");
        file.buses.get_mut("music").unwrap().effects.push(keyed);
        let mut fine = EffectLayout::new("compressor");
        fine.sidechain = String::from("voice");
        file.buses.get_mut("effects").unwrap().effects.push(fine);
        // An effect the engine lacks, a parameter its kind lacks, a value out of range.
        let mut odd = EffectLayout::new("chorus");
        odd.params.insert(String::from("depth"), 1.0);
        file.master.effects.push(odd);
        let mut typo = EffectLayout::new("delay");
        typo.params.insert(String::from("feedbak"), 0.5);
        typo.params.insert(String::from("mix"), 2.0);
        file.buses.get_mut("effects").unwrap().effects.push(typo);
        let routes = file.routes();
        assert_eq!(
            routes
                .sends
                .iter()
                .map(|(a, b, _)| (*a, *b))
                .collect::<Vec<_>>(),
            [("music", "ui")]
        );
        assert_eq!(routes.sidechains, [("effects", 0, "voice")]);
        let problems = file.problems().join("\n");
        for said in [
            "`ui`'s send into `music` would loop the sound back",
            "`voice` sends into `nowhere`",
            "`voice` sends into `reverb`",
            "`music`'s effect 1 (Compressor)'s sidechain `ui` would loop",
            "the master's effect 1 (chorus) is `chorus`, which the engine does not have",
            "`effects`'s effect 2 (Delay) has no `feedbak`",
            "`effects`'s effect 2 (Delay)'s `mix` is 2, beyond 0 to 1: it plays at 1",
        ] {
            assert!(problems.contains(said), "{said}\n{problems}");
        }
    }

    #[test]
    fn a_file_names_its_buses_and_no_others() {
        // The built-in four are what plays without a file, not defaults a file adds to.
        let (file, _) = MixerFile::parse("[mixer version=1]\nbuses[\"sfx\"].gain = -3\n").unwrap();
        assert_eq!(
            file.room,
            RoomLayout::default(),
            "no room's fader before version 3"
        );
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
