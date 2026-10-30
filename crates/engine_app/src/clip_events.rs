//! The clips' events (ADR 0124's E12-3b): the moments a model's `.meta` names in its clips
//! ([`engine_scene::ClipEvent`]), found by the clip's name, and the ones a tick's play
//! passed ([`crossed`]). The player fires them into [`engine_runtime::AnimationEvents`].

use std::sync::Arc;

use engine_scene::{ClipEvent, ClipImport};

/// One event as the player fires it: its moment, and texts shared by every firing so a
/// firing allocates nothing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventDef {
    /// Seconds into the clip.
    pub(crate) time: f32,
    pub(crate) name: Arc<str>,
    pub(crate) data: Arc<str>,
    /// Played at the character, by its path in the project.
    pub(crate) sound: Option<Arc<str>>,
}

/// How a clip plays (ADR 0124's E12-3e, its `.meta`'s): once, held at its end, or looped;
/// and what of its root's motion root motion takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ClipPlay {
    pub(crate) once: bool,
    pub(crate) axes: crate::skins::Axes,
}

/// A model file's clips as its `.meta` has them: per clip it names, its name in the file,
/// its events in time order and how it plays.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ModelEvents {
    clips: Vec<(Box<str>, Vec<EventDef>, ClipPlay)>,
}

impl ModelEvents {
    /// What `clips` (a `.meta`'s) name, their events sorted by time; moments that are not
    /// numbers left out, those before the clip's start at its start.
    pub(crate) fn new(clips: &[ClipImport]) -> Self {
        let clips = clips
            .iter()
            .filter(|c| !c.is_plain())
            .map(|c| {
                let events = event_defs(&c.events);
                let play = ClipPlay {
                    once: c.once,
                    axes: crate::skins::Axes {
                        across: c.root_across,
                        up: c.root_up,
                        turn: c.root_turn,
                    },
                };
                (Box::from(c.name.as_str()), events, play)
            })
            .collect();
        Self { clips }
    }

    /// No clip has an event.
    pub(crate) fn is_empty(&self) -> bool {
        self.clips.iter().all(|(_, events, _)| events.is_empty())
    }

    /// Clip `k` of a rig, `name` in its file: an unnamed one is found by the name the editor
    /// shows it by (`Clip 2` for the second).
    fn find(&self, name: &str, k: usize) -> Option<&(Box<str>, Vec<EventDef>, ClipPlay)> {
        self.clips.iter().find(|(n, _, _)| {
            if name.is_empty() {
                clip_number(n) == Some(k + 1)
            } else {
                **n == *name
            }
        })
    }

    /// The events of clip `k` of a rig, `name` in its file.
    pub(crate) fn of_clip(&self, name: &str, k: usize) -> &[EventDef] {
        self.find(name, k).map_or(&[], |(_, events, _)| events)
    }

    /// How clip `k` of a rig, `name` in its file, plays (E12-3e): its `.meta`'s, else looped
    /// with the ground's root motion.
    pub(crate) fn play(&self, name: &str, k: usize) -> ClipPlay {
        self.find(name, k)
            .map_or(ClipPlay::default(), |&(_, _, play)| play)
    }
}

/// `events` (a file's) as the player fires them, sorted by time; moments that are not
/// numbers left out, those before the clip's start at its start.
pub(crate) fn event_defs(events: &[ClipEvent]) -> Vec<EventDef> {
    let mut defs: Vec<EventDef> = events
        .iter()
        .filter(|e| e.time.is_finite())
        .map(|e| EventDef {
            time: e.time.max(0.0),
            name: Arc::from(e.name.as_str()),
            data: Arc::from(e.data.as_str()),
            sound: (!e.sound.is_empty()).then(|| Arc::from(e.sound.as_str())),
        })
        .collect();
    defs.sort_by(|a, b| a.time.total_cmp(&b.time));
    defs
}

/// `Clip 2`'s 2.
fn clip_number(name: &str) -> Option<usize> {
    name.strip_prefix("Clip ")?.parse().ok()
}

/// Loops a tick's play may pass at most: past them, a clip played absurdly fast fires no
/// more.
const MOST_LOOPS: i64 = 64;

/// Calls `fire` for each of `events` (sorted by time) of a clip `length` seconds long that
/// play from `from` to `to` passed, in the order played. Forward, a moment fires when
/// `from <= t < to`; backwards (`to < from`), when `to < t <= from`. `from` and `to` need
/// not lie in the clip: every loop between them fires its events (at most
/// [`MOST_LOOPS`]). An event past the clip's end never fires.
pub(crate) fn crossed<'a>(
    events: &'a [EventDef],
    length: f32,
    from: f32,
    to: f32,
    mut fire: impl FnMut(&'a EventDef),
) {
    if events.is_empty() || length <= 0.0 || !length.is_finite() || from == to {
        return;
    }
    if !from.is_finite() || !to.is_finite() {
        return;
    }
    let (first, last) = ((from / length).floor(), (to / length).floor());
    if to > from {
        let loops = ((last - first) as i64).min(MOST_LOOPS);
        for m in 0..=loops {
            let start = (first + m as f32) * length;
            for e in events.iter().filter(|e| e.time <= length) {
                let t = start + e.time;
                if t >= from && t < to {
                    fire(e);
                }
            }
        }
    } else {
        let loops = ((first - last) as i64).min(MOST_LOOPS);
        for m in 0..=loops {
            let start = (first - m as f32) * length;
            for e in events.iter().rev().filter(|e| e.time <= length) {
                let t = start + e.time;
                if t > to && t <= from {
                    fire(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_scene::ClipEvent;

    fn walk() -> ModelEvents {
        let event = |time: f32, name: &str, data: &str| ClipEvent {
            time,
            name: name.to_owned(),
            data: data.to_owned(),
            sound: String::new(),
        };
        ModelEvents::new(&[
            ClipImport {
                name: String::from("Walk"),
                // Out of order in the file: sorted when read.
                events: vec![
                    event(0.75, "step", "right"),
                    event(0.0, "start", ""),
                    event(0.25, "step", "left"),
                ],
                ..ClipImport::default()
            },
            ClipImport {
                name: String::from("Clip 2"),
                events: vec![event(0.5, "wave", "")],
                ..ClipImport::default()
            },
            ClipImport {
                name: String::from("Idle"),
                events: Vec::new(),
                ..ClipImport::default()
            },
        ])
    }

    fn fired(events: &[EventDef], from: f32, to: f32) -> Vec<String> {
        let mut out = Vec::new();
        crossed(events, 1.0, from, to, |e| {
            out.push(format!("{}{}", e.name, e.data));
        });
        out
    }

    /// E12-3b: the events a span of play passes, forward from the clip's start (its first
    /// moment fires), across its loop, over several loops in one span, backwards (the
    /// latest first), and none while it stands still.
    #[test]
    fn a_span_of_play_fires_the_events_it_passes() {
        let model = walk();
        let events = model.of_clip("Walk", 0);
        let names: Vec<f32> = events.iter().map(|e| e.time).collect();
        assert_eq!(names, [0.0, 0.25, 0.75], "sorted by time");
        assert_eq!(fired(events, 0.0, 0.1), ["start"]);
        assert_eq!(fired(events, 0.1, 0.25), Vec::<String>::new(), "not yet");
        assert_eq!(fired(events, 0.25, 0.3), ["stepleft"], "at its moment");
        // Across the loop (`Animator::advance` shifts the time before alike).
        assert_eq!(fired(events, -0.3, 0.1), ["stepright", "start"]);
        // Two and a half loops in one span.
        assert_eq!(
            fired(events, 0.5, 3.0),
            [
                "stepright",
                "start",
                "stepleft",
                "stepright",
                "start",
                "stepleft",
                "stepright"
            ]
        );
        // Backwards: the latest first, and the start once play goes back past it.
        assert_eq!(fired(events, 0.8, 0.2), ["stepright", "stepleft"]);
        assert_eq!(
            fired(events, 1.0, 0.9),
            ["start"],
            "backwards from the start"
        );
        assert_eq!(fired(events, 0.5, 0.5), Vec::<String>::new(), "paused");
        // A forward tick ending at a moment and the next starting there fire it once.
        let twice = [fired(events, 0.2, 0.25), fired(events, 0.25, 0.3)].concat();
        assert_eq!(twice, ["stepleft"]);
    }

    /// E12-3b: clips found by their names in the file, an unnamed one by its number.
    #[test]
    fn a_clips_events_are_found_by_its_name_or_number() {
        let model = walk();
        assert_eq!(model.of_clip("Walk", 0).len(), 3);
        assert_eq!(model.of_clip("", 1).len(), 1, "the second clip, unnamed");
        assert!(model.of_clip("", 0).is_empty());
        assert!(model.of_clip("Idle", 2).is_empty());
        assert!(model.of_clip("Run", 3).is_empty());
        assert!(!model.is_empty());
        assert!(ModelEvents::new(&[]).is_empty());
    }
}
