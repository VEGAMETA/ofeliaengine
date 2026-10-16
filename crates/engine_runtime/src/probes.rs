//! Reflection probes and the global illumination volume as components (ADR 0099 step 4):
//! [`ReflectionProbe`] entities mirrored into the renderer's probe set by a
//! [`ProbeExtract`], and the [`GiVolume`] whose box a volume of GI probes covers
//! ([`GiExtract`]).

use engine_core::math::Vec3;
use engine_ecs::{
    Changed, Entity, GlobalTransform, Or, QueryState, World, bytemuck, reflect_component,
};
use engine_reflect::Reflect;
use engine_render::gi::ProbeGrid;
use engine_render::reflections::{self, ProbeId, ProbeRefresh, ReflectionProbes};

/// A reflection probe for the box around its entity (a room, a corridor; ADR 0073):
/// captured from the entity's position, it reflects inside the box, fading in over
/// `blend` beyond its faces (neighbouring rooms' probes blend across their shared walls),
/// and corrects parallax against the box's faces. The box turns and scales with the
/// entity. A [`ProbeExtract`] mirrors these entities into the renderer's probe set.
#[derive(Debug, Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
pub struct ReflectionProbe {
    /// Half the box's size along the entity's axes: usually the room's walls.
    pub half_extents: Vec3,
    /// Multiplier of its radiance.
    pub intensity: f32,
    /// The distance over which it fades in beyond the box's faces (0: a tenth of the
    /// box's smallest half extent).
    pub blend: f32,
    /// Nonzero: captured again only when the probe changes, not when the scene inside its
    /// box does (`ProbeRefresh::Manual`).
    pub manual: u32,
}

reflect_component!(pod ReflectionProbe);

impl ReflectionProbe {
    /// A probe for the box of `half_extents` around its entity, at full intensity.
    pub fn new(half_extents: Vec3) -> Self {
        Self {
            half_extents,
            intensity: 1.0,
            blend: 0.0,
            manual: 0,
        }
    }

    /// The renderer's probe for an entity at `global`.
    pub fn at(&self, global: &GlobalTransform) -> reflections::ReflectionProbe {
        let (scale, rotation, center) = global.0.to_scale_rotation_translation();
        let half = (self.half_extents * scale.abs()).max(Vec3::splat(1e-3));
        let mut probe = reflections::ReflectionProbe::room(center, half);
        if self.blend > 0.0 {
            probe.blend_distance = self.blend;
            probe.influence.half_extents = half + self.blend;
        }
        probe.influence.rotation = rotation;
        probe.parallax.rotation = rotation;
        probe.intensity = self.intensity;
        if self.manual != 0 {
            probe.refresh = ProbeRefresh::Manual;
        }
        probe
    }
}

/// Where a [`ProbeExtract`] mirrors probes: the renderer's [`ReflectionProbes`], or a
/// test's own.
pub trait ProbeSink {
    /// Handle of a probe.
    type Id: Copy + Eq;
    /// Adds a probe; `None` when there is no room.
    fn add(&mut self, probe: reflections::ReflectionProbe) -> Option<Self::Id>;
    /// Changes one (unchanged: nothing).
    fn set(&mut self, id: Self::Id, probe: reflections::ReflectionProbe);
    /// Removes one.
    fn remove(&mut self, id: Self::Id);
}

impl ProbeSink for ReflectionProbes {
    type Id = ProbeId;
    fn add(&mut self, probe: reflections::ReflectionProbe) -> Option<ProbeId> {
        ReflectionProbes::add(self, probe)
    }
    fn set(&mut self, id: ProbeId, probe: reflections::ReflectionProbe) {
        ReflectionProbes::set(self, id, probe);
    }
    fn remove(&mut self, id: ProbeId) {
        ReflectionProbes::remove(self, id);
    }
}

type Probes = QueryState<
    (Entity, &'static GlobalTransform, &'static ReflectionProbe),
    Or<(Changed<GlobalTransform>, Changed<ReflectionProbe>)>,
>;

/// Mirrors the [`ReflectionProbe`] entities into a probe set ([`ProbeSink`]).
///
/// Like [`LightExtract`](crate::LightExtract) it visits only the chunks where a probe or
/// its transform changed, and takes removals from the world's removal log; a probe that
/// moved or changed is captured again (`ReflectionProbes::set`). A probe the set has no
/// room for waits: it is placed once another one goes. It allocates only when its table
/// of entities grows.
pub struct ProbeExtract<Id = ProbeId> {
    probes: Probes,
    /// By entity index: the entity and its probe.
    ids: Vec<Option<(Entity, Id)>>,
    placed: usize,
    removed: Vec<Entity>,
    /// A probe found no room in the set.
    unplaced: bool,
    epoch: Option<u64>,
}

impl<Id> core::fmt::Debug for ProbeExtract<Id> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ProbeExtract")
            .field("placed", &self.placed)
            .field("unplaced", &self.unplaced)
            .finish_non_exhaustive()
    }
}

impl<Id: Copy + Eq> ProbeExtract<Id> {
    /// State for `world`; starts the world's removal log of [`ReflectionProbe`] (drain it
    /// with [`Self::sync`] every frame).
    pub fn new(world: &mut World) -> Self {
        world.track_removals::<ReflectionProbe>();
        Self {
            probes: QueryState::new(world),
            ids: Vec::new(),
            placed: 0,
            removed: Vec::new(),
            unplaced: false,
            epoch: None,
        }
    }

    /// Probes in the set.
    pub fn len(&self) -> usize {
        self.placed
    }

    /// Whether the set holds none of the entities' probes.
    pub fn is_empty(&self) -> bool {
        self.placed == 0
    }

    /// Whether a probe waits for room in the set.
    pub fn waiting(&self) -> bool {
        self.unplaced
    }

    /// The probe of `entity`, when the set holds it.
    pub fn probe(&self, entity: Entity) -> Option<Id> {
        self.ids
            .get(entity.index() as usize)
            .copied()
            .flatten()
            .filter(|(e, _)| *e == entity)
            .map(|(_, id)| id)
    }

    /// Mirrors the changes since the previous call into `sink` (the same sink every
    /// time). Call it once a frame, after the frame's ticks.
    pub fn sync<S: ProbeSink<Id = Id>>(&mut self, world: &mut World, sink: &mut S) {
        engine_profiler::zone!("runtime::ProbeExtract::sync");
        if self.epoch != Some(world.resync_epoch()) {
            // First sync, or entities vanished without logs: start over.
            for (_, id) in self.ids.iter_mut().filter_map(Option::take) {
                sink.remove(id);
            }
            self.placed = 0;
            self.probes = QueryState::new(world);
            world.drain_removed::<ReflectionProbe>(|_| {});
            self.epoch = Some(world.resync_epoch());
        }
        // Removals: an entity that has its probe again (removed and re-added) keeps it.
        let removed = &mut self.removed;
        world.drain_removed::<ReflectionProbe>(|e| removed.push(e));
        let mut freed = false;
        for k in 0..self.removed.len() {
            let entity = self.removed[k];
            if world.get::<ReflectionProbe>(entity).is_none()
                && let Some(id) = self.probe(entity)
            {
                self.ids[entity.index() as usize] = None;
                self.placed -= 1;
                sink.remove(id);
                freed = true;
            }
        }
        self.removed.clear();
        // Room was freed for a probe that waits: every probe is visited again.
        if freed && self.unplaced {
            self.probes = QueryState::new(world);
            self.unplaced = false;
        }
        // Additions and changes.
        let (ids, placed, unplaced) = (&mut self.ids, &mut self.placed, &mut self.unplaced);
        self.probes
            .for_each_readonly(world, |(entity, global, probe)| {
                let probe = probe.at(global);
                let index = entity.index() as usize;
                if let Some(Some((e, id))) = ids.get(index)
                    && *e == entity
                {
                    sink.set(*id, probe);
                    return;
                }
                match sink.add(probe) {
                    Some(id) => {
                        if ids.len() <= index {
                            ids.resize(index + 1, None);
                        }
                        ids[index] = Some((entity, id));
                        *placed += 1;
                    }
                    None => *unplaced = true,
                }
            });
    }
}

/// The box over which global illumination comes from a volume of probes (ADR 0088,
/// ADR 0089), on GPUs with ray queries: probes about `spacing` apart over the box around
/// its entity (the world-aligned box around it when the entity turns), each tracing the
/// scene. One volume lights the world: the first one found ([`GiExtract`]). Moving or
/// resizing it makes a new volume, which converges again: place it once per level.
#[derive(Debug, Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
#[repr(C)]
pub struct GiVolume {
    /// Half the box's size along the entity's axes.
    pub half_extents: Vec3,
    /// The distance between neighbouring probes aimed for: they spread further apart to
    /// stay within `max_probes`.
    pub spacing: f32,
    /// The most probes (8 to 32,768; 0: 16,384).
    pub max_probes: u32,
}

reflect_component!(pod GiVolume);

impl GiVolume {
    /// A volume over the box of `half_extents` around its entity, probes about `spacing`
    /// apart, at most 16,384 of them.
    pub fn new(half_extents: Vec3, spacing: f32) -> Self {
        Self {
            half_extents,
            spacing,
            max_probes: 0,
        }
    }

    /// The probes' grid for an entity at `global`.
    pub fn grid(&self, global: &GlobalTransform) -> ProbeGrid {
        let m = global.0.matrix3;
        let e = self.half_extents.abs();
        // The world-aligned box around the turned and scaled one.
        let extent = Vec3::from(m.x_axis).abs() * e.x
            + Vec3::from(m.y_axis).abs() * e.y
            + Vec3::from(m.z_axis).abs() * e.z;
        let center = Vec3::from(global.0.translation);
        let max_probes = match self.max_probes {
            0 => 16_384,
            n => n,
        };
        ProbeGrid::covering(center - extent, center + extent, self.spacing, max_probes)
    }
}

/// Finds the world's [`GiVolume`]: the first one in the world's order.
pub struct GiExtract {
    volumes: QueryState<(&'static GlobalTransform, &'static GiVolume)>,
}

impl core::fmt::Debug for GiExtract {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GiExtract").finish_non_exhaustive()
    }
}

impl GiExtract {
    /// State for `world`.
    pub fn new(world: &mut World) -> Self {
        Self {
            volumes: QueryState::new(world),
        }
    }

    /// The grid of the world's volume, `None` without one.
    pub fn grid(&mut self, world: &World) -> Option<ProbeGrid> {
        let mut grid = None;
        self.volumes.for_each_readonly(world, |(global, volume)| {
            if grid.is_none() {
                grid = Some(volume.grid(global));
            }
        });
        grid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::{Affine3A, Quat};
    use engine_ecs::Transform;

    /// A probe set with room for `capacity` probes.
    struct Set {
        slots: Vec<Option<reflections::ReflectionProbe>>,
    }

    impl ProbeSink for Set {
        type Id = usize;
        fn add(&mut self, probe: reflections::ReflectionProbe) -> Option<usize> {
            let slot = self.slots.iter().position(Option::is_none)?;
            self.slots[slot] = Some(probe);
            Some(slot)
        }
        fn set(&mut self, id: usize, probe: reflections::ReflectionProbe) {
            self.slots[id] = Some(probe);
        }
        fn remove(&mut self, id: usize) {
            self.slots[id] = None;
        }
    }

    fn at(x: f32, y: f32, z: f32) -> GlobalTransform {
        GlobalTransform(Transform::from_xyz(x, y, z).to_affine())
    }

    #[test]
    fn probes_follow_their_entities_and_wait_for_room() {
        let mut world = World::new();
        let mut extract = ProbeExtract::<usize>::new(&mut world);
        let mut set = Set {
            slots: vec![None; 2],
        };
        let a = world.spawn((at(1.0, 2.0, 3.0), ReflectionProbe::new(Vec3::splat(2.0))));
        let b = world.spawn((
            GlobalTransform(Affine3A::from_scale_rotation_translation(
                Vec3::new(2.0, 1.0, 1.0),
                Quat::from_rotation_y(0.5),
                Vec3::new(10.0, 0.0, 0.0),
            )),
            ReflectionProbe {
                intensity: 0.5,
                blend: 0.25,
                manual: 1,
                ..ReflectionProbe::new(Vec3::new(1.0, 2.0, 3.0))
            },
        ));
        extract.sync(&mut world, &mut set);
        assert_eq!(extract.len(), 2);
        let pa = set.slots[extract.probe(a).unwrap()].unwrap();
        assert_eq!(
            pa,
            reflections::ReflectionProbe::room(Vec3::new(1.0, 2.0, 3.0), Vec3::splat(2.0))
        );
        let pb = set.slots[extract.probe(b).unwrap()].unwrap();
        // The entity's scale and turn reach both boxes; its blend, intensity and refresh.
        assert!((pb.parallax.half_extents - Vec3::new(2.0, 2.0, 3.0)).length() < 1e-5);
        assert!((pb.influence.half_extents - Vec3::new(2.25, 2.25, 3.25)).length() < 1e-5);
        assert!(
            pb.parallax
                .rotation
                .angle_between(Quat::from_rotation_y(0.5))
                < 1e-4
        );
        assert_eq!(pb.position, Vec3::new(10.0, 0.0, 0.0));
        assert_eq!((pb.intensity, pb.blend_distance), (0.5, 0.25));
        assert_eq!(pb.refresh, ProbeRefresh::Manual);

        // A moved probe is set again in its slot.
        let slot = extract.probe(a).unwrap();
        *world.get_mut::<GlobalTransform>(a).unwrap() = at(4.0, 2.0, 3.0);
        extract.sync(&mut world, &mut set);
        assert_eq!(extract.probe(a), Some(slot));
        assert_eq!(set.slots[slot].unwrap().position, Vec3::new(4.0, 2.0, 3.0));

        // A third finds no room and waits; it is placed once another goes.
        let c = world.spawn((at(0.0, 0.0, 9.0), ReflectionProbe::new(Vec3::ONE)));
        extract.sync(&mut world, &mut set);
        assert!(extract.probe(c).is_none() && extract.waiting());
        world.despawn(a);
        extract.sync(&mut world, &mut set);
        assert!(extract.probe(a).is_none());
        let slot = extract.probe(c).unwrap();
        assert_eq!(set.slots[slot].unwrap().position, Vec3::new(0.0, 0.0, 9.0));
        assert!(!extract.waiting());
        assert_eq!(extract.len(), 2);

        // A removed component frees its slot too.
        world.remove::<ReflectionProbe>(b);
        extract.sync(&mut world, &mut set);
        assert_eq!(extract.len(), 1);
        assert_eq!(set.slots.iter().flatten().count(), 1);
    }

    #[test]
    fn the_volume_covers_its_entitys_box() {
        let mut world = World::new();
        let mut extract = GiExtract::new(&mut world);
        assert!(extract.grid(&world).is_none());
        world.spawn((
            at(1.0, 2.0, 3.0),
            GiVolume::new(Vec3::new(4.0, 2.0, 4.0), 1.0),
        ));
        let grid = extract.grid(&world).unwrap();
        assert_eq!(grid.origin, Vec3::new(-3.0, 0.0, -1.0));
        assert_eq!(grid.max(), Vec3::new(5.0, 4.0, 7.0));
        assert_eq!(grid.counts, [9, 5, 9]);
        // Turned a quarter about Y: the world-aligned box around it.
        let turned = GiVolume {
            max_probes: 64,
            ..GiVolume::new(Vec3::new(4.0, 1.0, 1.0), 0.5)
        }
        .grid(&GlobalTransform(Affine3A::from_rotation_translation(
            Quat::from_rotation_y(core::f32::consts::FRAC_PI_2),
            Vec3::ZERO,
        )));
        assert!((turned.origin - Vec3::new(-1.0, -1.0, -4.0)).length() < 1e-5);
        assert!(turned.probe_count() <= 64);
    }
}
