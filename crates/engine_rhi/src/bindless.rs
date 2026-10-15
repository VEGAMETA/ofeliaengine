//! Bindless resources (ADR 0006, ADR 0013).
//!
//! One descriptor set holds every texture and sampler the renderer uses:
//!
//! | binding | contents | Slang |
//! |---|---|---|
//! | 0 | sampled images | `[[vk::binding(0, 0)]] Texture2D g_textures[];` |
//! | 1 | storage images | `[[vk::binding(1, 0)]] RWTexture2D<float4> g_images[];` |
//! | 2 | samplers | `[[vk::binding(2, 0)]] SamplerState g_samplers[];` |
//!
//! Shaders index them with integers from push constants or buffers; buffers are reached
//! through buffer device addresses, so they need no descriptors at all. One pipeline
//! layout (this set plus 128 bytes of push constants for every stage) serves every
//! pipeline, so binding never changes between draws.
//!
//! Indices are recycled only after the frames that might still use them have finished
//! (`retire` then `collect`), and the bindings are "update unused while pending", so adding
//! a texture never waits for the GPU.

use crate::device::Device;
use crate::error::RhiError;
use ash::vk;
use core::cell::RefCell;

/// Push constant bytes available to every pipeline (the Vulkan minimum guarantee).
pub const PUSH_CONSTANT_BYTES: u32 = 128;

/// Built-in samplers, at fixed indices in binding 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SamplerKind {
    /// Trilinear, repeat.
    LinearRepeat = 0,
    /// Trilinear, clamp to edge.
    LinearClamp = 1,
    /// Nearest, repeat (pixel art, lookup tables).
    NearestRepeat = 2,
    /// Nearest, clamp to edge.
    NearestClamp = 3,
    /// 16× anisotropic, repeat (materials).
    AnisotropicRepeat = 4,
    /// Depth comparison (shadow maps), clamp, `LESS_OR_EQUAL`.
    ShadowCompare = 5,
    /// Depth comparison, repeat (wrap-around shadow maps: the sun's cached cascades,
    /// ADR 0093), `LESS_OR_EQUAL`.
    ShadowCompareRepeat = 6,
}

/// How a material texture repeats along one axis (glTF's sampler wrap modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u32)]
pub enum Wrap {
    /// Tiles.
    #[default]
    Repeat = 0,
    /// Tiles, every other copy mirrored.
    MirroredRepeat = 1,
    /// Stretches the edge texels.
    ClampToEdge = 2,
}

impl Wrap {
    const ALL: [Self; 3] = [Self::Repeat, Self::MirroredRepeat, Self::ClampToEdge];

    fn address_mode(self) -> vk::SamplerAddressMode {
        match self {
            Self::Repeat => vk::SamplerAddressMode::REPEAT,
            Self::MirroredRepeat => vk::SamplerAddressMode::MIRRORED_REPEAT,
            Self::ClampToEdge => vk::SamplerAddressMode::CLAMP_TO_EDGE,
        }
    }
}

/// Material samplers after the built-in ones: 16× anisotropic trilinear, one per pair of
/// wrap modes.
const MATERIAL_SAMPLERS: u32 = 9;

/// Sets of material samplers besides the default one (no bias, 16× anisotropic): with a
/// level-of-detail bias (temporal upscaling, ADR 0060) or other anisotropic filtering (the
/// player's setting, ADR 0098). Their slots are reserved, the samplers made on demand.
const EXTRA_MATERIAL_SETS: u32 = 16;

/// A set of material samplers besides the default one ([`Bindless::material_samplers`]).
struct MaterialSet {
    /// The LOD bias in 1/16 levels.
    bias: i32,
    /// The most samples a lookup takes (1: trilinear).
    anisotropy: u32,
    samplers: Vec<vk::Sampler>,
}

const SAMPLER_KINDS: [SamplerKind; 7] = [
    SamplerKind::LinearRepeat,
    SamplerKind::LinearClamp,
    SamplerKind::NearestRepeat,
    SamplerKind::NearestClamp,
    SamplerKind::AnisotropicRepeat,
    SamplerKind::ShadowCompare,
    SamplerKind::ShadowCompareRepeat,
];

/// Hands out descriptor indices; freed ones come back after their frame finished.
#[derive(Debug)]
struct Indices {
    capacity: u32,
    next: u32,
    free: Vec<u32>,
    /// `(frame after which it is unused, index)`.
    retired: Vec<(u64, u32)>,
}

impl Indices {
    fn new(capacity: u32) -> Self {
        Self {
            capacity,
            next: 0,
            free: Vec::new(),
            retired: Vec::new(),
        }
    }

    fn alloc(&mut self) -> Option<u32> {
        if let Some(index) = self.free.pop() {
            return Some(index);
        }
        (self.next < self.capacity).then(|| {
            self.next += 1;
            self.next - 1
        })
    }

    fn retire(&mut self, index: u32, last_frame: u64) {
        self.retired.push((last_frame, index));
    }

    fn collect(&mut self, completed_frame: u64) {
        let free = &mut self.free;
        self.retired.retain(|&(frame, index)| {
            let done = frame <= completed_frame;
            if done {
                free.push(index);
            }
            !done
        });
    }
}

/// The bindless descriptor set, its layout and the shared pipeline layout. Drop it
/// before the [`Device`], after the GPU no longer uses it.
///
/// Every method takes `&self`, so renderers and the render graph can share it; it is
/// used from one thread (the render thread) and is `Send` but not `Sync`.
pub struct Bindless {
    device: ash::Device,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    pipeline_layout: vk::PipelineLayout,
    /// Owns `pipeline_layout` once made: pipelines still to be created with it keep it
    /// alive past the set (ADR 0079).
    shared_layout: Option<crate::pipeline::SharedPipelineLayout>,
    samplers: Vec<vk::Sampler>,
    /// The other material sampler sets.
    extra: RefCell<Vec<MaterialSet>>,
    max_anisotropy: f32,
    max_lod_bias: f32,
    sampled: RefCell<Indices>,
    storage: RefCell<Indices>,
}

impl core::fmt::Debug for Bindless {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bindless")
            .field("sampled_images", &self.sampled.borrow().capacity)
            .field("storage_images", &self.storage.borrow().capacity)
            .finish()
    }
}

impl Bindless {
    /// Creates the set with up to 65 536 sampled and 16 384 storage images (less if the
    /// device's limits are lower).
    pub fn new(device: &Device) -> Result<Self, RhiError> {
        let d = device.raw();
        let limits = device.adapter().limits;
        // Queried separately: the update-after-bind limits live in the 1.2 properties.
        let mut props12 = vk::PhysicalDeviceVulkan12Properties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut props12);
        // SAFETY: valid physical device of a live instance; valid output chain.
        unsafe {
            device
                .instance_raw()
                .get_physical_device_properties2(device.physical(), &mut props2)
        };
        let samplers_count =
            SAMPLER_KINDS.len() as u32 + MATERIAL_SAMPLERS * (1 + EXTRA_MATERIAL_SETS);
        let per_stage_total = props12
            .max_per_stage_update_after_bind_resources
            .saturating_sub(samplers_count);
        let sampled_count = (1u32 << 16)
            .min(props12.max_descriptor_set_update_after_bind_sampled_images)
            .min(props12.max_per_stage_descriptor_update_after_bind_sampled_images)
            .min(per_stage_total / 2);
        let storage_count = (1u32 << 14)
            .min(props12.max_descriptor_set_update_after_bind_storage_images)
            .min(props12.max_per_stage_descriptor_update_after_bind_storage_images)
            .min(per_stage_total - sampled_count);

        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(sampled_count)
                .stage_flags(vk::ShaderStageFlags::ALL),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(storage_count)
                .stage_flags(vk::ShaderStageFlags::ALL),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(samplers_count)
                .stage_flags(vk::ShaderStageFlags::ALL),
        ];
        let flags = vk::DescriptorBindingFlags::PARTIALLY_BOUND
            | vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
            | vk::DescriptorBindingFlags::UPDATE_UNUSED_WHILE_PENDING;
        let binding_flags = [flags; 3];
        let mut flags_info =
            vk::DescriptorSetLayoutBindingFlagsCreateInfo::default().binding_flags(&binding_flags);
        let layout_info = vk::DescriptorSetLayoutCreateInfo::default()
            .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
            .bindings(&bindings)
            .push_next(&mut flags_info);

        // Objects are created one by one; on failure `partial` destroys what exists.
        let mut partial = Self {
            device: d.clone(),
            set_layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            shared_layout: None,
            samplers: Vec::with_capacity(SAMPLER_KINDS.len()),
            extra: RefCell::new(Vec::new()),
            max_anisotropy: 16.0f32.min(limits.max_sampler_anisotropy),
            max_lod_bias: limits.max_sampler_lod_bias,
            sampled: RefCell::new(Indices::new(sampled_count)),
            storage: RefCell::new(Indices::new(storage_count)),
        };
        // SAFETY: valid create infos; every object is owned by `partial`.
        unsafe {
            partial.set_layout = d
                .create_descriptor_set_layout(&layout_info, None)
                .map_err(RhiError::vk("vkCreateDescriptorSetLayout"))?;
            let sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::SAMPLED_IMAGE,
                    descriptor_count: sampled_count,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_IMAGE,
                    descriptor_count: storage_count,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::SAMPLER,
                    descriptor_count: samplers_count,
                },
            ];
            partial.pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)
                        .max_sets(1)
                        .pool_sizes(&sizes),
                    None,
                )
                .map_err(RhiError::vk("vkCreateDescriptorPool"))?;
            let layouts = [partial.set_layout];
            partial.set = d
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(partial.pool)
                        .set_layouts(&layouts),
                )
                .map_err(RhiError::vk("vkAllocateDescriptorSets"))?[0];
            let push = [vk::PushConstantRange {
                stage_flags: vk::ShaderStageFlags::ALL,
                offset: 0,
                size: PUSH_CONSTANT_BYTES.min(limits.max_push_constants_size),
            }];
            partial.pipeline_layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&layouts)
                        .push_constant_ranges(&push),
                    None,
                )
                .map_err(RhiError::vk("vkCreatePipelineLayout"))?;
            partial.shared_layout = Some(crate::pipeline::SharedPipelineLayout::new(
                device,
                partial.pipeline_layout,
            ));
            for kind in SAMPLER_KINDS {
                let (filter, mip, address) = match kind {
                    SamplerKind::LinearRepeat
                    | SamplerKind::AnisotropicRepeat
                    | SamplerKind::ShadowCompareRepeat => (
                        vk::Filter::LINEAR,
                        vk::SamplerMipmapMode::LINEAR,
                        vk::SamplerAddressMode::REPEAT,
                    ),
                    SamplerKind::LinearClamp | SamplerKind::ShadowCompare => (
                        vk::Filter::LINEAR,
                        vk::SamplerMipmapMode::LINEAR,
                        vk::SamplerAddressMode::CLAMP_TO_EDGE,
                    ),
                    SamplerKind::NearestRepeat => (
                        vk::Filter::NEAREST,
                        vk::SamplerMipmapMode::NEAREST,
                        vk::SamplerAddressMode::REPEAT,
                    ),
                    SamplerKind::NearestClamp => (
                        vk::Filter::NEAREST,
                        vk::SamplerMipmapMode::NEAREST,
                        vk::SamplerAddressMode::CLAMP_TO_EDGE,
                    ),
                };
                let anisotropic = kind == SamplerKind::AnisotropicRepeat;
                let compare =
                    kind == SamplerKind::ShadowCompare || kind == SamplerKind::ShadowCompareRepeat;
                let info = vk::SamplerCreateInfo::default()
                    .mag_filter(filter)
                    .min_filter(filter)
                    .mipmap_mode(mip)
                    .address_mode_u(address)
                    .address_mode_v(address)
                    .address_mode_w(address)
                    .max_lod(vk::LOD_CLAMP_NONE)
                    .anisotropy_enable(anisotropic)
                    .max_anisotropy(if anisotropic {
                        16.0f32.min(limits.max_sampler_anisotropy)
                    } else {
                        1.0
                    })
                    .compare_enable(compare)
                    .compare_op(vk::CompareOp::LESS_OR_EQUAL);
                partial.samplers.push(
                    d.create_sampler(&info, None)
                        .map_err(RhiError::vk("vkCreateSampler"))?,
                );
            }
            let materials = partial.create_material_samplers(0.0, partial.max_anisotropy)?;
            partial.samplers.extend(materials);
            let image_infos: Vec<vk::DescriptorImageInfo> = partial
                .samplers
                .iter()
                .map(|&sampler| vk::DescriptorImageInfo::default().sampler(sampler))
                .collect();
            d.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(partial.set)
                    .dst_binding(2)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&image_infos)],
                &[],
            );
        }
        Ok(partial)
    }

    /// The descriptor set (set 0 of [`Self::pipeline_layout`]).
    pub fn set(&self) -> vk::DescriptorSet {
        self.set
    }

    /// The set's layout, for pipelines that add sets of their own.
    pub fn set_layout(&self) -> vk::DescriptorSetLayout {
        self.set_layout
    }

    /// The pipeline layout every engine pipeline uses: the bindless set and
    /// [`PUSH_CONSTANT_BYTES`] of push constants visible to all stages.
    pub fn pipeline_layout(&self) -> vk::PipelineLayout {
        self.pipeline_layout
    }

    /// The same layout, for pipelines described now and created later
    /// ([`crate::LazyPipeline`], ADR 0079), which keep it alive until they are made.
    pub fn shared_pipeline_layout(&self) -> &crate::pipeline::SharedPipelineLayout {
        self.shared_layout
            .as_ref()
            .unwrap_or_else(|| unreachable!("a made set has its layout"))
    }

    /// Capacity of binding 0 and 1.
    pub fn capacities(&self) -> (u32, u32) {
        (
            self.sampled.borrow().capacity,
            self.storage.borrow().capacity,
        )
    }

    /// Index of a built-in sampler in `g_samplers`.
    pub fn sampler(kind: SamplerKind) -> u32 {
        kind as u32
    }

    /// Index in `g_samplers` of the 16× anisotropic trilinear sampler that wraps `s`
    /// horizontally and `t` vertically (material textures).
    pub fn material_sampler(s: Wrap, t: Wrap) -> u32 {
        Self::material_sampler_in(SAMPLER_KINDS.len() as u32, s, t)
    }

    /// Index in `g_samplers` of the material sampler for `s` and `t` in the set starting
    /// at `first` ([`Self::material_samplers`]).
    pub fn material_sampler_in(first: u32, s: Wrap, t: Wrap) -> u32 {
        first + s as u32 * 3 + t as u32
    }

    /// The first index in `g_samplers` of a set of material samplers whose level of
    /// detail is biased by `bias` (negative: sharper; to 1/16 of a level, within the
    /// device's limit) and which take up to `anisotropy` samples along a texture's slope
    /// (1: trilinear; at most 16 and the device's limit).
    ///
    /// The default set (no bias, 16×) is always there. Up to 16 others are made on first
    /// request (a few samplers, no pipelines); beyond that the nearest one is returned,
    /// one with the same anisotropy first. Both live in the samplers, so shaders pay
    /// nothing for them (ADR 0060: a bias in the shader cost the Radeon 8 registers in
    /// every shading pipeline; ADR 0098).
    ///
    /// # Errors
    ///
    /// When a sampler cannot be created.
    pub fn material_samplers(&self, bias: f32, anisotropy: u32) -> Result<u32, RhiError> {
        let base = SAMPLER_KINDS.len() as u32;
        let limit = self.max_lod_bias.max(0.0);
        let sixteenths = (bias.clamp(-limit, limit) * 16.0).round() as i32;
        let anisotropy = (anisotropy.max(1) as f32).min(self.max_anisotropy) as u32;
        if sixteenths == 0 && anisotropy == self.max_anisotropy as u32 {
            return Ok(base);
        }
        let first = |k: usize| base + MATERIAL_SAMPLERS * (1 + k as u32);
        let mut extra = self.extra.borrow_mut();
        if let Some(k) = extra
            .iter()
            .position(|set| set.bias == sixteenths && set.anisotropy == anisotropy)
        {
            return Ok(first(k));
        }
        if extra.len() as u32 >= EXTRA_MATERIAL_SETS {
            let nearest = extra
                .iter()
                .enumerate()
                .min_by_key(|(_, set)| {
                    (set.anisotropy != anisotropy, (set.bias - sixteenths).abs())
                })
                .map_or(0, |(k, _)| k);
            return Ok(first(nearest));
        }
        let samplers =
            self.create_material_samplers(sixteenths as f32 / 16.0, anisotropy as f32)?;
        let k = extra.len();
        let infos: Vec<vk::DescriptorImageInfo> = samplers
            .iter()
            .map(|&sampler| vk::DescriptorImageInfo::default().sampler(sampler))
            .collect();
        // SAFETY: the slots were never written, so no submitted work reads them
        // (UPDATE_UNUSED_WHILE_PENDING); the samplers are live.
        unsafe {
            self.device.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(self.set)
                    .dst_binding(2)
                    .dst_array_element(first(k))
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&infos)],
                &[],
            );
        }
        extra.push(MaterialSet {
            bias: sixteenths,
            anisotropy,
            samplers,
        });
        Ok(first(k))
    }

    /// The 9 material samplers (one per pair of wrap modes) with `bias` and up to
    /// `anisotropy` samples (1: trilinear).
    fn create_material_samplers(
        &self,
        bias: f32,
        anisotropy: f32,
    ) -> Result<Vec<vk::Sampler>, RhiError> {
        let mut samplers = Vec::with_capacity(MATERIAL_SAMPLERS as usize);
        for s in Wrap::ALL {
            for t in Wrap::ALL {
                let info = vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                    .address_mode_u(s.address_mode())
                    .address_mode_v(t.address_mode())
                    .address_mode_w(vk::SamplerAddressMode::REPEAT)
                    .mip_lod_bias(bias)
                    .max_lod(vk::LOD_CLAMP_NONE)
                    .anisotropy_enable(anisotropy > 1.0)
                    .max_anisotropy(anisotropy.max(1.0));
                // SAFETY: valid create info; the device is live.
                match unsafe { self.device.create_sampler(&info, None) } {
                    Ok(sampler) => samplers.push(sampler),
                    Err(e) => {
                        for sampler in samplers {
                            // SAFETY: just created, never used.
                            unsafe { self.device.destroy_sampler(sampler, None) };
                        }
                        return Err(RhiError::vk("vkCreateSampler")(e));
                    }
                }
            }
        }
        Ok(samplers)
    }

    /// Adds a sampled image view (in `READ_ONLY_OPTIMAL` when sampled); returns its
    /// index in `g_textures`, or `None` when the binding is full.
    pub fn add_sampled_image(&self, view: vk::ImageView) -> Option<u32> {
        let index = self.sampled.borrow_mut().alloc()?;
        self.write_image(0, index, view, vk::ImageLayout::READ_ONLY_OPTIMAL);
        Some(index)
    }

    /// Adds a storage image view (in `GENERAL` when used); returns its index in
    /// `g_images`, or `None` when the binding is full.
    pub fn add_storage_image(&self, view: vk::ImageView) -> Option<u32> {
        let index = self.storage.borrow_mut().alloc()?;
        self.write_image(1, index, view, vk::ImageLayout::GENERAL);
        Some(index)
    }

    /// Releases a sampled image index. It is reused once frame `last_frame` has
    /// finished (see [`Self::collect`]).
    pub fn remove_sampled_image(&self, index: u32, last_frame: u64) {
        self.sampled.borrow_mut().retire(index, last_frame);
    }

    /// Releases a storage image index, as [`Self::remove_sampled_image`].
    pub fn remove_storage_image(&self, index: u32, last_frame: u64) {
        self.storage.borrow_mut().retire(index, last_frame);
    }

    /// Makes indices retired up to `completed_frame` available again. Call once per
    /// frame with the last frame the GPU finished.
    pub fn collect(&self, completed_frame: u64) {
        self.sampled.borrow_mut().collect(completed_frame);
        self.storage.borrow_mut().collect(completed_frame);
    }

    /// Binds the set for `bind_point` in `cmd`.
    ///
    /// # Safety
    ///
    /// `cmd` is recording and belongs to this device.
    pub unsafe fn bind(&self, cmd: vk::CommandBuffer, bind_point: vk::PipelineBindPoint) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.device.cmd_bind_descriptor_sets(
                cmd,
                bind_point,
                self.pipeline_layout,
                0,
                &[self.set],
                &[],
            )
        };
    }

    /// Sets push constants for every stage.
    ///
    /// # Safety
    ///
    /// `cmd` is recording; `bytes` is at most [`PUSH_CONSTANT_BYTES`] long.
    pub unsafe fn push_constants(&self, cmd: vk::CommandBuffer, bytes: &[u8]) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.device.cmd_push_constants(
                cmd,
                self.pipeline_layout,
                vk::ShaderStageFlags::ALL,
                0,
                bytes,
            )
        };
    }

    fn write_image(&self, binding: u32, index: u32, view: vk::ImageView, layout: vk::ImageLayout) {
        let (ty, info) = if binding == 0 {
            (
                vk::DescriptorType::SAMPLED_IMAGE,
                [vk::DescriptorImageInfo::default()
                    .image_view(view)
                    .image_layout(layout)],
            )
        } else {
            (
                vk::DescriptorType::STORAGE_IMAGE,
                [vk::DescriptorImageInfo::default()
                    .image_view(view)
                    .image_layout(layout)],
            )
        };
        let write = vk::WriteDescriptorSet::default()
            .dst_set(self.set)
            .dst_binding(binding)
            .dst_array_element(index)
            .descriptor_type(ty)
            .image_info(&info);
        // SAFETY: the index is within the binding; update-after-bind and
        // unused-while-pending allow this while frames are in flight (the index is not
        // used by them: it is fresh or retired after their completion).
        unsafe { self.device.update_descriptor_sets(&[write], &[]) };
    }
}

impl Drop for Bindless {
    fn drop(&mut self) {
        // SAFETY: owned objects; the GPU no longer uses them (type-level contract).
        unsafe {
            for &sampler in &self.samplers {
                self.device.destroy_sampler(sampler, None);
            }
            for set in self.extra.get_mut().iter() {
                for &sampler in &set.samplers {
                    self.device.destroy_sampler(sampler, None);
                }
            }
            // Once shared, its last holder destroys the layout.
            if self.shared_layout.is_none() && self.pipeline_layout != vk::PipelineLayout::null() {
                self.device
                    .destroy_pipeline_layout(self.pipeline_layout, None);
            }
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
            }
            if self.set_layout != vk::DescriptorSetLayout::null() {
                self.device
                    .destroy_descriptor_set_layout(self.set_layout, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_are_recycled_only_after_their_frame() {
        let mut indices = Indices::new(3);
        let a = indices.alloc().unwrap();
        let b = indices.alloc().unwrap();
        let _c = indices.alloc().unwrap();
        assert!(indices.alloc().is_none());
        indices.retire(a, 10);
        indices.retire(b, 12);
        indices.collect(9);
        assert!(indices.alloc().is_none());
        indices.collect(10);
        assert_eq!(indices.alloc(), Some(a));
        assert!(indices.alloc().is_none());
        indices.collect(12);
        assert_eq!(indices.alloc(), Some(b));
    }
}
