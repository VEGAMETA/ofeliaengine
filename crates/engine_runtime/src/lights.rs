//! Lights as components (ADR 0099 step 3): point and spot lights and the sun on entities,
//! mirrored into the renderer's light list after every tick.

use engine_core::math::{Vec3, Vec3A};
use engine_ecs::{
    Changed, Entity, GlobalTransform, Or, QueryState, World, bytemuck, pod_component,
};
use engine_render::forward::{DirectionalLight, LocalLight, Spot};

/// A point light at its entity's position.
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct PointLight {
    /// Color × intensity: its intensity per steradian (candela-like).
    pub color: Vec3,
    /// Where its influence ends.
    pub range: f32,
    /// Nonzero: it casts shadows (ADR 0071).
    pub shadow: u32,
}

pod_component!(PointLight);

impl PointLight {
    /// A light of `color` (× intensity) reaching `range`, without shadows.
    pub fn new(color: Vec3, range: f32) -> Self {
        Self {
            color,
            range,
            shadow: 0,
        }
    }

    /// This light, casting shadows.
    pub fn with_shadow(mut self) -> Self {
        self.shadow = 1;
        self
    }
}

/// A spot light at its entity's position, shining down its entity's −Z axis.
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct SpotLight {
    /// Color × intensity: its intensity per steradian (candela-like).
    pub color: Vec3,
    /// Where its influence ends.
    pub range: f32,
    /// Half angle of full intensity, radians.
    pub inner: f32,
    /// Half angle where it ends, radians (at least `inner`).
    pub outer: f32,
    /// Nonzero: it casts shadows (ADR 0071).
    pub shadow: u32,
}

pod_component!(SpotLight);

impl SpotLight {
    /// A cone of `color` (× intensity) reaching `range`, at full intensity within
    /// `inner` radians of its axis and none beyond `outer`, without shadows.
    pub fn new(color: Vec3, range: f32, inner: f32, outer: f32) -> Self {
        Self {
            color,
            range,
            inner,
            outer,
            shadow: 0,
        }
    }

    /// This light, casting shadows.
    pub fn with_shadow(mut self) -> Self {
        self.shadow = 1;
        self
    }
}

/// The sun: light from far away down its entity's −Z axis, with a constant ambient term.
/// One sun lights the world: the first one found.
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Sun {
    /// Color × intensity.
    pub color: Vec3,
    /// The ambient term, multiplied by the surfaces' base color.
    pub ambient: Vec3,
}

pod_component!(Sun);

type Points = QueryState<
    (Entity, &'static GlobalTransform, &'static PointLight),
    Or<(Changed<GlobalTransform>, Changed<PointLight>)>,
>;
type Spots = QueryState<
    (Entity, &'static GlobalTransform, &'static SpotLight),
    Or<(Changed<GlobalTransform>, Changed<SpotLight>)>,
>;
type Suns = QueryState<(&'static GlobalTransform, &'static Sun)>;

/// A slot no light uses: shades nothing, casts nothing.
const DARK: LocalLight = LocalLight {
    position: Vec3::ZERO,
    range: 0.0,
    color: Vec3::ZERO,
    spot: None,
    shadow: false,
};

/// The entity's −Z axis in the world: where its lights shine.
fn forward(global: &GlobalTransform) -> Vec3 {
    Vec3::from(global.0.matrix3 * Vec3A::NEG_Z).normalize_or(Vec3::NEG_Z)
}

/// Mirrors the [`PointLight`] and [`SpotLight`] entities into a light list for the
/// renderer, and finds the [`Sun`].
///
/// A light keeps its slot in the list while it lives: the renderer knows a light's cached
/// shadow by its index (ADR 0071). A removed light leaves a dark slot, which the next new
/// light takes. Like [`RenderExtract`](crate::RenderExtract) it visits only the chunks
/// where a light or its transform changed, and takes removals from the world's removal
/// logs; it allocates only when the list grows.
///
/// A light whose entity loses its `GlobalTransform` alone stays where it was.
pub struct LightExtract {
    points: Points,
    spots: Spots,
    suns: Suns,
    lights: Vec<LocalLight>,
    /// By entity index: the entity and its slot.
    slots: Vec<Option<(Entity, u32)>>,
    free: Vec<u32>,
    removed: Vec<Entity>,
    sun: Option<DirectionalLight>,
    epoch: Option<u64>,
}

impl core::fmt::Debug for LightExtract {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LightExtract")
            .field("lights", &self.lights.len())
            .field("free", &self.free.len())
            .field("sun", &self.sun)
            .finish_non_exhaustive()
    }
}

impl LightExtract {
    /// State for `world`; starts the world's removal logs of the light components.
    pub fn new(world: &mut World) -> Self {
        world.track_removals::<PointLight>();
        world.track_removals::<SpotLight>();
        Self {
            points: QueryState::new(world),
            spots: QueryState::new(world),
            suns: QueryState::new(world),
            lights: Vec::new(),
            slots: Vec::new(),
            free: Vec::new(),
            removed: Vec::new(),
            sun: None,
            epoch: None,
        }
    }

    /// The light list: every light's slot, dark slots included.
    pub fn lights(&self) -> &[LocalLight] {
        &self.lights
    }

    /// The sun, if an entity has one.
    pub fn sun(&self) -> Option<DirectionalLight> {
        self.sun
    }

    /// The slot of `entity`'s light.
    pub fn slot(&self, entity: Entity) -> Option<u32> {
        self.slots
            .get(entity.index() as usize)
            .copied()
            .flatten()
            .filter(|(e, _)| *e == entity)
            .map(|(_, slot)| slot)
    }

    /// Mirrors the changes since the previous call. Call it after each tick, after
    /// transform propagation.
    pub fn sync(&mut self, world: &mut World) {
        engine_profiler::zone!("runtime::LightExtract::sync");
        if self.epoch != Some(world.resync_epoch()) {
            // First sync, or entities vanished without logs: start over.
            self.lights.clear();
            self.slots.clear();
            self.free.clear();
            self.points = QueryState::new(world);
            self.spots = QueryState::new(world);
            world.drain_removed::<PointLight>(|_| {});
            world.drain_removed::<SpotLight>(|_| {});
            self.epoch = Some(world.resync_epoch());
        }
        // Removals: a slot is freed once its entity has neither kind of light.
        let removed = &mut self.removed;
        world.drain_removed::<PointLight>(|e| removed.push(e));
        world.drain_removed::<SpotLight>(|e| removed.push(e));
        for k in 0..self.removed.len() {
            let entity = self.removed[k];
            let lit = world.get::<PointLight>(entity).is_some()
                || world.get::<SpotLight>(entity).is_some();
            if !lit && let Some(slot) = self.slot(entity) {
                self.slots[entity.index() as usize] = None;
                self.lights[slot as usize] = DARK;
                self.free.push(slot);
            }
        }
        self.removed.clear();
        // Additions and changes.
        let (lights, slots, free) = (&mut self.lights, &mut self.slots, &mut self.free);
        let world = &*world;
        self.points
            .for_each_readonly(world, |(entity, global, light)| {
                let slot = slot_for(lights, slots, free, entity);
                lights[slot] = LocalLight {
                    position: Vec3::from(global.0.translation),
                    range: light.range,
                    color: light.color,
                    spot: None,
                    shadow: light.shadow != 0,
                };
            });
        self.spots
            .for_each_readonly(world, |(entity, global, light)| {
                let slot = slot_for(lights, slots, free, entity);
                lights[slot] = LocalLight {
                    position: Vec3::from(global.0.translation),
                    range: light.range,
                    color: light.color,
                    spot: Some(Spot {
                        direction: forward(global),
                        inner: light.inner,
                        outer: light.outer.max(light.inner),
                    }),
                    shadow: light.shadow != 0,
                };
            });
        let mut sun = None;
        self.suns.for_each_readonly(world, |(global, light)| {
            if sun.is_none() {
                sun = Some(DirectionalLight {
                    direction: forward(global),
                    color: light.color,
                    ambient: light.ambient,
                });
            }
        });
        self.sun = sun;
    }
}

/// `entity`'s slot, or a free one (a dark slot first, then a new one at the end).
fn slot_for(
    lights: &mut Vec<LocalLight>,
    slots: &mut Vec<Option<(Entity, u32)>>,
    free: &mut Vec<u32>,
    entity: Entity,
) -> usize {
    let index = entity.index() as usize;
    if let Some(Some((e, slot))) = slots.get(index)
        && *e == entity
    {
        return *slot as usize;
    }
    let slot = free.pop().unwrap_or_else(|| {
        lights.push(DARK);
        (lights.len() - 1) as u32
    });
    if slots.len() <= index {
        slots.resize(index + 1, None);
    }
    slots[index] = Some((entity, slot));
    slot as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_ecs::Transform;

    fn light_world() -> (World, LightExtract) {
        let mut world = World::new();
        let extract = LightExtract::new(&mut world);
        (world, extract)
    }

    #[test]
    fn lights_keep_their_slots_and_removed_ones_go_dark() {
        let (mut world, mut extract) = light_world();
        let a = world.spawn((
            GlobalTransform(Transform::from_xyz(1.0, 2.0, 3.0).to_affine()),
            PointLight::new(Vec3::splat(10.0), 5.0).with_shadow(),
        ));
        let b = world.spawn((
            GlobalTransform(Transform::from_xyz(0.0, 4.0, 0.0).to_affine()),
            SpotLight::new(Vec3::ONE, 8.0, 0.3, 0.5),
        ));
        extract.sync(&mut world);
        assert_eq!(extract.lights().len(), 2);
        let (sa, sb) = (extract.slot(a).unwrap(), extract.slot(b).unwrap());
        let light = extract.lights()[sa as usize];
        assert_eq!(light.position, Vec3::new(1.0, 2.0, 3.0));
        assert!(light.shadow && light.spot.is_none());
        // The spot looks down −Z by default.
        let spot = extract.lights()[sb as usize].spot.unwrap();
        assert!((spot.direction - Vec3::NEG_Z).length() < 1e-6);

        // A move changes the light in its slot.
        world.get_mut::<GlobalTransform>(a).unwrap().0 =
            Transform::from_xyz(5.0, 2.0, 3.0).to_affine();
        extract.sync(&mut world);
        assert_eq!(extract.slot(a), Some(sa));
        assert_eq!(extract.lights()[sa as usize].position.x, 5.0);

        // A despawned light leaves a dark slot; the next light takes it.
        world.despawn(a);
        extract.sync(&mut world);
        assert_eq!(extract.lights()[sa as usize], DARK);
        assert_eq!(extract.slot(b), Some(sb));
        let c = world.spawn((GlobalTransform::IDENTITY, PointLight::new(Vec3::ONE, 1.0)));
        extract.sync(&mut world);
        assert_eq!(extract.slot(c), Some(sa));
        assert_eq!(extract.lights().len(), 2);

        // A light changing kind keeps its slot.
        world.remove::<PointLight>(c);
        world.insert(c, SpotLight::new(Vec3::ONE, 2.0, 0.1, 0.2));
        extract.sync(&mut world);
        assert_eq!(extract.slot(c), Some(sa));
        assert!(extract.lights()[sa as usize].spot.is_some());
    }

    #[test]
    fn the_first_sun_lights_the_world() {
        let (mut world, mut extract) = light_world();
        extract.sync(&mut world);
        assert!(extract.sun().is_none());
        let down = Transform::IDENTITY.with_rotation(engine_core::math::Quat::from_rotation_x(
            -core::f32::consts::FRAC_PI_2,
        ));
        world.spawn((
            GlobalTransform(down.to_affine()),
            Sun {
                color: Vec3::splat(3.0),
                ambient: Vec3::splat(0.1),
            },
        ));
        extract.sync(&mut world);
        let sun = extract.sun().unwrap();
        assert!((sun.direction - Vec3::NEG_Y).length() < 1e-5, "{sun:?}");
        assert_eq!(sun.color, Vec3::splat(3.0));
    }
}
