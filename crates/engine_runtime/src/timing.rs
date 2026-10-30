//! Timers and tweens on the game's clock (ADR 0104): saved with their entities, so after a
//! load they go on from where they were. [`Timing::advance`] moves them every tick (the
//! player calls it before `Game::tick`).

use engine_ecs::{
    ComponentId, Entity, QueryState, ReflectedKind, World, find_reflected, reflect_component,
};
use engine_reflect::{Primitive, Reflect, TypeKind};

/// A countdown on the game's clock: use it instead of `Instant`, which no save can hold.
#[derive(Debug, Clone, Copy, PartialEq, Default, Reflect)]
pub struct Timer {
    /// Seconds a run lasts.
    pub duration: f32,
    /// Seconds left of the current run.
    pub remaining: f32,
    /// Starts another run when one ends.
    pub repeat: bool,
    /// Stopped where it is.
    pub paused: bool,
    /// Runs that ended in the last tick (more than one when a run is shorter than a tick).
    #[reflect(skip)]
    pub ended: u32,
}

reflect_component!(Timer);

impl Timer {
    /// One run of `seconds`.
    pub fn new(seconds: f32) -> Self {
        Self {
            duration: seconds,
            remaining: seconds,
            ..Self::default()
        }
    }

    /// Runs of `seconds`, one after another.
    pub fn repeating(seconds: f32) -> Self {
        Self {
            repeat: true,
            ..Self::new(seconds)
        }
    }

    /// Moves it `dt` seconds on.
    pub fn tick(&mut self, dt: f32) {
        self.ended = 0;
        if self.paused || self.done() {
            return;
        }
        self.remaining -= dt;
        while self.remaining <= 0.0 {
            self.ended += 1;
            if self.repeat && self.duration > 0.0 {
                self.remaining += self.duration;
            } else {
                self.remaining = 0.0;
                break;
            }
        }
    }

    /// Whether a run ended in the last tick.
    pub fn just_ended(&self) -> bool {
        self.ended > 0
    }

    /// Whether a one-run timer has run out.
    pub fn done(&self) -> bool {
        !self.repeat && self.remaining <= 0.0
    }

    /// How far into the current run, `0..=1`.
    pub fn progress(&self) -> f32 {
        if self.duration <= 0.0 {
            1.0
        } else {
            (1.0 - self.remaining / self.duration).clamp(0.0, 1.0)
        }
    }

    /// Starts the run again.
    pub fn restart(&mut self) {
        self.remaining = self.duration;
        self.ended = 0;
    }
}

/// How a tween eases from its start to its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum Ease {
    /// Even speed.
    #[default]
    Linear = 0,
    /// Slow start.
    QuadIn = 1,
    /// Slow end.
    QuadOut = 2,
    /// Slow start and end.
    QuadInOut = 3,
    /// Slower start.
    CubicIn = 4,
    /// Slower end.
    CubicOut = 5,
    /// Slower start and end.
    CubicInOut = 6,
    /// A sine's start.
    SineIn = 7,
    /// A sine's end.
    SineOut = 8,
    /// A sine's start and end.
    SineInOut = 9,
    /// Backs up, then goes.
    BackIn = 10,
    /// Overshoots, then settles.
    BackOut = 11,
    /// Springs past the end and back.
    ElasticOut = 12,
    /// Bounces at the end.
    BounceOut = 13,
}

impl Ease {
    /// The eased fraction of `t` (`0..=1`).
    pub fn apply(self, t: f32) -> f32 {
        use core::f32::consts::PI;
        let t = t.clamp(0.0, 1.0);
        const BACK: f32 = 1.701_58;
        match self {
            Self::Linear => t,
            Self::QuadIn => t * t,
            Self::QuadOut => 1.0 - (1.0 - t) * (1.0 - t),
            Self::QuadInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
                }
            }
            Self::CubicIn => t * t * t,
            Self::CubicOut => 1.0 - (1.0 - t).powi(3),
            Self::CubicInOut => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
            Self::SineIn => 1.0 - (t * PI / 2.0).cos(),
            Self::SineOut => (t * PI / 2.0).sin(),
            Self::SineInOut => -((PI * t).cos() - 1.0) / 2.0,
            Self::BackIn => (BACK + 1.0) * t * t * t - BACK * t * t,
            Self::BackOut => {
                let u = t - 1.0;
                1.0 + (BACK + 1.0) * u * u * u + BACK * u * u
            }
            Self::ElasticOut => {
                if t == 0.0 || t == 1.0 {
                    t
                } else {
                    2f32.powf(-10.0 * t) * ((t * 10.0 - 0.75) * (2.0 * PI / 3.0)).sin() + 1.0
                }
            }
            Self::BounceOut => {
                let (n, d) = (7.5625, 2.75);
                if t < 1.0 / d {
                    n * t * t
                } else if t < 2.0 / d {
                    let u = t - 1.5 / d;
                    n * u * u + 0.75
                } else if t < 2.5 / d {
                    let u = t - 2.25 / d;
                    n * u * u + 0.9375
                } else {
                    let u = t - 2.625 / d;
                    n * u * u + 0.984_375
                }
            }
        }
    }
}

/// What a tween does at its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
#[repr(u8)]
pub enum Repeat {
    /// Stops there.
    #[default]
    Once = 0,
    /// Starts over.
    Loop = 1,
    /// Goes back, then forth again.
    PingPong = 2,
}

/// Where a tween writes, found once from its target's name.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Resolved {
    component: Option<ComponentId>,
    offset: u32,
    lanes: u8,
    /// Looked for: a target that names nothing is not looked for again every tick.
    tried: bool,
}

/// Moves a field of a component of its entity from `from` to `to` over `duration` seconds:
/// a number or a vector of up to four (`from[..lanes]`).
#[derive(Debug, Clone, PartialEq, Default, Reflect)]
pub struct Tween {
    /// `Component.path`: `Transform.translation`, `Transform.scale.y`, `PointLight.color`.
    pub target: String,
    /// The start value.
    pub from: [f32; 4],
    /// The end value.
    pub to: [f32; 4],
    /// Seconds from the start to the end.
    pub duration: f32,
    /// Seconds since the start (saved: it goes on after a load).
    pub elapsed: f32,
    /// How it eases.
    pub ease: Ease,
    /// What it does at its end.
    pub repeat: Repeat,
    /// Stopped where it is.
    pub paused: bool,
    #[reflect(opaque)]
    resolved: Resolved,
}

reflect_component!(Tween);

impl Tween {
    /// A tween of `target` from `from` to `to` (as many numbers as the field holds).
    pub fn new(target: &str, from: &[f32], to: &[f32], duration: f32) -> Self {
        let lanes = |v: &[f32]| core::array::from_fn(|i| v.get(i).copied().unwrap_or(0.0));
        Self {
            target: target.to_owned(),
            from: lanes(from),
            to: lanes(to),
            duration,
            ..Self::default()
        }
    }

    /// With an easing.
    pub fn eased(mut self, ease: Ease) -> Self {
        self.ease = ease;
        self
    }

    /// Looping or back and forth.
    pub fn repeating(mut self, repeat: Repeat) -> Self {
        self.repeat = repeat;
        self
    }

    /// Whether a tween played once has reached its end.
    pub fn done(&self) -> bool {
        self.repeat == Repeat::Once && self.elapsed >= self.duration
    }

    /// The fraction of the way from `from` to `to` now, eased.
    pub fn fraction(&self) -> f32 {
        if self.duration <= 0.0 {
            return self.ease.apply(1.0);
        }
        let runs = self.elapsed / self.duration;
        let t = match self.repeat {
            Repeat::Once => runs.min(1.0),
            Repeat::Loop => runs.fract(),
            Repeat::PingPong => {
                let r = runs % 2.0;
                if r > 1.0 { 2.0 - r } else { r }
            }
        };
        self.ease.apply(t)
    }
}

/// A field of a reflected component that tweens and clips write (ADR 0124's E12-4a): where
/// it lies in its component and how many `f32`s it holds, found once from its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldRef {
    component: ComponentId,
    offset: u32,
    lanes: u8,
    rotation: bool,
}

impl FieldRef {
    /// The field `target` names, `Component.path` (`Transform.translation`,
    /// `Transform.scale.y`, `PointLight.color`): the component by its reflected name, the
    /// field by its path, a number or a vector of up to four `f32`s. None for anything else.
    pub fn find(world: &mut World, target: &str) -> Option<Self> {
        let (name, path) = target.split_once('.').unwrap_or((target, ""));
        let reflected = find_reflected(name)?;
        let ReflectedKind::Component { id, .. } = reflected.kind else {
            return None;
        };
        let (offset, ty, _) = reflected.info.path(path)?;
        let lanes = match ty.kind {
            TypeKind::Primitive(Primitive::F32) => 1,
            TypeKind::Struct(fields)
                if (1..=4).contains(&fields.len())
                    && fields
                        .iter()
                        .all(|f| matches!(f.ty.kind, TypeKind::Primitive(Primitive::F32))) =>
            {
                fields.len() as u8
            }
            TypeKind::Array { element, len }
                if (1..=4).contains(&len)
                    && matches!(element.kind, TypeKind::Primitive(Primitive::F32)) =>
            {
                len as u8
            }
            _ => return None,
        };
        Some(Self {
            component: id(world),
            offset: offset as u32,
            lanes,
            rotation: ty.name == "Quat" && lanes == 4,
        })
    }

    /// How many `f32`s it holds (1 to 4).
    pub fn lanes(&self) -> usize {
        usize::from(self.lanes)
    }

    /// It is a rotation (a `Quat`: x, y, z, w).
    pub fn is_rotation(&self) -> bool {
        self.rotation
    }

    /// Its value in `entity`'s component into `out[..lanes]`; false when the entity lacks
    /// the component.
    pub fn read(&self, world: &World, entity: Entity, out: &mut [f32; 4]) -> bool {
        let Some(ptr) = world.get_raw(entity, self.component) else {
            return false;
        };
        // SAFETY: the offset and lanes came from the component's description: that many
        // `f32`s lie there, inside the entity's live value.
        unsafe {
            let at = ptr.as_ptr().add(self.offset as usize).cast::<f32>();
            for (i, v) in out.iter_mut().take(self.lanes()).enumerate() {
                *v = at.add(i).read_unaligned();
            }
        }
        true
    }

    /// `values[..lanes]` written into `entity`'s component (marked changed); false when the
    /// entity lacks the component.
    pub fn write(&self, world: &mut World, entity: Entity, values: &[f32]) -> bool {
        let Some(ptr) = world.get_raw_mut(entity, self.component) else {
            return false;
        };
        // SAFETY: as in `read`.
        unsafe {
            let at = ptr.as_ptr().add(self.offset as usize).cast::<f32>();
            for (i, v) in values.iter().take(self.lanes()).enumerate() {
                at.add(i).write_unaligned(*v);
            }
        }
        true
    }
}

/// Finds where a tween writes ([`FieldRef::find`]).
fn resolve(world: &mut World, target: &str) -> Resolved {
    FieldRef::find(world, target).map_or(Resolved::default(), |f| Resolved {
        component: Some(f.component),
        offset: f.offset,
        lanes: f.lanes,
        tried: true,
    })
}

/// Moves the world's timers and tweens on; keeps its queries and buffers (no allocation
/// in steady state).
pub struct Timing {
    timers: QueryState<&'static mut Timer>,
    tweens: QueryState<(Entity, &'static mut Tween)>,
    writes: Vec<(Entity, Resolved, [f32; 4])>,
}

impl Timing {
    /// Makes its queries in `world`.
    pub fn new(world: &mut World) -> Self {
        Self {
            timers: QueryState::new(world),
            tweens: QueryState::new(world),
            writes: Vec::new(),
        }
    }

    /// One tick of `dt` seconds: every timer and tween moved on, each tween's field
    /// written (after every tween is moved, so a tween may move another's entity).
    pub fn advance(&mut self, world: &mut World, dt: f32) {
        engine_profiler::zone!("runtime::timing");
        self.timers.for_each(world, |timer| timer.tick(dt));
        self.writes.clear();
        let mut unresolved: Vec<Entity> = Vec::new();
        let writes = &mut self.writes;
        self.tweens.for_each(world, |(entity, tween)| {
            if tween.resolved.component.is_none() {
                if !tween.resolved.tried {
                    unresolved.push(entity);
                }
                return;
            }
            if !tween.paused && !tween.done() {
                tween.elapsed += dt;
            }
            let f = tween.fraction();
            let value = core::array::from_fn(|i| tween.from[i] + (tween.to[i] - tween.from[i]) * f);
            writes.push((entity, tween.resolved, value));
        });
        // Targets named since the last tick (or loaded): found once.
        for entity in unresolved {
            let Some(target) = world.get::<Tween>(entity).map(|t| t.target.clone()) else {
                continue;
            };
            let resolved = Resolved {
                tried: true,
                ..resolve(world, &target)
            };
            if resolved.component.is_none() {
                log::warn!("tween: `{target}` names no f32 field of a reflected component");
            }
            if let Some(tween) = world.get_mut::<Tween>(entity) {
                tween.resolved = resolved;
            }
        }
        for &(entity, resolved, value) in &self.writes {
            let Some(component) = resolved.component else {
                continue;
            };
            if let Some(ptr) = world.get_raw_mut(entity, component) {
                // SAFETY: the offset and lanes came from the component's description: that
                // many `f32`s lie there, inside the entity's live value.
                unsafe {
                    let at = ptr.as_ptr().add(resolved.offset as usize).cast::<f32>();
                    for (i, v) in value.iter().take(usize::from(resolved.lanes)).enumerate() {
                        at.add(i).write_unaligned(*v);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use super::*;
    use engine_ecs::Transform;

    #[test]
    fn timers_end_and_repeat_on_the_game_clock() {
        let mut t = Timer::new(1.0);
        t.tick(0.6);
        assert!(!t.just_ended() && !t.done());
        t.tick(0.6);
        assert!(t.just_ended() && t.done());
        t.tick(0.6);
        assert!(!t.just_ended(), "a finished run ends once");
        let mut r = Timer::repeating(0.25);
        r.tick(0.6);
        assert_eq!(r.ended, 2);
        assert!((r.remaining - 0.15).abs() < 1e-5);
    }

    #[test]
    fn eases_start_and_end_where_they_should() {
        for ease in [
            Ease::Linear,
            Ease::QuadIn,
            Ease::QuadOut,
            Ease::QuadInOut,
            Ease::CubicIn,
            Ease::CubicOut,
            Ease::CubicInOut,
            Ease::SineIn,
            Ease::SineOut,
            Ease::SineInOut,
            Ease::BackIn,
            Ease::BackOut,
            Ease::ElasticOut,
            Ease::BounceOut,
        ] {
            assert!(ease.apply(0.0).abs() < 1e-5, "{ease:?}");
            assert!((ease.apply(1.0) - 1.0).abs() < 1e-5, "{ease:?}");
        }
        assert_eq!(Ease::QuadIn.apply(0.5), 0.25);
    }

    #[test]
    fn tweens_move_a_named_field_and_go_on_from_where_they_were() {
        let mut world = World::new();
        let e = world.spawn((
            Transform::default(),
            Tween::new(
                "Transform.translation",
                &[0.0, 0.0, 0.0],
                &[10.0, 0.0, -4.0],
                2.0,
            ),
        ));
        let mut timing = Timing::new(&mut world);
        for _ in 0..5 {
            timing.advance(&mut world, 0.2);
        }
        // The first tick found the field; four moved it.
        let t = world.get::<Transform>(e).unwrap().translation;
        assert!((t.x - 4.0).abs() < 1e-4 && (t.z + 1.6).abs() < 1e-4, "{t}");
        // Back and forth: past the end it comes back.
        let back = world.spawn((
            Transform::default(),
            Tween::new("Transform.scale.y", &[1.0], &[3.0], 1.0).repeating(Repeat::PingPong),
        ));
        world.get_mut::<Tween>(back).unwrap().elapsed = 1.5;
        timing.advance(&mut world, 0.0);
        timing.advance(&mut world, 0.0);
        assert!((world.get::<Transform>(back).unwrap().scale.y - 2.0).abs() < 1e-4);
    }
}
