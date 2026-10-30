//! Clips made in the editor (`.anim` files, ADR 0124's E12-4a), played by any entity's
//! [`Animator`]: its clip found among its `clips` moves the fields the clip keys, each tick
//! after the animators move on, so the game, scripts, saves and the network see the tick's
//! values. A moved transform is drawn between its last two ticks, as every moved one is.
//!
//! A clip is read from its file once (the editor gives the one it edits), its keys laid
//! out per track with the slopes of its smooth keys; an animator binds it once to the
//! entities and fields its tracks name. Each tick then samples and writes, allocating
//! nothing.

use std::collections::HashMap;
use std::sync::Arc;

use engine_core::math::{EulerRot, Quat};
use engine_ecs::{Entity, QueryState, World};
use engine_runtime::{Animator, FieldRef};
use engine_scene::{AnimFile, AnimTrack, AssetId, AssetRef, KeyCurve, Name, split_anim_target};

use crate::clip_events::{EventDef, crossed, event_defs};

/// A track laid out to sample: its keys' times, values and the slopes of its smooth keys.
#[derive(Debug, PartialEq)]
pub(crate) struct FieldTrack {
    /// The names of the entities below the animated one down to the moved one, `/` between
    /// them; empty: itself.
    below: Box<str>,
    /// `Component.field`.
    field: Box<str>,
    /// Numbers a key (1 to 4).
    lanes: usize,
    times: Box<[f32]>,
    /// `lanes` a key.
    values: Box<[f32]>,
    /// Each key's slope per lane (units a second), for the smooth keys beside it.
    slopes: Box<[f32]>,
    curves: Box<[KeyCurve]>,
}

impl FieldTrack {
    /// Track `target` of a file, its keys sorted by time; keys that are not numbers left
    /// out. None for a track without keys.
    fn new(target: &str, track: &AnimTrack) -> Option<Self> {
        let mut keys: Vec<_> = track
            .keys
            .iter()
            .filter(|k| k.time.is_finite() && !k.value.is_empty())
            .collect();
        keys.sort_by(|a, b| a.time.total_cmp(&b.time));
        let lanes = keys.first()?.value.len().min(4);
        let times: Box<[f32]> = keys.iter().map(|k| k.time).collect();
        let values: Box<[f32]> = keys
            .iter()
            .flat_map(|k| (0..lanes).map(|i| k.value.get(i).copied().unwrap_or(0.0)))
            .collect();
        let curves = keys.iter().map(|k| k.curve).collect();
        let slopes = slopes(&times, &values, lanes);
        let (below, field) = split_anim_target(target);
        Some(Self {
            below: Box::from(below),
            field: Box::from(field),
            lanes,
            times,
            values,
            slopes,
            curves,
        })
    }

    /// Its value `t` seconds into the clip, in `out[..lanes]`: the first key's before it,
    /// the last's after it; between two keys as the first one's curve says.
    pub(crate) fn sample(&self, t: f32, out: &mut [f32; 4]) {
        let n = self.times.len();
        let l = self.lanes;
        let after = self.times.partition_point(|&k| k <= t);
        if after == 0 || after == n {
            let k = if after == 0 { 0 } else { n - 1 };
            out[..l].copy_from_slice(&self.values[k * l..k * l + l]);
            return;
        }
        let (a, b) = (after - 1, after);
        let (t0, t1) = (self.times[a], self.times[b]);
        let h = t1 - t0;
        let s = if h > 0.0 { (t - t0) / h } else { 1.0 };
        for (i, o) in out.iter_mut().take(l).enumerate() {
            let (v0, v1) = (self.values[a * l + i], self.values[b * l + i]);
            *o = match self.curves[a] {
                KeyCurve::Step => v0,
                KeyCurve::Linear => v0 + (v1 - v0) * s,
                KeyCurve::Smooth => {
                    let (m0, m1) = (self.slopes[a * l + i], self.slopes[b * l + i]);
                    let (s2, s3) = (s * s, s * s * s);
                    (2.0 * s3 - 3.0 * s2 + 1.0) * v0
                        + (s3 - 2.0 * s2 + s) * h * m0
                        + (-2.0 * s3 + 3.0 * s2) * v1
                        + (s3 - s2) * h * m1
                }
            };
        }
    }
}

/// The slope at each key of each lane for a smooth curve through the keys that never
/// overshoots them (monotone piecewise cubic Hermite, Fritsch and Butland's weighted
/// harmonic mean): flat at the ends and where a key is higher or lower than both its
/// neighbours.
fn slopes(times: &[f32], values: &[f32], lanes: usize) -> Box<[f32]> {
    let n = times.len();
    let mut out = vec![0.0; n * lanes].into_boxed_slice();
    for k in 1..n.saturating_sub(1) {
        let (h0, h1) = (times[k] - times[k - 1], times[k + 1] - times[k]);
        if h0 <= 0.0 || h1 <= 0.0 {
            continue;
        }
        for i in 0..lanes {
            let d0 = (values[k * lanes + i] - values[(k - 1) * lanes + i]) / h0;
            let d1 = (values[(k + 1) * lanes + i] - values[k * lanes + i]) / h1;
            if d0 * d1 <= 0.0 {
                continue;
            }
            out[k * lanes + i] = 3.0 * (h0 + h1) / ((2.0 * h1 + h0) / d0 + (h1 + 2.0 * h0) / d1);
        }
    }
    out
}

/// A clip read from an `.anim` file.
#[derive(Debug, PartialEq)]
pub(crate) struct FieldClip {
    /// Seconds long.
    pub(crate) length: f32,
    /// Plays once and holds its end.
    pub(crate) once: bool,
    tracks: Box<[FieldTrack]>,
    /// Its events in time order.
    pub(crate) events: Vec<EventDef>,
}

impl FieldClip {
    /// The clip `file` holds.
    pub(crate) fn new(file: &AnimFile) -> Self {
        Self {
            length: if file.length.is_finite() {
                file.length.max(0.0)
            } else {
                0.0
            },
            once: file.once,
            tracks: file
                .tracks
                .iter()
                .filter_map(|(target, track)| FieldTrack::new(target, track))
                .collect(),
            events: event_defs(&file.events),
        }
    }
}

/// A field the editor's preview of a clip changed, and the value it had (ADR 0124's
/// E12-4b): what the editor puts back before it edits, so the scene keeps its own values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeptField {
    entity: Entity,
    field: FieldRef,
    value: [f32; 4],
}

impl KeptField {
    /// The field given its value back (nothing when the entity or its component is gone).
    pub fn put_back(&self, world: &mut World) {
        self.field
            .write(world, self.entity, &self.value[..self.field.lanes()]);
    }

    /// The entity whose field it is.
    pub fn entity(&self) -> Entity {
        self.entity
    }
}

/// Where a track writes: the entity and its field.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Target {
    entity: Entity,
    field: FieldRef,
    /// A rotation keyed as three Euler angles in degrees.
    euler: bool,
}

/// A clip bound to an animated entity: per track, where it writes (none for a target not
/// found).
#[derive(Debug)]
struct Binding {
    file: AssetId,
    clip: Arc<FieldClip>,
    targets: Box<[Option<Target>]>,
}

/// An animator's bound clips: every clip it has played (kept, so playing one again binds
/// nothing and allocates nothing), and which of them it plays and fades out.
#[derive(Debug, Default)]
struct Bound {
    bindings: Vec<Binding>,
    main: Option<usize>,
    from: Option<usize>,
    /// The clips' versions it was bound against.
    epoch: u64,
}

impl Bound {
    fn main(&self) -> Option<&Binding> {
        self.main.map(|i| &self.bindings[i])
    }

    fn from(&self) -> Option<&Binding> {
        self.from.map(|i| &self.bindings[i])
    }

    /// Whether it plays file `main` and fades out file `from` as bound now.
    fn binds(&self, main: Option<AssetId>, from: Option<AssetId>, epoch: u64) -> bool {
        self.epoch == epoch
            && self.main().map(|b| b.file) == main
            && self.from().map(|b| b.file) == from
    }
}

/// What one animator asks of a tick, read before anything is written.
#[derive(Debug, Clone, Copy)]
struct Work {
    entity: Entity,
    /// The files of the clips it plays and fades out, where they are its own.
    main: Option<AssetRef>,
    from: Option<AssetRef>,
    time: f32,
    from_time: f32,
    /// The playing clip's weight: 1 outside a fade.
    weight: f32,
    previous: f32,
    from_previous: f32,
    /// The lengths (and holds) the animator knows of its clips now.
    length: (f32, bool),
    from_length: (f32, bool),
}

/// The clips made in the editor and the animators playing them.
#[derive(Default)]
pub(crate) struct FieldClips {
    /// Clips read, by file; `None`: it could not be read (not tried again until it changes).
    read: HashMap<AssetId, Option<Arc<FieldClip>>>,
    /// Clips the editor holds as it edits them, read in place of their files.
    held: HashMap<AssetId, Arc<FieldClip>>,
    /// Each animator's bindings, in the order the animators came in the last tick (the
    /// same tick after tick until animators come or go: found without a lookup).
    bound: Vec<(Entity, Bound)>,
    animators: Option<QueryState<(Entity, &'static Animator)>>,
    work: Vec<Work>,
    /// Bumped when a clip changes: bindings made before are made again.
    epoch: u64,
    /// The events the last tick played past: (entity, event).
    pub(crate) fired: Vec<(Entity, EventDef)>,
    /// The editor's preview (E12-4b): the entity it shows a clip on, bound as an animator.
    preview: Option<(Entity, Bound)>,
}

impl core::fmt::Debug for FieldClips {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FieldClips")
            .field("read", &self.read.len())
            .field("bound", &self.bound.len())
            .finish_non_exhaustive()
    }
}

impl FieldClips {
    /// Clip file `file` shown on `entity` at `time` seconds while the editor edits (E12-4b):
    /// the fields its tracks key written as it says there, each one's value before it put in
    /// `kept` the first time (a field already there is not kept again). The entity is bound
    /// as an animator is, once per clip.
    pub(crate) fn preview(
        &mut self,
        world: &mut World,
        entity: Entity,
        file: AssetRef,
        time: f32,
        kept: &mut Vec<KeptField>,
        mut path_of: impl FnMut(AssetRef) -> Option<std::path::PathBuf>,
    ) {
        let mut bound = match self.preview.take() {
            Some((e, b)) if e == entity => b,
            _ => Bound::default(),
        };
        if bound.epoch != self.epoch {
            bound.bindings.clear();
            bound.epoch = self.epoch;
        }
        bound.main = self.binding(world, entity, file, &mut bound, &mut path_of);
        bound.from = None;
        if let Some(b) = bound.main() {
            // The editor's playhead lies in the clip: its end shows the last keys, also
            // for a clip that loops.
            let t = time.clamp(0.0, b.clip.length);
            let mut value = [0.0f32; 4];
            for (track, target) in b.clip.tracks.iter().zip(&b.targets) {
                let Some(target) = target else {
                    continue;
                };
                let known = kept
                    .iter()
                    .any(|k| k.entity == target.entity && k.field == target.field);
                let mut was = [0.0f32; 4];
                if !known && target.field.read(world, target.entity, &mut was) {
                    kept.push(KeptField {
                        entity: target.entity,
                        field: target.field,
                        value: was,
                    });
                }
                track.sample(t, &mut value);
                write(world, target, &value, track.lanes);
            }
        }
        self.preview = Some((entity, bound));
    }

    /// File `file` read again on its next use (it changed on disk).
    pub(crate) fn forget(&mut self, file: AssetId) {
        if self.read.remove(&file).is_some() {
            self.epoch += 1;
        }
    }

    /// Clip file `file` as the editor holds it, read in place of the file (`None`: the
    /// file again). Its animators follow from the next tick.
    pub(crate) fn hold(&mut self, file: AssetId, anim: Option<&AnimFile>) {
        match anim {
            Some(anim) => self.held.insert(file, Arc::new(FieldClip::new(anim))),
            None => self.held.remove(&file),
        };
        self.read.remove(&file);
        self.epoch += 1;
    }

    /// The clip of file `file`: the editor's, else read from `path` (under the project's
    /// root) once.
    fn clip(&mut self, file: AssetId, path: Option<std::path::PathBuf>) -> Option<Arc<FieldClip>> {
        if let Some(held) = self.held.get(&file) {
            return Some(Arc::clone(held));
        }
        self.read
            .entry(file)
            .or_insert_with(|| {
                let path = path?;
                AnimFile::load(&path)
                    .map(|anim| Arc::new(FieldClip::new(&anim)))
                    .inspect_err(|e| log::warn!("animation clip: {e}"))
                    .ok()
            })
            .clone()
    }

    /// One tick of the animators that play clips made in the editor (after the animators
    /// moved on): their lengths told back for the next tick to wrap or hold at, their
    /// fields written at the tick's moment (through a fade, the two clips' values blended
    /// by its weight), their events collected into `fired`. `path_of` finds a clip file's
    /// path on disk.
    pub(crate) fn tick(
        &mut self,
        world: &mut World,
        mut path_of: impl FnMut(AssetRef) -> Option<std::path::PathBuf>,
    ) {
        self.fired.clear();
        let animators = self.animators.get_or_insert_with(|| QueryState::new(world));
        self.work.clear();
        let work = &mut self.work;
        animators.for_each(world, |(entity, animator)| {
            if animator.clips.is_empty() {
                return;
            }
            let main = animator.clips.get(&animator.clip).copied();
            let from = if animator.fade > 0.0 {
                animator.clips.get(&animator.from_clip).copied()
            } else {
                None
            };
            if main.is_none() && from.is_none() {
                return;
            }
            work.push(Work {
                entity,
                main,
                from,
                time: animator.time,
                from_time: animator.from_time,
                weight: animator.weight(1.0),
                previous: animator.previous,
                from_previous: animator.from_previous,
                length: (animator.length, animator.hold),
                from_length: (animator.from_length, animator.from_hold),
            });
        });
        self.line_up();
        // Taken while the bindings change (given back below: no allocation).
        let work = core::mem::take(&mut self.work);
        for (k, w) in work.iter().enumerate() {
            let (main, from) = (w.main.map(|f| f.file), w.from.map(|f| f.file));
            if !self.bound[k].1.binds(main, from, self.epoch) {
                self.bind(world, k, w, &mut path_of);
            }
            let bound = &self.bound[k].1;
            // The lengths back to the animator, for its next tick to wrap or hold at.
            let main = bound.main().map(|b| (b.clip.length, b.clip.once));
            let from = bound.from().map(|b| (b.clip.length, b.clip.once));
            let told = main.is_none_or(|m| m == w.length)
                && (w.from.is_none() || from.is_none_or(|f| f == w.from_length));
            if !told && let Some(animator) = world.get_mut::<Animator>(w.entity) {
                if let Some((length, once)) = main {
                    animator.length = length;
                    animator.hold = once;
                }
                if let Some((length, once)) = from
                    && animator.fade > 0.0
                {
                    animator.from_length = length;
                    animator.from_hold = once;
                }
            }
            write_fields(world, bound, w);
            // The events of the clip that weighs more over the tick's span.
            let (binding, from, to) = if bound.from.is_some() && w.weight < 0.5 {
                (bound.from(), w.from_previous, w.from_time)
            } else {
                (bound.main(), w.previous, w.time)
            };
            if let Some(b) = binding {
                let fired = &mut self.fired;
                crossed(&b.clip.events, b.clip.length, from, to, |event| {
                    fired.push((w.entity, event.clone()));
                });
            }
        }
        self.work = work;
    }

    /// The bindings lined up with this tick's animators, `bound[k]` the `k`th's: as they
    /// were when the same animators come in the same order (every tick but those after
    /// animators came or went), else moved to their animators' places, new ones empty and
    /// the gone ones' let go.
    fn line_up(&mut self) {
        let same = self.bound.len() == self.work.len()
            && self
                .bound
                .iter()
                .zip(&self.work)
                .all(|((e, _), w)| *e == w.entity);
        if same {
            return;
        }
        let mut old = core::mem::take(&mut self.bound);
        old.sort_unstable_by_key(|(e, _)| *e);
        self.bound.reserve(self.work.len());
        for w in &self.work {
            let bound = match old.binary_search_by_key(&w.entity, |(e, _)| *e) {
                Ok(i) => core::mem::take(&mut old[i].1),
                Err(_) => Bound::default(),
            };
            self.bound.push((w.entity, bound));
        }
    }

    /// The `k`th animator (`w`'s) bound to the clips it plays and fades out: a clip it
    /// played before found among its bindings, one new to it bound (all made again once a
    /// clip changed).
    fn bind(
        &mut self,
        world: &mut World,
        k: usize,
        w: &Work,
        path_of: &mut impl FnMut(AssetRef) -> Option<std::path::PathBuf>,
    ) {
        let mut bound = core::mem::take(&mut self.bound[k].1);
        if bound.epoch != self.epoch {
            bound.bindings.clear();
            bound.epoch = self.epoch;
        }
        bound.main = w
            .main
            .and_then(|f| self.binding(world, w.entity, f, &mut bound, path_of));
        bound.from = w
            .from
            .and_then(|f| self.binding(world, w.entity, f, &mut bound, path_of));
        self.bound[k].1 = bound;
    }

    /// File `file`'s clip bound to `entity`: its place among `bound`'s bindings, the binding
    /// made when it has none (none for a clip that cannot be read).
    fn binding(
        &mut self,
        world: &mut World,
        entity: Entity,
        file: AssetRef,
        bound: &mut Bound,
        path_of: &mut impl FnMut(AssetRef) -> Option<std::path::PathBuf>,
    ) -> Option<usize> {
        if let Some(i) = bound.bindings.iter().position(|b| b.file == file.file) {
            return Some(i);
        }
        let clip = self.clip(file.file, path_of(file))?;
        let targets = clip
            .tracks
            .iter()
            .map(|track| {
                let target = below(world, entity, &track.below)?;
                let field = FieldRef::find(world, &track.field)?;
                let euler = field.is_rotation() && track.lanes == 3;
                if !euler && field.lanes() != track.lanes {
                    log::warn!(
                        "animation clip: `{}` holds {} numbers, its keys {}",
                        track.field,
                        field.lanes(),
                        track.lanes
                    );
                    return None;
                }
                Some(Target {
                    entity: target,
                    field,
                    euler,
                })
            })
            .collect();
        bound.bindings.push(Binding {
            file: file.file,
            clip,
            targets,
        });
        Some(bound.bindings.len() - 1)
    }
}

/// The entity `path` names below `root` (its children's names, `/` between them); `root`
/// itself for an empty path.
fn below(world: &World, root: Entity, path: &str) -> Option<Entity> {
    let mut at = root;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        at = world
            .children(at)
            .find(|&c| world.get::<Name>(c).is_some_and(|n| n.0 == part))?;
    }
    Some(at)
}

/// Where `target` is among `binding`'s targets.
fn same_target(binding: &Binding, target: &Target) -> Option<usize> {
    binding
        .targets
        .iter()
        .position(|o| o.is_some_and(|o| o.entity == target.entity && o.field == target.field))
}

/// The fields of `bound`'s clips written at `w`'s moments: the playing clip's, blended
/// with the fading one's where both key a field; a field only the fading one keys keeps
/// its value until the fade ends. A clip's tracks are few: the pairs are found as written.
fn write_fields(world: &mut World, bound: &Bound, w: &Work) {
    let mut value = [0.0f32; 4];
    let mut other = [0.0f32; 4];
    let (main, from) = (bound.main(), bound.from());
    if let Some(main) = main {
        for (track, target) in main.clip.tracks.iter().zip(&main.targets) {
            let Some(target) = target else {
                continue;
            };
            track.sample(clip_time(&main.clip, w.time), &mut value);
            if let Some(from) = from
                && let Some(j) = same_target(from, target)
            {
                from.clip.tracks[j].sample(clip_time(&from.clip, w.from_time), &mut other);
                for (v, o) in value.iter_mut().zip(other).take(track.lanes) {
                    *v = o + (*v - o) * w.weight;
                }
            }
            write(world, target, &value, track.lanes);
        }
    }
    if let Some(from) = from {
        for (track, target) in from.clip.tracks.iter().zip(&from.targets) {
            let Some(target) = target else {
                continue;
            };
            if main.is_some_and(|m| same_target(m, target).is_some()) {
                continue;
            }
            track.sample(clip_time(&from.clip, w.from_time), &mut value);
            write(world, target, &value, track.lanes);
        }
    }
}

/// Seconds into `clip` at an animator's `time`: wrapped into a looping clip, held within
/// one played once.
fn clip_time(clip: &FieldClip, time: f32) -> f32 {
    if clip.length <= 0.0 {
        0.0
    } else if clip.once {
        time.clamp(0.0, clip.length)
    } else {
        time.rem_euclid(clip.length)
    }
}

/// `value[..lanes]` written to `target`'s field: a rotation keyed in Euler degrees as the
/// quaternion they make (pitch about X, yaw about Y, roll about Z, applied yaw first, as
/// the inspector shows it).
fn write(world: &mut World, target: &Target, value: &[f32; 4], lanes: usize) {
    if target.euler {
        let [pitch, yaw, roll] = [value[0], value[1], value[2]].map(f32::to_radians);
        let q = Quat::from_euler(EulerRot::YXZ, yaw, pitch, roll);
        target.field.write(world, target.entity, &q.to_array());
    } else {
        target.field.write(world, target.entity, &value[..lanes]);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;

    fn track(keys: &[(f32, f32, KeyCurve)]) -> FieldTrack {
        let mut anim = AnimFile::default();
        for &(t, v, _) in keys {
            anim.set_key("Lamp:PointLight.range", t, &[v]);
        }
        for (k, &(_, _, curve)) in keys.iter().enumerate() {
            anim.tracks.get_mut("Lamp:PointLight.range").unwrap().keys[k].curve = curve;
        }
        FieldTrack::new(
            "Lamp:PointLight.range",
            &anim.tracks["Lamp:PointLight.range"],
        )
        .unwrap()
    }

    fn at(track: &FieldTrack, t: f32) -> f32 {
        let mut out = [0.0; 4];
        track.sample(t, &mut out);
        out[0]
    }

    #[test]
    fn a_track_holds_its_ends_and_follows_each_keys_curve() {
        use KeyCurve::{Linear, Smooth, Step};
        let t = track(&[(0.0, 1.0, Step), (1.0, 3.0, Linear), (2.0, 5.0, Smooth)]);
        assert_eq!((&*t.below, &*t.field), ("Lamp", "PointLight.range"));
        assert_eq!(at(&t, -1.0), 1.0, "before the first key");
        assert_eq!(at(&t, 0.5), 1.0, "a step holds");
        assert_eq!(at(&t, 1.0), 3.0, "at a key, its value");
        assert_eq!(at(&t, 1.5), 4.0, "a line");
        assert_eq!(at(&t, 9.0), 5.0, "after the last key");
    }

    #[test]
    fn smooth_keys_ease_in_and_out_and_never_overshoot() {
        use KeyCurve::Smooth;
        // Two keys: an ease in and out, halfway at half the time.
        let door = track(&[(0.0, 0.0, Smooth), (1.0, 90.0, Smooth)]);
        assert!((at(&door, 0.5) - 45.0).abs() < 1e-4);
        assert!(
            at(&door, 0.1) < 9.0,
            "slow out of the first key: {}",
            at(&door, 0.1)
        );
        // A peak stays the highest value; a rise keeps rising.
        let peak = track(&[(0.0, 0.0, Smooth), (1.0, 1.0, Smooth), (1.5, 0.0, Smooth)]);
        let mut last = 0.0;
        for k in 0..=100 {
            let v = at(&peak, k as f32 * 0.015);
            assert!(v <= 1.0 + 1e-6, "overshot at {}: {v}", k as f32 * 0.015);
            if k as f32 * 0.015 <= 1.0 {
                assert!(
                    v >= last - 1e-6,
                    "fell while rising at {}",
                    k as f32 * 0.015
                );
                last = v;
            }
        }
        let rise = track(&[(0.0, 0.0, Smooth), (1.0, 1.0, Smooth), (3.0, 10.0, Smooth)]);
        let mut last = -1.0;
        for k in 0..=300 {
            let v = at(&rise, k as f32 * 0.01);
            assert!(
                v >= last - 1e-5 && v <= 10.0 + 1e-5,
                "at {}: {v}",
                k as f32 * 0.01
            );
            last = v;
        }
    }

    fn yaw(world: &World, e: Entity) -> f32 {
        let q = world.get::<engine_ecs::Transform>(e).unwrap().rotation;
        q.to_euler(EulerRot::YXZ).0.to_degrees()
    }

    /// ADR 0124's E12-4a on a bare world: a door whose animator plays one of its own clips
    /// turns as its keys say each tick, its lamp's range steps, the clip played once holds
    /// its end, its event fires once; a fade to another clip blends the two; an animator
    /// playing a clip it does not own is left alone.
    #[test]
    fn an_animator_playing_its_own_clip_moves_the_fields_it_keys() {
        use engine_core::math::Vec3;
        use engine_ecs::Transform;
        use engine_runtime::{Animators, PointLight};
        use engine_scene::ClipEvent;
        let mut world = World::new();
        let (open, shut) = (
            AssetRef::file(AssetId::from_u128(42)),
            AssetRef::file(AssetId::from_u128(43)),
        );
        let door = world.spawn((
            Transform::IDENTITY,
            Animator::new("open")
                .with_clip("open", open)
                .with_clip("shut", shut),
        ));
        let lamp = world.spawn((Name::new("Lamp"), PointLight::new(Vec3::ONE, 2.0)));
        world.set_parent(lamp, Some(door)).unwrap();
        let walker = world.spawn((
            Transform::IDENTITY,
            Animator::new("Walk").with_clip("open", open),
        ));
        let mut opening = AnimFile {
            once: true,
            ..AnimFile::default()
        };
        opening.set_key("Transform.rotation", 0.0, &[0.0, 0.0, 0.0]);
        opening.set_key("Transform.rotation", 1.0, &[0.0, 90.0, 0.0]);
        opening.set_key("Lamp:PointLight.range", 0.0, &[2.0]);
        opening
            .tracks
            .get_mut("Lamp:PointLight.range")
            .unwrap()
            .keys[0]
            .curve = KeyCurve::Step;
        opening.set_key("Lamp:PointLight.range", 0.5, &[8.0]);
        opening.events.push(ClipEvent {
            time: 0.5,
            name: String::from("creak"),
            ..ClipEvent::default()
        });
        let mut shutting = AnimFile {
            once: true,
            ..AnimFile::default()
        };
        shutting.set_key("Transform.rotation", 0.0, &[0.0, 90.0, 0.0]);
        shutting.set_key("Transform.rotation", 1.0, &[0.0, 0.0, 0.0]);
        let mut clips = FieldClips::default();
        clips.hold(open.file, Some(&opening));
        clips.hold(shut.file, Some(&shutting));
        let mut animators = Animators::new(&mut world);
        let dt = 1.0 / 64.0;
        let mut creaks = 0;
        let mut tick = |world: &mut World, clips: &mut FieldClips| {
            animators.advance(world, dt);
            clips.tick(world, |_| None);
            clips
                .fired
                .iter()
                .filter(|(e, event)| *e == door && &*event.name == "creak")
                .count()
        };
        for _ in 0..32 {
            creaks += tick(&mut world, &mut clips);
        }
        // Half a second in: halfway round (an ease in and out), the lamp's step taken.
        assert!(
            (yaw(&world, door) - 45.0).abs() < 0.01,
            "{}",
            yaw(&world, door)
        );
        assert_eq!(world.get::<PointLight>(lamp).unwrap().range, 8.0);
        for _ in 0..64 {
            creaks += tick(&mut world, &mut clips);
        }
        assert!((yaw(&world, door) - 90.0).abs() < 0.01, "held open");
        let animator = world.get::<Animator>(door).unwrap();
        assert!(animator.hold && animator.length == 1.0 && animator.time == 1.0);
        assert_eq!(creaks, 1, "its event fired once");
        assert_eq!(
            yaw(&world, walker),
            0.0,
            "a clip it does not own moves nothing"
        );
        // Shut over a fade: the two clips blend, the door turning back steadily.
        world.get_mut::<Animator>(door).unwrap().play("shut", 0.25);
        let mut last = yaw(&world, door);
        for _ in 0..80 {
            tick(&mut world, &mut clips);
            let now = yaw(&world, door);
            assert!(
                now <= last + 0.01,
                "turned back the other way: {last} then {now}"
            );
            last = now;
        }
        assert!(
            yaw(&world, door).abs() < 0.01,
            "shut: {}",
            yaw(&world, door)
        );
        // The editor's edit is followed from the next tick.
        shutting.set_key("Transform.rotation", 1.0, &[0.0, -30.0, 0.0]);
        clips.hold(shut.file, Some(&shutting));
        tick(&mut world, &mut clips);
        assert!(
            (yaw(&world, door) + 30.0).abs() < 0.01,
            "{}",
            yaw(&world, door)
        );
    }

    /// What a tick of clips made in the editor costs: 1,000 doors, each turning (a rotation
    /// keyed in Euler degrees, three smooth keys) with its lamp's range stepping (two keys),
    /// the doors' clips at different moments. Run with `--release --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    fn measure_a_thousand_doors() {
        use engine_core::math::Vec3;
        use engine_ecs::Transform;
        use engine_runtime::{Animators, PointLight};
        let mut world = World::new();
        let file = AssetRef::file(AssetId::from_u128(9));
        let mut anim = AnimFile::default();
        anim.set_key("Transform.rotation", 0.0, &[0.0, 0.0, 0.0]);
        anim.set_key("Transform.rotation", 0.5, &[0.0, 90.0, 0.0]);
        anim.set_key("Transform.rotation", 1.0, &[0.0, 0.0, 0.0]);
        anim.set_key("Lamp:PointLight.range", 0.0, &[2.0]);
        anim.set_key("Lamp:PointLight.range", 0.5, &[6.0]);
        let mut clips = FieldClips::default();
        clips.hold(file.file, Some(&anim));
        for i in 0..1000 {
            let door = world.spawn((
                Transform::from_xyz(i as f32, 0.0, 0.0),
                Animator {
                    time: i as f32 * 0.001,
                    ..Animator::new("swing").with_clip("swing", file)
                },
            ));
            let lamp = world.spawn((Name::new("Lamp"), PointLight::new(Vec3::ONE, 2.0)));
            world.set_parent(lamp, Some(door)).unwrap();
        }
        let mut animators = Animators::new(&mut world);
        let dt = 1.0 / 64.0;
        for _ in 0..64 {
            animators.advance(&mut world, dt);
            clips.tick(&mut world, |_| None);
        }
        let ticks = 640;
        let started = std::time::Instant::now();
        for _ in 0..ticks {
            clips.tick(&mut world, |_| None);
        }
        let per_tick = started.elapsed() / ticks;
        println!(
            "1,000 doors, two tracks each: {per_tick:?} a tick, {:?} a door",
            per_tick / 1000
        );
    }

    /// ADR 0124's E12-4b: the editor's preview writes a clip's values at a moment and keeps
    /// what the fields held; put back, the scene has its own values again, and a preview
    /// at another moment keeps nothing twice.
    #[test]
    fn a_preview_shows_a_clip_and_its_kept_values_put_the_scene_back() {
        use engine_core::math::Vec3;
        use engine_ecs::Transform;
        use engine_runtime::PointLight;
        let mut world = World::new();
        let file = AssetRef::file(AssetId::from_u128(77));
        let door = world.spawn((
            Transform::from_rotation(Quat::from_rotation_y(0.3)),
            Animator::new("open").with_clip("open", file),
        ));
        let lamp = world.spawn((Name::new("Lamp"), PointLight::new(Vec3::ONE, 3.0)));
        world.set_parent(lamp, Some(door)).unwrap();
        let mut anim = AnimFile::default();
        anim.set_key("Transform.rotation", 0.0, &[0.0, 0.0, 0.0]);
        anim.set_key("Transform.rotation", 1.0, &[0.0, 90.0, 0.0]);
        anim.set_key("Lamp:PointLight.range", 0.0, &[8.0]);
        let mut clips = FieldClips::default();
        clips.hold(file.file, Some(&anim));
        let mut kept = Vec::new();
        clips.preview(&mut world, door, file, 1.0, &mut kept, |_| None);
        assert!((yaw(&world, door) - 90.0).abs() < 0.01);
        assert_eq!(world.get::<PointLight>(lamp).unwrap().range, 8.0);
        assert_eq!(kept.len(), 2, "both fields kept");
        // Again at half a second: the values move, nothing is kept twice.
        clips.preview(&mut world, door, file, 0.5, &mut kept, |_| None);
        assert!((yaw(&world, door) - 45.0).abs() < 0.01);
        assert_eq!(kept.len(), 2);
        for k in kept.drain(..) {
            k.put_back(&mut world);
        }
        assert!(
            (yaw(&world, door) - 0.3f32.to_degrees()).abs() < 0.01,
            "the scene's own"
        );
        assert_eq!(world.get::<PointLight>(lamp).unwrap().range, 3.0);
    }

    #[test]
    fn a_clip_wraps_or_holds_its_time() {
        let mut anim = AnimFile {
            length: 2.0,
            ..AnimFile::default()
        };
        anim.set_key("Transform.scale.x", 0.0, &[1.0]);
        let mut clip = FieldClip::new(&anim);
        assert_eq!(clip_time(&clip, 2.5), 0.5, "looped");
        clip.once = true;
        assert_eq!(clip_time(&clip, 2.5), 2.0, "held at its end");
        assert_eq!(clip_time(&clip, -1.0), 0.0);
    }
}
