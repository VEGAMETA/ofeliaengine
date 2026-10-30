//! A model file's clip events as the Animation tab edits them (ADR 0124's E12-3b-2): its
//! `.meta` held, each edit written back at once and given to the player, undo handing the
//! `.meta`'s text back ([`crate::undo::History::change_file`]).

use std::path::Path;

use engine_core::math::Vec3;
use engine_scene::{AssetId, AssetMeta, ClipEvent, ClipImport};

/// A model file's `.meta`, held while its clips show (see the module).
#[derive(Debug)]
pub(crate) struct EventsDoc {
    /// The model file, and its path under the project.
    pub(crate) file: AssetId,
    pub(crate) path: String,
    pub(crate) meta: AssetMeta,
    /// The `.meta`'s text as last read or written: what an edit's undo puts back.
    text: String,
    /// The player has its events as held ([`engine_app::Assets::set_clip_events`]).
    pub(crate) given: bool,
}

impl EventsDoc {
    /// Model file `file` at `path` under `root`, its `.meta` read (the defaults without one).
    pub(crate) fn open(root: &Path, file: AssetId, path: &str) -> Self {
        let meta = AssetMeta::read(&root.join(path));
        let text = meta.to_text(file);
        Self {
            file,
            path: path.to_owned(),
            meta,
            text,
            given: true,
        }
    }

    /// Clip `clip`'s events (by its name in the file, an unnamed one's as the editor shows
    /// it), in time order.
    pub(crate) fn events(&self, clip: &str) -> &[ClipEvent] {
        self.meta
            .model
            .clips
            .iter()
            .find(|c| c.name == clip)
            .map_or(&[], |c| &c.events)
    }

    /// Clip `clip`'s events changed by `edit` in memory only (a drag, until its release):
    /// given to the player, not written.
    pub(crate) fn edit_live(&mut self, clip: &str, edit: impl FnOnce(&mut Vec<ClipEvent>)) {
        let k = match self.meta.model.clips.iter().position(|c| c.name == clip) {
            Some(k) => k,
            None => {
                self.meta.model.clips.push(ClipImport {
                    name: clip.to_owned(),
                    events: Vec::new(),
                });
                self.meta.model.clips.len() - 1
            }
        };
        edit(&mut self.meta.model.clips[k].events);
        // A clip left without events leaves the file.
        if self.meta.model.clips[k].events.is_empty() {
            self.meta.model.clips.remove(k);
        }
        self.given = false;
    }

    /// What the held events are now, written to the `.meta` under `root`: the text before
    /// and after, or None when nothing changed.
    ///
    /// # Errors
    ///
    /// The `.meta` could not be written: the held events go back to what it holds.
    pub(crate) fn write(&mut self, root: &Path) -> Result<Option<(String, String)>, String> {
        let after = self.meta.to_text(self.file);
        if after == self.text {
            return Ok(None);
        }
        if let Err(e) = self.meta.write(&root.join(&self.path), self.file) {
            let text = core::mem::take(&mut self.text);
            self.put_text(root, &text).ok();
            return Err(e);
        }
        let before = core::mem::replace(&mut self.text, after.clone());
        Ok(Some((before, after)))
    }

    /// Clip `clip`'s events changed by `edit` and written ([`Self::write`]).
    ///
    /// # Errors
    ///
    /// As [`Self::write`].
    pub(crate) fn edit(
        &mut self,
        root: &Path,
        clip: &str,
        edit: impl FnOnce(&mut Vec<ClipEvent>),
    ) -> Result<Option<(String, String)>, String> {
        self.edit_live(clip, edit);
        self.write(root)
    }

    /// The `.meta`'s text undo or redo put back: held, written and given to the player.
    ///
    /// # Errors
    ///
    /// Not a `.meta`, or it could not be written.
    pub(crate) fn put_text(&mut self, root: &Path, text: &str) -> Result<(), String> {
        let (meta, _) = AssetMeta::parse(text).map_err(|e| format!("{}.meta:{e}", self.path))?;
        std::fs::write(AssetMeta::path(&root.join(&self.path)), text)
            .map_err(|e| format!("{}.meta: {e}", self.path))?;
        self.meta = meta;
        text.clone_into(&mut self.text);
        self.given = false;
        Ok(())
    }
}

/// Event `k` moved to `time` in `events` (kept in time order): where it is now.
pub(crate) fn move_event(events: &mut Vec<ClipEvent>, k: usize, time: f32) -> usize {
    if k >= events.len() {
        return k;
    }
    let mut event = events.remove(k);
    event.time = time;
    let at = events.partition_point(|e| e.time <= time);
    events.insert(at, event);
    at
}

/// `event` put into `events` (kept in time order, after those at the same time): where.
pub(crate) fn add_event(events: &mut Vec<ClipEvent>, event: ClipEvent) -> usize {
    let at = events.partition_point(|e| e.time <= event.time);
    events.insert(at, event);
    at
}

/// Calls `each` for the events (in time order) of a clip `length` seconds long that play
/// from `from` to `to` passed, as the editor's previews play it (E12-3b-3): forward,
/// `from <= t < to`; `to` may run past the clip's end into its loops (64 at most).
pub(crate) fn passed<'a>(
    events: &'a [ClipEvent],
    length: f32,
    from: f32,
    to: f32,
    mut each: impl FnMut(&'a ClipEvent),
) {
    if events.is_empty() || length <= 0.0 || to <= from || !from.is_finite() || !to.is_finite() {
        return;
    }
    let first = (from / length).floor();
    let loops = (((to / length).floor() - first) as i64).min(64);
    for m in 0..=loops {
        let start = (first + m as f32) * length;
        for e in events.iter().filter(|e| e.time <= length) {
            let t = start + e.time;
            if t >= from && t < to {
                each(e);
            }
        }
    }
}

/// Which side a joint's name says it is on (ADR 0124's E12-3d): `left` or `right` in it, or
/// an `L` or `R` part of it (`leg_joint_L_5`, `foot.R`).
fn side_of(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("left") {
        return Some("left");
    }
    if lower.contains("right") {
        return Some("right");
    }
    let parts = || lower.split(|c: char| !c.is_ascii_alphanumeric());
    if parts().any(|p| p == "l") {
        Some("left")
    } else if parts().any(|p| p == "r") {
        Some("right")
    } else {
        None
    }
}

/// A skeleton's feet with their sides (ADR 0124's E12-3d), from its joints' `names` and
/// their places in the rest pose (`rest`, `up` the model's up): the first joint named for a
/// foot on each side (`foot`, else `ankle`); else the rest pose's lowest joint of each side
/// the names give; else the lowest joint and the lowest one well apart from it (the first
/// left).
pub(crate) fn find_feet(names: &[String], rest: &[Vec3], up: Vec3) -> Vec<(usize, &'static str)> {
    for word in ["foot", "ankle"] {
        let mut feet: Vec<(usize, &'static str)> = Vec::new();
        for (j, name) in names.iter().enumerate() {
            if !name.to_ascii_lowercase().contains(word) {
                continue;
            }
            if let Some(side) = side_of(name)
                && !feet.iter().any(|&(_, s)| s == side)
            {
                feet.push((j, side));
            }
        }
        if feet.len() == 2 {
            return feet;
        }
    }
    let height = |p: Vec3| p.dot(up);
    let flat = |p: Vec3| p - up * p.dot(up);
    let lowest = |of: &mut dyn Iterator<Item = usize>| {
        of.min_by(|&a, &b| height(rest[a]).total_cmp(&height(rest[b])))
    };
    // Sides from the names: each side's lowest joint.
    let named = |side: &str| {
        let mut of =
            (0..rest.len()).filter(|&j| names.get(j).and_then(|n| side_of(n)) == Some(side));
        lowest(&mut of)
    };
    if let (Some(left), Some(right)) = (named("left"), named("right")) {
        return vec![(left, "left"), (right, "right")];
    }
    let Some(low) = lowest(&mut (0..rest.len())) else {
        return Vec::new();
    };
    let (lo, hi) = rest.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &p| {
        (lo.min(height(p)), hi.max(height(p)))
    });
    let apart = 0.1 * (hi - lo).max(1e-3);
    let other = (0..rest.len())
        .filter(|&j| (flat(rest[j]) - flat(rest[low])).length() >= apart)
        .min_by(|&a, &b| height(rest[a]).total_cmp(&height(rest[b])));
    let Some(other) = other else {
        return vec![(low, "left")];
    };
    let side =
        |j: usize, other: &'static str| names.get(j).and_then(|n| side_of(n)).unwrap_or(other);
    let first = side(low, "left");
    let second = side(other, if first == "left" { "right" } else { "left" });
    vec![(low, first), (other, second)]
}

/// The moments a foot touches down in a clip (ADR 0124's E12-3d), from its path (a place a
/// sample at `rate` samples a second, `up` the model's up): where it comes down into the
/// lowest fifth of its height's range. Its speed is no guide: in a clip made in place a
/// planted foot slides back nearly as fast as it swings. A clip whose ends meet (a loop)
/// leaves its last sample, its first again, out, and does not mark its start when the foot
/// was already down before its end.
pub(crate) fn foot_plants(path: &[Vec3], up: Vec3, rate: f32) -> Vec<f32> {
    let n = path.len();
    if n < 3 || rate <= 0.0 {
        return Vec::new();
    }
    let heights: Vec<f32> = path.iter().map(|p| p.dot(up)).collect();
    let (lo, hi) = heights
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), &h| (lo.min(h), hi.max(h)));
    if hi - lo < 1e-4 {
        return Vec::new();
    }
    let down = |s: usize| heights[s] <= lo + 0.2 * (hi - lo);
    let looped = (path[0] - path[n - 1]).length() <= 0.05 * (hi - lo);
    let last = if looped { n - 1 } else { n };
    (0..last)
        .filter(|&s| {
            let before = match s {
                0 => looped && down(n - 2),
                _ => down(s - 1),
            };
            down(s) && !before
        })
        .map(|s| s as f32 / rate)
        .collect()
}

/// `time` on the clip's nearest frame at `rate` samples a second, within its `length`.
pub(crate) fn on_frame(time: f32, rate: f32, length: f32) -> f32 {
    let time = time.clamp(0.0, length.max(0.0));
    if rate <= 0.0 {
        return time;
    }
    ((time * rate).round() / rate).min(length.max(0.0))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn event(time: f32, name: &str) -> ClipEvent {
        ClipEvent {
            time,
            name: name.to_owned(),
            ..ClipEvent::default()
        }
    }

    /// E12-3b-2: a model's events held from its `.meta`, edited and written at once, the
    /// text before and after for undo; a clip left without events leaves the file; undo's
    /// text put back is read, written and given to the player.
    #[test]
    fn a_models_events_are_edited_written_and_put_back() {
        let dir = std::env::temp_dir().join(format!("ofelia-events-doc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("models")).unwrap();
        let file = dir.join("models/walker.gltf");
        std::fs::write(&file, "{}").unwrap();
        let id = AssetMeta::ensure(&file).unwrap();
        let meta_text = || std::fs::read_to_string(AssetMeta::path(&file)).unwrap();
        let mut doc = EventsDoc::open(&dir, id, "models/walker.gltf");
        assert!(doc.events("Walk").is_empty());
        assert!(doc.given, "nothing to give yet");
        // Added: written at once, the texts before and after.
        let (before, after) = doc
            .edit(&dir, "Walk", |events| {
                add_event(events, event(0.5, "step"));
                add_event(events, event(0.25, "start"));
            })
            .unwrap()
            .unwrap();
        assert_eq!(after, meta_text());
        assert!(!before.contains("events"), "{before}");
        let names: Vec<&str> = doc.events("Walk").iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["start", "step"], "in time order");
        assert!(!doc.given, "the player to be told");
        // Moved past the other: its place follows.
        let at = {
            let mut at = 0;
            doc.edit(&dir, "Walk", |events| at = move_event(events, 0, 0.75))
                .unwrap();
            at
        };
        assert_eq!(at, 1);
        assert_eq!(doc.events("Walk")[1].name, "start");
        // Nothing changed: no step.
        assert!(doc.edit(&dir, "Walk", |_| {}).unwrap().is_none());
        // Both removed: the clip leaves the file.
        doc.edit(&dir, "Walk", Vec::clear).unwrap();
        assert!(doc.meta.model.clips.is_empty());
        assert!(!meta_text().contains("model.clips"), "{}", meta_text());
        // Undo's text put back: held and written.
        doc.put_text(&dir, &after).unwrap();
        assert_eq!(doc.events("Walk").len(), 2);
        assert_eq!(meta_text(), after);
        // A live edit (a drag) is held and given, then written once.
        doc.given = true;
        doc.edit_live("Walk", |events| {
            move_event(events, 1, 0.1);
        });
        assert!(!doc.given);
        assert_eq!(meta_text(), after, "not written while dragged");
        let (from, to) = doc.write(&dir).unwrap().unwrap();
        assert_eq!(
            (from.as_str(), to.as_str()),
            (after.as_str(), meta_text().as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// E12-3b-3: the events a preview's frame passed, across the loop too; none standing
    /// still.
    #[test]
    fn a_previews_frame_passes_the_events_it_played() {
        let events = [event(0.0, "a"), event(0.25, "b"), event(0.75, "c")];
        let names = |from: f32, to: f32| {
            let mut out = Vec::new();
            passed(&events, 1.0, from, to, |e| out.push(e.name.clone()));
            out
        };
        assert_eq!(names(0.0, 0.3), ["a", "b"]);
        assert_eq!(names(0.3, 0.7), Vec::<String>::new());
        assert_eq!(names(0.7, 1.1), ["c", "a"], "across the loop");
        assert_eq!(names(0.5, 0.5), Vec::<String>::new(), "standing still");
        assert_eq!(names(0.9, 0.2), Vec::<String>::new(), "never backwards");
    }

    /// A foot walking in place: down and sliding back for `down` samples, then up and
    /// swinging forward, `cycles` times; its first and last places the same.
    fn walking_foot(cycles: usize, down: usize, up: usize, shift: usize) -> Vec<Vec3> {
        let period = down + up;
        let mut path: Vec<Vec3> = (0..cycles * period)
            .map(|s| {
                let k = (s + shift) % period;
                if k < down {
                    Vec3::new(0.0, 0.0, 0.2 - 0.4 * k as f32 / down as f32)
                } else {
                    let f = (k - down) as f32 / up as f32;
                    let lift = (f * core::f32::consts::PI).sin() * 0.15;
                    Vec3::new(0.0, lift, -0.2 + 0.4 * f)
                }
            })
            .collect();
        path.push(path[0]);
        path
    }

    /// E12-3d: a foot's touch-downs: one a cycle where it comes down, none for a foot that
    /// never lifts; a loop down across its end marks the touch-down inside, not its start.
    #[test]
    fn a_foots_touch_downs_are_where_it_comes_down() {
        let up = Vec3::Y;
        let path = walking_foot(2, 10, 10, 0);
        let plants = foot_plants(&path, up, 20.0);
        assert_eq!(plants, [0.0, 1.0], "a touch-down a cycle");
        // Down across the loop's end: its touch-down is inside, not at the start.
        let shifted = walking_foot(2, 10, 10, 5);
        let plants = foot_plants(&shifted, up, 20.0);
        assert_eq!(plants, [0.75, 1.75], "{plants:?}");
        // A foot that never lifts marks nothing.
        let still = vec![Vec3::new(0.0, 0.0, 0.1); 20];
        assert!(foot_plants(&still, up, 20.0).is_empty());
    }

    /// E12-3d: feet found by their names and sides (the Fox's), else the rest pose's two
    /// lowest joints apart (CesiumMan's), sided by their names.
    #[test]
    fn feet_are_found_by_name_or_as_the_lowest_joints_apart() {
        let names: Vec<String> = [
            "b_Hip_01",
            "b_LeftLeg01_015",
            "b_LeftFoot01_017",
            "b_LeftFoot02_018",
            "b_RightFoot01_021",
        ]
        .map(String::from)
        .to_vec();
        let rest = vec![Vec3::ZERO; names.len()];
        assert_eq!(
            find_feet(&names, &rest, Vec3::Y),
            [(2, "left"), (4, "right")]
        );
        let names: Vec<String> = ["torso", "leg_joint_L_5", "leg_joint_R_5", "leg_joint_R_3"]
            .map(String::from)
            .to_vec();
        let rest = vec![
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(0.1, 0.02, 0.0),
            Vec3::new(-0.1, 0.0, 0.0),
            Vec3::new(-0.1, 0.4, 0.0),
        ];
        assert_eq!(
            find_feet(&names, &rest, Vec3::Y),
            [(1, "left"), (2, "right")]
        );
        // Each side's lowest by name, though a joint of the same leg lies well apart.
        let names: Vec<String> = ["leg_joint_L_3", "leg_joint_L_5", "leg_joint_R_5"]
            .map(String::from)
            .to_vec();
        let rest = vec![
            Vec3::new(0.1, 0.05, 0.4),
            Vec3::new(0.1, 0.0, 0.0),
            Vec3::new(-0.1, 0.01, 0.0),
        ];
        assert_eq!(
            find_feet(&names, &rest, Vec3::Y),
            [(1, "left"), (2, "right")]
        );
        // No sides in the names: the lowest and the lowest well apart (a tenth of the
        // skeleton's height).
        let names: Vec<String> = ["a", "b", "c", "head"].map(String::from).to_vec();
        let rest = vec![
            Vec3::new(0.1, 0.0, 0.0),
            Vec3::new(0.1, 0.01, 0.01),
            Vec3::new(-0.1, 0.02, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
        ];
        assert_eq!(
            find_feet(&names, &rest, Vec3::Y),
            [(0, "left"), (2, "right")]
        );
        assert_eq!(side_of("foot.L"), Some("left"));
        assert_eq!(side_of("mixamorig:RightFoot"), Some("right"));
        assert_eq!(side_of("tail"), None);
    }

    /// E12-3b-2: events kept in time order as they are added and moved; times on frames.
    #[test]
    fn events_keep_their_order_and_land_on_frames() {
        let mut events = vec![event(0.1, "a"), event(0.5, "b")];
        assert_eq!(
            add_event(&mut events, event(0.5, "c")),
            2,
            "after its equal"
        );
        assert_eq!(add_event(&mut events, event(0.0, "d")), 0);
        assert_eq!(move_event(&mut events, 0, 0.3), 1);
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a", "d", "b", "c"]);
        assert_eq!(move_event(&mut events, 9, 0.3), 9, "no such event");
        assert_eq!(on_frame(0.51, 30.0, 1.0), 0.5);
        assert_eq!(on_frame(1.2, 30.0, 1.0), 1.0, "within the clip");
        assert_eq!(on_frame(-0.2, 30.0, 1.0), 0.0);
        assert_eq!(on_frame(0.123, 0.0, 1.0), 0.123, "no rate: as it is");
    }
}
