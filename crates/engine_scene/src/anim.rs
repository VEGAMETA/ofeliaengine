//! A clip made in the editor (`.anim`, ADR 0124's E12-4a): keyed tracks of component
//! fields, its events, whether it loops.
//!
//! The file is one value in the scenes' grammar. A track is keyed by what it moves, so a
//! clip has one track a field and its lines sort by it:
//!
//! ```text
//! [anim version=1 id=…]
//!
//! length = 1
//! once = true
//! tracks["Transform.rotation"].keys.0.time = 0
//! tracks["Transform.rotation"].keys.0.value = [0, 0, 0]
//! tracks["Transform.rotation"].keys.1.time = 1
//! tracks["Transform.rotation"].keys.1.value = [0, 90, 0]
//! tracks["Lamp:PointLight.range"].keys.0.curve = Step
//! ```
//!
//! A target is `Component.field` as `Tween`'s are, behind the names of the entities below
//! the animated one when it is not that one (`Lamp:`, `Arm/Lamp:`). A key holds as many
//! numbers as the field (1 to 4); a rotation (`Quat`) is keyed as the inspector shows it,
//! three Euler angles in degrees, so a door turns 90 degrees as `[0, 90, 0]` and a fan
//! spins past 180 between two keys.

use std::collections::BTreeMap;
use std::path::Path;

use engine_reflect::Reflect;

use crate::{AssetId, ClipEvent, LoadError, ValueReport};

/// How a key goes on to the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum KeyCurve {
    /// Eases out of it and into the next: a cubic through the keys, flat where a key is
    /// higher or lower than both its neighbours, so it never overshoots them.
    #[default]
    Smooth = 0,
    /// A straight line to the next key.
    Linear = 1,
    /// Holds until the next key.
    Step = 2,
}

/// One key of a track.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct AnimKey {
    /// Seconds into the clip.
    pub time: f32,
    /// The field's value: as many numbers as the field holds (a rotation's three Euler
    /// angles in degrees).
    pub value: Vec<f32>,
    /// How it goes on to the next key.
    pub curve: KeyCurve,
}

/// The keys of one field.
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct AnimTrack {
    /// Its keys, in time order.
    pub keys: Vec<AnimKey>,
}

/// A clip made in the editor: an `.anim` file.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct AnimFile {
    /// Seconds long.
    pub length: f32,
    /// Frames a second: the grid the editor moves keys on.
    pub rate: f32,
    /// It plays once and holds its end (a door opening); false: it loops (a lamp's
    /// flicker).
    pub once: bool,
    /// Its tracks, by what each moves.
    pub tracks: BTreeMap<String, AnimTrack>,
    /// Its events, in time order (as an imported clip's in its model's `.meta`).
    pub events: Vec<ClipEvent>,
}

impl Default for AnimFile {
    fn default() -> Self {
        Self {
            length: 1.0,
            rate: 30.0,
            once: false,
            tracks: BTreeMap::new(),
            events: Vec::new(),
        }
    }
}

impl AnimFile {
    /// The kind in the file's header.
    pub const KIND: &'static str = "anim";
    /// The format's version.
    pub const VERSION: u32 = 1;
    /// The files' extension.
    pub const EXTENSION: &'static str = "anim";

    /// The file's text with its id; lines at their defaults left out (a reader takes them
    /// so).
    pub fn to_text(&self, id: AssetId) -> String {
        let mut all = String::new();
        crate::write_value(Self::KIND, Self::VERSION, Some(id), self, None, &mut all);
        let defaults = ["once = false", "rate = 30", "events = []", "tracks = {}"];
        let mut out = String::with_capacity(all.len());
        for line in all.lines() {
            let default = defaults.contains(&line)
                || (line.starts_with("tracks[") && line.ends_with(".curve = Smooth"))
                || (line.starts_with("events.")
                    && (line.ends_with(".data = \"\"") || line.ends_with(".sound = \"\"")));
            if !default {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// A clip from its text, and what reading it reported (its id, lines it skipped).
    ///
    /// # Errors
    ///
    /// Not a clip's file, a newer one, or a bad line (with its number).
    pub fn parse(text: &str) -> Result<(Self, ValueReport), LoadError> {
        let mut file = Self::default();
        let report = crate::read_value(Self::KIND, Self::VERSION, text, &mut file)?;
        Ok((file, report))
    }

    /// The clip in file `path`.
    ///
    /// # Errors
    ///
    /// The file cannot be read or is not a clip's (with its path).
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text)
            .map(|(file, _)| file)
            .map_err(|e| format!("{}:{e}", path.display()))
    }

    /// The key `track`'s keys hold at `time` exactly, if any (the editor keys there again
    /// instead of adding one).
    pub fn key_at(&self, track: &str, time: f32) -> Option<usize> {
        self.tracks
            .get(track)?
            .keys
            .iter()
            .position(|k| (k.time - time).abs() < 1e-4)
    }

    /// Key `value` set at `time` in track `track` (made if missing): the key there changed,
    /// or a new one in time order (its curve the one before it's, else smooth). The clip
    /// grows to hold it.
    pub fn set_key(&mut self, track: &str, time: f32, value: &[f32]) {
        let time = time.max(0.0);
        let keys = &mut self.tracks.entry(track.to_owned()).or_default().keys;
        match keys.iter().position(|k| (k.time - time).abs() < 1e-4) {
            Some(i) => {
                keys[i].value.clear();
                keys[i].value.extend_from_slice(value);
            }
            None => {
                let at = keys.partition_point(|k| k.time < time);
                let curve = at
                    .checked_sub(1)
                    .map_or(KeyCurve::Smooth, |i| keys[i].curve);
                keys.insert(
                    at,
                    AnimKey {
                        time,
                        value: value.to_vec(),
                        curve,
                    },
                );
            }
        }
        self.length = self.length.max(time);
    }
}

/// The target of a field of the animated entity itself, or of one below it named by the
/// path of names `below` (`""`: itself; `"Lamp"`, `"Arm/Lamp"`): `Lamp:PointLight.range`.
pub fn anim_target(below: &str, field: &str) -> String {
    if below.is_empty() {
        field.to_owned()
    } else {
        format!("{below}:{field}")
    }
}

/// A target's entity path below the animated entity (`""` for itself) and its
/// `Component.field`.
pub fn split_anim_target(target: &str) -> (&str, &str) {
    match target.rsplit_once(':') {
        Some((below, field)) => (below, field),
        None => ("", target),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;

    fn door() -> AnimFile {
        let mut anim = AnimFile {
            once: true,
            ..AnimFile::default()
        };
        anim.set_key("Transform.rotation", 0.0, &[0.0, 0.0, 0.0]);
        anim.set_key("Transform.rotation", 1.0, &[0.0, 90.0, 0.0]);
        anim.set_key("Lamp:PointLight.range", 0.0, &[1.0]);
        anim.tracks.get_mut("Lamp:PointLight.range").unwrap().keys[0].curve = KeyCurve::Step;
        anim.set_key("Lamp:PointLight.range", 0.5, &[0.25]);
        anim.events.push(ClipEvent {
            time: 0.9,
            name: String::from("thud"),
            ..ClipEvent::default()
        });
        anim
    }

    #[test]
    fn a_clip_reads_back_as_written_with_its_defaults_left_out() {
        let anim = door();
        let id = AssetId::from_u128(7);
        let text = anim.to_text(id);
        assert!(text.starts_with("[anim version=1 id="), "{text}");
        assert!(
            text.contains("tracks[\"Transform.rotation\"].keys.1.value = [0, 90, 0]"),
            "{text}"
        );
        assert!(text.contains("once = true"), "{text}");
        assert!(
            text.contains("tracks[\"Lamp:PointLight.range\"].keys.0.curve = Step"),
            "{text}"
        );
        // The second lamp key took the first's curve; smooth keys and empty fields say
        // nothing.
        assert!(!text.contains("Smooth"), "{text}");
        assert!(!text.contains("rate ="), "{text}");
        assert!(!text.contains(".data ="), "{text}");
        let (back, report) = AnimFile::parse(&text).unwrap();
        assert_eq!(report.id, Some(id));
        assert_eq!(back, anim);
    }

    #[test]
    fn keys_go_in_time_order_and_a_key_at_a_keyed_time_changes_it() {
        let mut anim = AnimFile::default();
        anim.set_key("Transform.translation.y", 0.5, &[1.0]);
        anim.set_key("Transform.translation.y", 0.0, &[0.0]);
        anim.set_key("Transform.translation.y", 2.0, &[3.0]);
        anim.set_key("Transform.translation.y", 0.5, &[2.0]);
        let keys = &anim.tracks["Transform.translation.y"].keys;
        let times: Vec<f32> = keys.iter().map(|k| k.time).collect();
        assert_eq!(times, [0.0, 0.5, 2.0]);
        assert_eq!(keys[1].value, [2.0]);
        assert_eq!(anim.length, 2.0, "the clip grew to hold its last key");
        assert_eq!(anim.key_at("Transform.translation.y", 0.5), Some(1));
        assert_eq!(anim.key_at("Transform.translation.y", 0.25), None);
    }

    #[test]
    fn targets_split_into_the_entity_below_and_the_field() {
        assert_eq!(
            split_anim_target("Transform.rotation"),
            ("", "Transform.rotation")
        );
        assert_eq!(
            split_anim_target("Arm/Lamp:PointLight.range"),
            ("Arm/Lamp", "PointLight.range")
        );
        assert_eq!(anim_target("", "Transform.scale"), "Transform.scale");
        assert_eq!(
            anim_target("Lamp", "PointLight.color"),
            "Lamp:PointLight.color"
        );
    }
}
