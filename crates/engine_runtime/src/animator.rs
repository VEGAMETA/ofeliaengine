//! What a character plays (ADR 0124's E12-2): the [`Animator`], saved with its entity, and
//! [`Animators::advance`], which moves every animator on each tick (the player calls it
//! after the scripts' tick and before `Game::tick`). Each rendered frame then poses the
//! character at the frame's moment between the last two ticks ([`Animator::at`]). A
//! [`JointAttachment`] under a character follows one of its joints (E12-1c).

use engine_ecs::{QueryState, World, reflect_component};
use engine_reflect::Reflect;

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
    /// The clip, by its name in the model's file; empty, or a name it has not: its first.
    #[reflect(hint = "clip")]
    pub clip: String,
    /// How fast it plays: 1 at its own pace, 0.5 half as fast, negative backwards.
    pub speed: f32,
    /// Held where it is.
    pub paused: bool,
    /// Seconds into the clip. The player moves it each tick (saves keep it, so a load goes
    /// on from there); setting it moves the clip there.
    pub time: f32,
    /// The clip's length, as the player last found it (0 before): where `time` wraps.
    #[reflect(skip)]
    pub length: f32,
    /// `time` at the tick before the last, in step with `time` across a wrap.
    #[reflect(skip)]
    pub previous: f32,
    /// A tick set `previous` since the animator was made or loaded.
    #[reflect(skip)]
    pub ticked: bool,
}

reflect_component!(Animator);

impl Default for Animator {
    fn default() -> Self {
        Self {
            clip: String::new(),
            speed: 1.0,
            paused: false,
            time: 0.0,
            length: 0.0,
            previous: 0.0,
            ticked: false,
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

    /// One tick of `dt` seconds: `time` moves on by `dt` times the speed (held while
    /// paused), wrapped into the clip's length once it is known; the time before the tick
    /// is kept, shifted alike, for the frames that fall between this tick and the next.
    pub fn advance(&mut self, dt: f32) {
        let mut previous = self.time;
        let mut time = if self.paused {
            self.time
        } else {
            self.time + dt * self.speed
        };
        if self.length > 0.0 && !(0.0..self.length).contains(&time) {
            let shift = (time / self.length).floor() * self.length;
            time -= shift;
            previous -= shift;
        }
        self.previous = previous;
        self.time = time;
        self.ticked = true;
    }

    /// Where in its clip of `length` seconds a frame `alpha` of the way from the last tick
    /// to the next shows it: between the last two ticks' times, wrapped into the clip (its
    /// `time` before any tick).
    pub fn at(&self, alpha: f32, length: f32) -> f32 {
        let t = if self.ticked {
            self.previous + (self.time - self.previous) * alpha.clamp(0.0, 1.0)
        } else {
            self.time
        };
        if length > 0.0 {
            t.rem_euclid(length)
        } else {
            0.0
        }
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
}
