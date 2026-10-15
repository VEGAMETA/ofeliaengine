//! Skinned meshes as components (ADR 0099 step 3).

use crate::runtime::Runtime;
use engine_ecs::{Entity, QueryState, bytemuck, pod_component};
use engine_render::gpu_scene::{GpuScene, InstanceId};
use engine_render::skinning::{SkinnedId, Skinning};

/// A skinned instance drawn as its entity's mesh: the entity's
/// [`MeshRenderer`](crate::MeshRenderer) draws the instance's own mesh
/// (`Skinning::add_instance`). The animation sets its skinning matrices
/// (`Skinning::set_palette`); [`Runtime::place_skinned`] places it where the entity's
/// scene instance is, every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct SkinnedMesh {
    /// The skinned instance.
    pub id: SkinnedId,
}

pod_component!(SkinnedMesh);

/// The skinned entities, for [`Runtime::place_skinned`].
pub(crate) type SkinnedQuery = QueryState<(Entity, &'static SkinnedMesh)>;

impl Runtime<InstanceId> {
    /// Places every [`SkinnedMesh`] at its entity's scene instance this frame: ties the
    /// skinned instance to it and gives it the instance's transform, bit for bit (the
    /// motion pass then matches the forward pass's depth). Call after [`Self::frame`] and
    /// the frame's palettes, before `Skinning::update_bounds`.
    pub fn place_skinned(&mut self, scene: &GpuScene, skinning: &mut Skinning) {
        engine_profiler::zone!("runtime::Runtime::place_skinned");
        let extract = &self.extract;
        self.skinned
            .for_each_readonly(&self.world, |(entity, skinned)| {
                if let Some(instance) = extract.instance(entity)
                    && let Some(placed) = scene.get(instance)
                {
                    skinning.set_scene_instance(skinned.id, Some(instance));
                    skinning.set_transform(skinned.id, &placed.world_from_object);
                }
            });
    }
}
