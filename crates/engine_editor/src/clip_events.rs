//! A model file's clip events as the Animation tab edits them (ADR 0124's E12-3b-2): its
//! `.meta` held, each edit written back at once and given to the player, undo handing the
//! `.meta`'s text back ([`crate::undo::History::change_file`]).

use std::path::Path;

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
