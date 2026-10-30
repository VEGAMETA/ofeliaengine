//! What a character plays (ADR 0124's E12-2): the [`Animator`], saved with its entity, and
//! [`Animators::advance`], which moves every animator on each tick (the player calls it
//! after the scripts' tick and before `Game::tick`). Each rendered frame then poses the
//! character at the frame's moment between the last two ticks ([`Animator::at`]). A
//! [`JointAttachment`] under a character follows one of its joints (E12-1c). The clips'
//! events a tick played past are its [`AnimationEvents`] (E12-3b). An animator may play an
//! animation graph instead of a clip (E12-5c), its parameters named by the graph
//! ([`GraphParams`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use engine_core::math::Vec3;
use engine_ecs::{Entity, QueryState, World, reflect_component};
use engine_reflect::Reflect;
use engine_scene::AssetRef;

/// What an [`Animator`] does with its clips' root motion (ADR 0124's E12-2c): its root
/// joint's crossing of the ground and its turn about the character's up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum RootMotion {
    /// Left in the pose, as the clip was made: a walk that crosses the ground goes back to
    /// where it started each loop.
    #[default]
    InPose = 0,
    /// Taken out of the pose, and moves the entity's `Transform` each tick.
    Move = 1,
    /// Taken out of the pose, and left in `moved` and `turned` each tick for the game to
    /// move the character by (a character controller).
    Report = 2,
}

/// A character's animation (ADR 0124's E12-2): its skinned model plays the clip named here,
/// looped. Its time moves on each tick by the tick's length times `speed` (none while
/// `paused`); each rendered frame poses the model at the frame's moment between the last
/// two ticks, so a slow tick rate still moves smoothly. While the editor edits, the world
/// stands still and the character shows its clip at `time` (its first frame unless set).
///
/// ```ignore
/// world.spawn((Model::new(mesh, material), Animator::new("Walk")));
/// ```
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct Animator {
    /// The clip, by its name in the model's file or among `clips`; empty, or a name neither
    /// has: the model's first.
    #[reflect(hint = "clip")]
    pub clip: String,
    /// Clips made in the editor (`.anim` files, ADR 0124's E12-4a) it plays by these names,
    /// besides its model's (a name here wins): a door's `open`, a lamp's `flicker`. Any
    /// entity can play them; they move the fields they key.
    #[reflect(hidden)]
    pub clips: BTreeMap<String, AssetRef>,
    /// How fast it plays: 1 at its own pace, 0.5 half as fast, negative backwards.
    pub speed: f32,
    /// Held where it is.
    pub paused: bool,
    /// Seconds into the clip. The player moves it each tick (saves keep it, so a load goes
    /// on from there); setting it moves the clip there.
    pub time: f32,
    /// What its clips' root motion does (E12-2c).
    pub root_motion: RootMotion,
    /// The clip fading out while `clip` fades in ([`Self::play`], ADR 0124's E12-2b): its
    /// name, kept while `fade` is above 0. Saved, so a load goes on fading.
    #[reflect(hidden)]
    pub from_clip: String,
    /// Seconds into the clip fading out, moving on with `time`.
    #[reflect(hidden)]
    pub from_time: f32,
    /// The fade's length in seconds; 0: none.
    #[reflect(hidden)]
    pub fade: f32,
    /// Seconds into the fade.
    #[reflect(hidden)]
    pub faded: f32,
    /// The clip's length, as the player last found it (0 before): where `time` wraps.
    #[reflect(skip)]
    pub length: f32,
    /// The clip fading out's length, likewise.
    #[reflect(skip)]
    pub from_length: f32,
    /// `from_time` and `faded` at the tick before the last, as `previous` is for `time`.
    #[reflect(skip)]
    pub from_previous: f32,
    /// See `from_previous`.
    #[reflect(skip)]
    pub faded_previous: f32,
    /// `time` at the tick before the last, in step with `time` across a wrap.
    #[reflect(skip)]
    pub previous: f32,
    /// A tick set `previous` since the animator was made or loaded.
    #[reflect(skip)]
    pub ticked: bool,
    /// The clip plays once and holds its last moment (its model's `.meta` says so, ADR
    /// 0124's E12-3e), as the player last found it: `time` stops at `length` instead of
    /// wrapping.
    #[reflect(skip)]
    pub hold: bool,
    /// Likewise for the clip fading out.
    #[reflect(skip)]
    pub from_hold: bool,
    /// The last tick's root motion taken out of the pose (`RootMotion::Move` or `Report`):
    /// how far the character crossed the ground, in its parent's space.
    #[reflect(skip)]
    pub moved: Vec3,
    /// Likewise, its turn about its own up, in radians (counterclockwise seen from above).
    #[reflect(skip)]
    pub turned: f32,
    /// The animation graph it plays (an `.agraph`, ADR 0124's E12-5) in place of `clip`;
    /// none: it plays `clip`.
    pub graph: AssetRef,
    /// Its graph's parameters, by the slots its graph names them in ([`GraphParams`]): a
    /// number, a flag's 0 or 1, a trigger's count of fires not yet taken by a transition.
    /// Scripts set them by name (`animator:set("speed", 3.5)`, `animator:trigger("jump")`).
    #[reflect(hidden)]
    pub params: [f32; MAX_GRAPH_PARAMS],
    /// The state its graph's first layer is in, by name, as the last tick left it (empty
    /// without a graph): what scripts read as `animator.state`. Not sent over the network:
    /// every peer's graph makes its own.
    #[reflect(hidden, net_skip)]
    pub state: String,
}

/// The most parameters an animation graph has (ADR 0124's E12-5).
pub const MAX_GRAPH_PARAMS: usize = 16;

reflect_component!(Animator);

impl Default for Animator {
    fn default() -> Self {
        Self {
            clip: String::new(),
            clips: BTreeMap::new(),
            speed: 1.0,
            paused: false,
            time: 0.0,
            root_motion: RootMotion::InPose,
            from_clip: String::new(),
            from_time: 0.0,
            fade: 0.0,
            faded: 0.0,
            length: 0.0,
            from_length: 0.0,
            from_previous: 0.0,
            faded_previous: 0.0,
            previous: 0.0,
            ticked: false,
            hold: false,
            from_hold: false,
            moved: Vec3::ZERO,
            turned: 0.0,
            graph: AssetRef::NONE,
            params: [0.0; MAX_GRAPH_PARAMS],
            state: String::new(),
        }
    }
}

impl Animator {
    /// Playing clip `clip` from its start at its own pace.
    pub fn new(clip: &str) -> Self {
        Self {
            clip: clip.to_owned(),
            ..Self::default()
        }
    }

    /// With clip file `file` (an `.anim`) among its clips as `name` (ADR 0124's E12-4a).
    pub fn with_clip(mut self, name: &str, file: AssetRef) -> Self {
        self.clips.insert(name.to_owned(), file);
        self
    }

    /// Whether its clip is one of its `clips` (made in the editor), not its model's.
    pub fn plays_own_clip(&self) -> bool {
        self.clips.contains_key(&self.clip)
    }

    /// Whether it plays an animation graph (E12-5c) rather than a clip.
    pub fn plays_graph(&self) -> bool {
        self.graph != AssetRef::NONE
    }

    /// Parameter `name` of its graph set to `value` (a flag: 0 or 1), its slot found in
    /// `params` ([`GraphParams`]). False when the graph has no such parameter, or it is a
    /// trigger (fired by [`Self::trigger`], not set).
    pub fn set(&mut self, params: &GraphParams, name: &str, value: f32) -> bool {
        match params.slot(self.graph.file, name) {
            Some((slot, false)) => self.set_slot(slot, value),
            _ => false,
        }
    }

    /// Trigger `name` of its graph fired: it holds until a transition takes it. False when
    /// the graph has no such trigger.
    pub fn trigger(&mut self, params: &GraphParams, name: &str) -> bool {
        match params.slot(self.graph.file, name) {
            Some((slot, true)) => self.fire_slot(slot),
            _ => false,
        }
    }

    /// The parameter in slot `slot` of its graph set to `value`: what [`Self::set`] does
    /// once it found the slot, for a caller that found it itself (the scripts' binding,
    /// which reads [`GraphParams`] and then writes the animator). False for a slot past the
    /// last ([`MAX_GRAPH_PARAMS`]), which holds nothing.
    pub fn set_slot(&mut self, slot: u16, value: f32) -> bool {
        let Some(param) = self.params.get_mut(usize::from(slot)) else {
            return false;
        };
        *param = value;
        true
    }

    /// The trigger in slot `slot` of its graph fired, as [`Self::trigger`] fires it once it
    /// found the slot. False for a slot past the last.
    pub fn fire_slot(&mut self, slot: u16) -> bool {
        let Some(param) = self.params.get_mut(usize::from(slot)) else {
            return false;
        };
        *param += 1.0;
        true
    }

    /// Plays clip `clip` from its start, fading from what plays now over `fade` seconds (at
    /// once for 0 or less): what scripts call as `animator:play(clip, fade)` (ADR 0124's
    /// E12-2b). A fade cut short by another lets the clip that was fading out go.
    pub fn play(&mut self, clip: &str, fade: f32) {
        if fade > 0.0 {
            core::mem::swap(&mut self.from_clip, &mut self.clip);
            self.from_time = self.time;
            self.from_previous = self.time;
            self.from_length = self.length;
            self.from_hold = self.hold;
            self.fade = fade;
        } else {
            self.from_clip.clear();
            self.fade = 0.0;
        }
        self.faded = 0.0;
        self.faded_previous = 0.0;
        self.clip.clear();
        self.clip.push_str(clip);
        self.time = 0.0;
        self.previous = 0.0;
        self.length = 0.0;
        self.hold = false;
    }

    /// One tick of `dt` seconds: `time` moves on by `dt` times the speed (held while
    /// paused), wrapped into the clip's length once it is known; the time before the tick
    /// is kept, shifted alike, for the frames that fall between this tick and the next. A
    /// fade moves on with it (the clip fading out at the same speed), and ends the tick
    /// after it reaches its length.
    pub fn advance(&mut self, dt: f32) {
        let step = if self.paused { 0.0 } else { dt * self.speed };
        (self.previous, self.time) = moved_on(self.time, step, self.length, self.hold);
        if self.fade > 0.0 && self.faded >= self.fade {
            self.from_clip.clear();
            self.fade = 0.0;
            self.faded = 0.0;
            self.faded_previous = 0.0;
        } else if self.fade > 0.0 {
            (self.from_previous, self.from_time) =
                moved_on(self.from_time, step, self.from_length, self.from_hold);
            self.faded_previous = self.faded;
            if !self.paused {
                self.faded = (self.faded + dt).min(self.fade);
            }
        }
        self.ticked = true;
    }

    /// Where in its clip of `length` seconds a frame `alpha` of the way from the last tick
    /// to the next shows it: between the last two ticks' times, wrapped into the clip (its
    /// `time` before any tick).
    pub fn at(&self, alpha: f32, length: f32) -> f32 {
        between(
            self.previous,
            self.time,
            self.ticked,
            alpha,
            length,
            self.hold,
        )
    }

    /// Where the same frame shows the clip fading out, of `length` seconds.
    pub fn from_at(&self, alpha: f32, length: f32) -> f32 {
        between(
            self.from_previous,
            self.from_time,
            self.ticked,
            alpha,
            length,
            self.from_hold,
        )
    }

    /// How much of the pose `clip` makes the same frame, the clip fading out the rest: 1
    /// but while a fade goes on.
    pub fn weight(&self, alpha: f32) -> f32 {
        if self.fade <= 0.0 {
            return 1.0;
        }
        let faded = if self.ticked {
            self.faded_previous + (self.faded - self.faded_previous) * alpha.clamp(0.0, 1.0)
        } else {
            self.faded
        };
        (faded / self.fade).clamp(0.0, 1.0)
    }
}

/// `time` moved on by `step`, wrapped into `length` once that is known (held at its ends for
/// a clip that plays once, E12-3e), and the time before: `(previous, time)`.
fn moved_on(time: f32, step: f32, length: f32, hold: bool) -> (f32, f32) {
    if hold && length > 0.0 {
        return (time, (time + step).clamp(0.0, length));
    }
    wrapped(time, step, length)
}

/// `time` moved on by `step` and wrapped into `length` once that is known, and the time
/// before shifted alike: `(previous, time)`.
fn wrapped(time: f32, step: f32, length: f32) -> (f32, f32) {
    let (mut previous, mut time) = (time, time + step);
    if length > 0.0 && !(0.0..length).contains(&time) {
        let shift = (time / length).floor() * length;
        time -= shift;
        previous -= shift;
    }
    (previous, time)
}

/// A moment `alpha` of the way from `previous` to `time` (just `time` before any tick),
/// wrapped into `length` (0 for a length not known), or held within it (`hold`, E12-3e).
fn between(previous: f32, time: f32, ticked: bool, alpha: f32, length: f32, hold: bool) -> f32 {
    let t = if ticked {
        previous + (time - previous) * alpha.clamp(0.0, 1.0)
    } else {
        time
    };
    if length <= 0.0 {
        0.0
    } else if hold {
        t.clamp(0.0, length)
    } else {
        t.rem_euclid(length)
    }
}

/// A socket on a character (ADR 0124's E12-1c): an entity right under a skinned model that
/// follows one of its joints, as Godot's `BoneAttachment3D`. Each tick, before transforms
/// propagate, the player sets its `Transform` (in its parent's space, the character's) to
/// where the joint is in the character's pose then: what hangs under it (a sword in a hand)
/// follows the joint, its own transform its place in the joint's space. While the editor
/// edits, it follows the pose the editor shows. A joint the skeleton lacks leaves it where
/// it is.
///
/// ```ignore
/// let hand = world.spawn((JointAttachment::new("Hand.R"), Transform::IDENTITY));
/// world.set_parent(hand, Some(character))?;
/// world.set_parent(sword, Some(hand))?;
/// ```
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct JointAttachment {
    /// The joint, by its name in the skeleton.
    pub joint: String,
    /// The joint's index in the skeleton, as the player found it.
    #[reflect(skip)]
    pub found_joint: u16,
    /// Which of the player's rigs it was found in; 0 before.
    #[reflect(skip)]
    pub found_rig: u64,
}

reflect_component!(JointAttachment);

impl JointAttachment {
    /// Following joint `joint`.
    pub fn new(joint: &str) -> Self {
        Self {
            joint: joint.to_owned(),
            ..Self::default()
        }
    }
}

/// A clip's event a character's animator played past (ADR 0124's E12-3b): a moment its
/// model's `.meta` names in the clip, as a footstep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnimationEvent {
    /// The character: the entity with the [`Animator`].
    pub entity: Entity,
    /// The event's name ("footstep").
    pub name: Arc<str>,
    /// What else it tells ("left"); empty when nothing.
    pub data: Arc<str>,
}

/// The parameters of the animation graphs the player has read (ADR 0124's E12-5c), a world
/// resource: each graph file's parameters by slot, by name and whether each is a trigger.
/// Scripts and the inspector find a parameter's slot in an [`Animator`]'s `params` here.
#[derive(Debug, Default)]
pub struct GraphParams {
    graphs: std::collections::HashMap<engine_scene::AssetId, ParamTable>,
}

/// A graph's parameters by slot: each one's name and whether it is a trigger.
type ParamTable = Box<[(Box<str>, bool)]>;

impl GraphParams {
    /// Graph file `graph`'s parameters, by slot: each one's name and whether it is a
    /// trigger (in place of what was known of it).
    pub fn insert<'a>(
        &mut self,
        graph: engine_scene::AssetId,
        params: impl IntoIterator<Item = (&'a str, bool)>,
    ) {
        let params = params.into_iter().map(|(n, t)| (n.into(), t)).collect();
        self.graphs.insert(graph, params);
    }

    /// Whether graph file `graph`'s parameters are known (the player read the graph), none
    /// or some: what tells a name the graph lacks from a graph not read yet.
    pub fn knows(&self, graph: engine_scene::AssetId) -> bool {
        self.graphs.contains_key(&graph)
    }

    /// Parameter `name` of graph file `graph`: its slot and whether it is a trigger.
    pub fn slot(&self, graph: engine_scene::AssetId, name: &str) -> Option<(u16, bool)> {
        let params = self.graphs.get(&graph)?;
        let k = params.iter().position(|(n, _)| &**n == name)?;
        Some((k as u16, params[k].1))
    }

    /// Graph file `graph`'s parameters, by slot: each one's name and whether it is a
    /// trigger (none before the player read it).
    pub fn of(&self, graph: engine_scene::AssetId) -> &[(Box<str>, bool)] {
        self.graphs.get(&graph).map_or(&[], |p| p)
    }
}

/// The clips' events the last tick fired (ADR 0124's E12-3b), a resource the player keeps.
/// Each tick, once the animators move on, every character's clip fires the events its play
/// over the tick passed (with every loop, backwards too): during a cross-fade only the clip
/// that weighs more, so a fade never doubles a footstep. `Game::tick` reads them in the same
/// tick; a script on the character hears each as `on_animation_event(name, data)` in the
/// next tick's script stage. Characters fire in the order of their entities, each one's
/// events in the order played. The list keeps its room from tick to tick, so a steady game
/// allocates nothing for it.
///
/// ```ignore
/// fn tick(&mut self, world: &mut World, _dt: f32) {
///     let steps = world.resource::<AnimationEvents>().map_or(0, |e| e.fired().len());
/// }
/// ```
#[derive(Debug, Default)]
pub struct AnimationEvents {
    fired: Vec<AnimationEvent>,
    ticks: u64,
}

impl AnimationEvents {
    /// Room for 256 events a tick before it grows.
    pub fn new() -> Self {
        Self {
            fired: Vec::with_capacity(256),
            ticks: 0,
        }
    }

    /// The events the last tick fired.
    pub fn fired(&self) -> &[AnimationEvent] {
        &self.fired
    }

    /// How many ticks fired events into it so far (none or some): what tells a tick's events
    /// from the next one's, the same events or not.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Forgets the last tick's events, for a new tick's (the player's, as it fires them).
    pub fn clear(&mut self) {
        self.fired.clear();
        self.ticks += 1;
    }

    /// One more fired this tick.
    pub fn push(&mut self, event: AnimationEvent) {
        self.fired.push(event);
    }
}

/// Moves every [`Animator`] on each tick.
pub struct Animators {
    animators: QueryState<&'static mut Animator>,
}

impl core::fmt::Debug for Animators {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Animators").finish_non_exhaustive()
    }
}

impl Animators {
    /// Makes its query in `world`.
    pub fn new(world: &mut World) -> Self {
        Self {
            animators: QueryState::new(world),
        }
    }

    /// One tick of `dt` seconds for every animator ([`Animator::advance`]).
    pub fn advance(&mut self, world: &mut World, dt: f32) {
        engine_profiler::zone!("runtime::animators");
        self.animators
            .for_each(world, |animator| animator.advance(dt));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_animator_moves_on_each_tick_and_frames_fall_between_two_ticks() {
        let mut a = Animator::new("Walk");
        a.length = 1.0;
        // Before any tick: where it is.
        assert_eq!(a.at(0.5, 1.0), 0.0);
        a.advance(0.25);
        assert_eq!((a.previous, a.time), (0.0, 0.25));
        assert_eq!(a.at(0.5, 1.0), 0.125, "halfway between the two ticks");
        // Twice as fast: across the clip's end, the two times shifted alike.
        a.speed = 2.0;
        a.time = 0.9;
        a.advance(0.1);
        assert!((a.time - 0.1).abs() < 1e-6, "{}", a.time);
        assert!((a.previous + 0.1).abs() < 1e-6, "{}", a.previous);
        assert!((a.at(0.5, 1.0) - 0.0).abs() < 1e-6, "the frame at the seam");
        assert!((a.at(0.25, 1.0) - 0.95).abs() < 1e-6, "before the seam");
        // Paused: held, frames too.
        a.paused = true;
        a.advance(0.1);
        assert_eq!(a.previous, a.time);
        // Backwards, from the start: the end of the clip.
        a.paused = false;
        a.speed = -1.0;
        a.time = 0.05;
        a.advance(0.1);
        assert!((a.time - 0.95).abs() < 1e-6, "{}", a.time);
        assert!((a.previous - 1.05).abs() < 1e-6, "{}", a.previous);
        // A length not known yet: the time grows; a frame of an unknown clip shows its
        // start.
        let mut b = Animator::new("Run");
        b.advance(3.0);
        assert_eq!(b.time, 3.0);
        assert_eq!(b.at(1.0, 0.0), 0.0);
        assert!((b.at(1.0, 2.0) - 1.0).abs() < 1e-6);
    }

    /// E12-2b: a fade from one clip to another: both move on, the new one's weight grows
    /// over the fade and the frames between ticks, and the old one goes the tick after.
    #[test]
    fn a_fade_moves_both_clips_and_ends_after_its_length() {
        let mut a = Animator::new("Walk");
        a.length = 1.0;
        a.time = 0.5;
        a.advance(0.1);
        assert_eq!(a.weight(0.5), 1.0, "no fade");
        a.play("Run", 0.4);
        assert_eq!((a.clip.as_str(), a.from_clip.as_str()), ("Run", "Walk"));
        assert!((a.from_time - 0.6).abs() < 1e-6 && a.time == 0.0);
        assert_eq!(a.weight(1.0), 0.0, "all of the old one at first");
        a.length = 2.0;
        a.advance(0.1);
        assert!((a.from_time - 0.7).abs() < 1e-6 && (a.time - 0.1).abs() < 1e-6);
        assert!((a.weight(1.0) - 0.25).abs() < 1e-6);
        assert!(
            (a.weight(0.5) - 0.125).abs() < 1e-6,
            "between the two ticks"
        );
        assert!((a.from_at(0.5, 1.0) - 0.65).abs() < 1e-6);
        for _ in 0..3 {
            a.advance(0.1);
        }
        assert!((a.weight(1.0) - 1.0).abs() < 1e-6, "faded in");
        assert_eq!(a.from_clip, "Walk", "until the tick after");
        a.advance(0.1);
        assert_eq!((a.fade, a.from_clip.as_str()), (0.0, ""));
        assert_eq!(a.weight(0.0), 1.0);
        // A fade cut short by another: the clip fading out goes, the one playing fades.
        a.play("Walk", 0.2);
        a.advance(0.1);
        a.play("Idle", 0.2);
        assert_eq!((a.clip.as_str(), a.from_clip.as_str()), ("Idle", "Walk"));
        // At once: nothing fades.
        a.play("Run", 0.0);
        assert_eq!(
            (a.fade, a.from_clip.as_str(), a.weight(0.3)),
            (0.0, "", 1.0)
        );
    }

    /// ADR 0124's E12-3e: a clip that plays once stops at its end and holds it, frames
    /// between its last ticks showing its end, not its start; backwards it holds its start;
    /// a clip played after it loops again.
    #[test]
    fn a_clip_played_once_holds_its_end() {
        let mut a = Animator {
            length: 1.0,
            hold: true,
            ..Animator::new("Die")
        };
        for _ in 0..5 {
            a.advance(0.25);
        }
        assert_eq!(a.time, 1.0, "held at its end");
        assert_eq!(a.at(0.5, 1.0), 1.0, "its end, not its start");
        a.speed = -1.0;
        for _ in 0..6 {
            a.advance(0.25);
        }
        assert_eq!(a.time, 0.0, "backwards, held at its start");
        a.speed = 1.0;
        a.play("Walk", 0.5);
        assert!(
            !a.hold && a.from_hold,
            "the next clip loops; the fading one holds"
        );
        a.length = 1.0;
        for _ in 0..5 {
            a.advance(0.25);
        }
        assert_eq!(a.time, 0.25, "looped");
    }

    #[test]
    fn animators_move_on_together() {
        let mut world = World::new();
        let walking = world.spawn((Animator::new("Walk"),));
        let held = world.spawn((Animator {
            paused: true,
            ..Animator::new("Idle")
        },));
        let mut animators = Animators::new(&mut world);
        for _ in 0..3 {
            animators.advance(&mut world, 0.5);
        }
        assert_eq!(world.get::<Animator>(walking).unwrap().time, 1.5);
        assert_eq!(world.get::<Animator>(held).unwrap().time, 0.0);
    }

    /// ADR 0124's E12-5c: a graph's parameters set and fired by name through the slots the
    /// graph names them in; a trigger is not set, a number is not fired, an unknown name
    /// is neither.
    #[test]
    fn a_graphs_parameters_are_set_and_fired_by_name() {
        let graph = engine_scene::AssetId::new();
        let mut params = GraphParams::default();
        params.insert(graph, [("speed", false), ("jump", true)]);
        assert_eq!(params.slot(graph, "jump"), Some((1, true)));
        let mut a = Animator {
            graph: AssetRef::file(graph),
            ..Animator::default()
        };
        assert!(a.plays_graph());
        assert!(a.set(&params, "speed", 3.5));
        assert!(a.trigger(&params, "jump"));
        assert!(a.trigger(&params, "jump"));
        assert_eq!(&a.params[..2], &[3.5, 2.0]);
        assert!(!a.set(&params, "jump", 1.0), "a trigger is fired, not set");
        assert!(!a.trigger(&params, "speed"), "a number is set, not fired");
        assert!(!a.set(&params, "sped", 1.0));
        assert!(!Animator::default().plays_graph());
        assert_eq!(params.of(graph).len(), 2);
        // By slot, as the scripts' binding sets them: a slot past the last holds nothing.
        assert!(a.set_slot(0, 1.5) && a.fire_slot(1));
        assert_eq!(&a.params[..2], &[1.5, 3.0]);
        assert!(!a.set_slot(MAX_GRAPH_PARAMS as u16, 1.0));
        assert!(!a.fire_slot(u16::MAX));
        // A graph read with no parameters is known; one not read yet is not.
        let empty = engine_scene::AssetId::new();
        params.insert(empty, []);
        assert!(params.knows(graph) && params.knows(empty));
        assert!(!params.knows(engine_scene::AssetId::new()));
    }
}
