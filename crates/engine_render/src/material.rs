//! Materials (ADR 0040): glTF 2.0's metallic-roughness model, every material in one GPU
//! table that shaders index per instance. No descriptor sets per material and no
//! rebinding between draws: a material is 80 bytes of factors and texture indices, and a
//! texture index carries its sampler (the wrap modes) in its top 8 bits.
//!
//! Textures come from a [`TexturePool`]. A missing texture is the same as white, so its
//! factor applies unchanged (a missing normal map leaves the interpolated normal), and
//! the shaders skip its sample; a texture not uploaded yet counts as missing.
//! [`MaterialTable::begin_frame`] resolves materials again when textures become ready or
//! are removed.
//!
//! Slot 0 is the default material (grey, dielectric): stale ids draw with it.
//!
//! **Material types** (ADR 0092): a material may be of a type of its own (a Slang type
//! implementing `IMaterial`, registered with the forward renderer,
//! `ForwardRenderer::register_material_type`). It then carries its type's parameters
//! ([`MaterialParams`], up to [`PARAMS_BYTES`]) besides its glTF description, which stays
//! its fallback: it draws the material until the type's pipelines are made, and the passes
//! that run no material code (GI's ray hits) read it.

use crate::graph::{Access, Buffer, BufferState, FrameGraph, ImportedBuffer, ShaderStages};
use crate::texture_pool::{TextureId, TexturePool};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError, Wrap};

/// How a material's alpha is used.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AlphaMode {
    /// Alpha is ignored.
    #[default]
    Opaque,
    /// Alpha tested: fragments whose alpha is below `cutoff` are discarded (foliage,
    /// fences). Give base color textures of masked materials coverage-preserving mip
    /// levels (`engine_assets::TextureUsage::ColorMasked`).
    Mask {
        /// Alpha cutoff, 0 … 1.
        cutoff: f32,
    },
    /// Blended over what is behind (glTF's `BLEND`): alpha is the surface's coverage,
    /// scaling its whole shading (light, reflections, emission). Drawn after the opaque
    /// surfaces, back to front, without writing depth and without casting shadows
    /// (ADR 0054).
    Blend,
}

/// A texture of a material and how it repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureRef {
    /// The texture.
    pub texture: TextureId,
    /// Wrap along u and v.
    pub wrap: [Wrap; 2],
}

impl TextureRef {
    /// A texture that repeats both ways.
    pub fn new(texture: TextureId) -> Self {
        Self {
            texture,
            wrap: [Wrap::Repeat; 2],
        }
    }
}

/// How light passes through a subsurface-scattering material (ADR 0059).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transmission {
    /// None: light only scatters within the lit side.
    #[default]
    None,
    /// Thin surfaces (leaves, paper, ears): light from behind passes through the
    /// material's thickness.
    Thin,
    /// Solid objects: light from behind passes through the thicker of the material's
    /// thickness and the object itself, measured in the sun's shadow map.
    Thick,
}

/// Where a subsurface-scattering material's base color applies (ADR 0059).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubsurfaceTexturing {
    /// Where light leaves the surface: the texture's detail stays sharp (skin with pores
    /// and freckles in the texture).
    #[default]
    PostScatter,
    /// Half where light enters and half where it leaves (the square root of the color at
    /// each): the texture's detail blurs with the light, as in a scan whose texture
    /// already holds the scattering.
    PreAndPostScatter,
}

/// Subsurface scattering of a material (ADR 0059): its diffuse light spreads under the
/// surface following one of the renderer's profiles
/// ([`crate::forward::ForwardRenderer::set_subsurface_profiles`]). Opaque and masked
/// materials only; blended ones ignore it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Subsurface {
    /// Index of the profile in the renderer's list (clamped to the list).
    pub profile: u8,
    /// How much of the diffuse light scatters, 0 … 1 (1: all of it).
    pub mask: f32,
    /// Light passing through from behind.
    pub transmission: Transmission,
    /// Where the base color applies.
    pub texturing: SubsurfaceTexturing,
    /// Thickness in world units for transmission (glTF's `KHR_materials_volume`
    /// `thicknessFactor`).
    pub thickness: f32,
    /// Thickness in green, times `thickness` (`KHR_materials_volume`'s
    /// `thicknessTexture`), linear.
    pub thickness_texture: Option<TextureRef>,
}

impl Default for Subsurface {
    /// Profile 0, all the diffuse light, no transmission, post-scatter texturing.
    fn default() -> Self {
        Self {
            profile: 0,
            mask: 1.0,
            transmission: Transmission::None,
            texturing: SubsurfaceTexturing::PostScatter,
            thickness: 0.0,
            thickness_texture: None,
        }
    }
}

/// A material: glTF 2.0 metallic-roughness. Each factor multiplies its texture; a missing
/// texture counts as white (for the normal map: no perturbation). Every texture uses the
/// mesh's texture coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaterialDesc {
    /// Linear base color and alpha.
    pub base_color: [f32; 4],
    /// Base color (sRGB) and alpha.
    pub base_color_texture: Option<TextureRef>,
    /// 0 dielectric … 1 metal.
    pub metallic: f32,
    /// Perceptual roughness, 0 (mirror) … 1.
    pub roughness: f32,
    /// Roughness in green and metallic in blue, linear.
    pub metallic_roughness_texture: Option<TextureRef>,
    /// Tangent-space normal map (RGB = XYZ × 0.5 + 0.5, Y towards decreasing v).
    pub normal_texture: Option<TextureRef>,
    /// Scale of the normal map's X and Y.
    pub normal_scale: f32,
    /// Ambient occlusion in red, linear.
    pub occlusion_texture: Option<TextureRef>,
    /// How much of the occlusion applies, 0 … 1.
    pub occlusion_strength: f32,
    /// Emitted radiance, linear (above 1 for bright emitters).
    pub emissive: [f32; 3],
    /// Emission (sRGB), multiplied by `emissive`.
    pub emissive_texture: Option<TextureRef>,
    /// Alpha usage.
    pub alpha: AlphaMode,
    /// Both faces are drawn (back faces lit with the normal flipped), and both cast
    /// shadows.
    pub double_sided: bool,
    /// Subsurface scattering, for opaque and masked materials (ADR 0059).
    pub subsurface: Option<Subsurface>,
}

impl MaterialDesc {
    /// An untextured opaque surface.
    pub fn solid(color: [f32; 3], metallic: f32, roughness: f32) -> Self {
        Self {
            base_color: [color[0], color[1], color[2], 1.0],
            metallic,
            roughness,
            ..Self::default()
        }
    }
}

impl Default for MaterialDesc {
    /// Grey (0.8) dielectric, roughness 0.5, opaque, single-sided, no textures.
    fn default() -> Self {
        Self {
            base_color: [0.8, 0.8, 0.8, 1.0],
            base_color_texture: None,
            metallic: 0.0,
            roughness: 0.5,
            metallic_roughness_texture: None,
            normal_texture: None,
            normal_scale: 1.0,
            occlusion_texture: None,
            occlusion_strength: 1.0,
            emissive: [0.0; 3],
            emissive_texture: None,
            alpha: AlphaMode::Opaque,
            double_sided: false,
            subsurface: None,
        }
    }
}

/// A material type (ADR 0092): [`MaterialTypeId::GLTF`], the built-in glTF material, or one
/// registered with the forward renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct MaterialTypeId(pub(crate) u16);

impl MaterialTypeId {
    /// The built-in glTF 2.0 metallic-roughness material.
    pub const GLTF: Self = Self(0);

    /// Its index (0 for glTF's; registered types count from 1).
    pub fn index(self) -> u16 {
        self.0
    }
}

/// Bytes of a material type's parameters: each material slot has this many in the
/// parameters' buffer (`material_params<T>` in `forward.slang`).
pub const PARAMS_BYTES: usize = 256;

/// A material type's parameters: the bytes of its parameter struct (as the Slang type
/// lays them out, std430), and its textures, written at byte offsets as `uint`s (bindless
/// index in the low 24 bits, sampler in the top 8; `0xFFFFFFFF` while a texture is not
/// uploaded, as for glTF materials).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MaterialParams {
    /// The struct's bytes, at most [`PARAMS_BYTES`].
    pub bytes: Vec<u8>,
    /// Textures and the byte offsets they go to (4-byte aligned, inside `bytes`).
    pub textures: Vec<(u32, TextureRef)>,
}

/// A material in a [`MaterialTable`]. Ids of removed materials are stale: they draw with
/// the default material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct MaterialId {
    index: u32,
    generation: u32,
}

/// `Material` in `forward.slang` (80 bytes). Texture fields: bindless index in the low
/// 24 bits, sampler index in the top 8; [`NO_TEXTURE`] for a texture that is missing or
/// not uploaded yet.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(C)]
struct GpuMaterial {
    base_color: [f32; 4],
    emissive: [f32; 3],
    normal_scale: f32,
    metallic: f32,
    roughness: f32,
    occlusion_strength: f32,
    alpha_cutoff: f32,
    base_color_texture: u32,
    metallic_roughness_texture: u32,
    normal_texture: u32,
    occlusion_texture: u32,
    emissive_texture: u32,
    /// Bits 0-7: profile index + 1 (0: none); 8-15: mask (unorm8); 16-17: transmission
    /// (0 none, 1 thin, 2 thick); 18: pre- and post-scatter texturing.
    subsurface: u32,
    thickness_texture: u32,
    thickness: f32,
}

/// `NO_TEXTURE` in `forward.slang`.
const NO_TEXTURE: u32 = u32::MAX;

const MATERIAL_BYTES: u64 = size_of::<GpuMaterial>() as u64;
const _: () = assert!(MATERIAL_BYTES == 80);

/// What the forward renderer needs to know of a material to pick its pipeline state.
// Eight bytes: with the slot, `MaterialFrame::resolve` returns it in one register (it runs
// once per draw).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C, align(4))]
pub struct MaterialKind {
    /// Its type (ADR 0092).
    pub ty: MaterialTypeId,
    /// Alpha tested.
    pub masked: bool,
    /// Drawn without face culling.
    pub double_sided: bool,
    /// Blended over what is behind, in the transparent pass.
    pub blended: bool,
    /// Subsurface scattering (ADR 0059): shaded by the forward subsurface pass.
    pub subsurface: bool,
}

#[derive(Debug)]
struct Slot {
    generation: u32,
    desc: Option<MaterialDesc>,
    gpu: GpuMaterial,
    kind: MaterialKind,
    /// A material type's parameters (ADR 0092).
    params: Option<Box<MaterialParams>>,
    dirty: bool,
}

/// The materials' storage buffer.
#[derive(Debug)]
struct TableBuffer {
    buffer: vk::Buffer,
    allocation: Option<Allocation>,
    address: vk::DeviceAddress,
    /// In materials.
    capacity: u32,
    state: BufferState,
}

/// Reads by fragment shaders: the state the table is left in at the end of every frame.
const MATERIAL_READ: BufferState = BufferState {
    stages: vk::PipelineStageFlags2::FRAGMENT_SHADER,
    access: vk::AccessFlags2::SHADER_STORAGE_READ,
};

/// Every material (see the module documentation).
#[derive(Debug)]
pub struct MaterialTable {
    slots: Vec<Slot>,
    free_slots: Vec<u32>,
    dirty: Vec<u32>,
    table: Option<TableBuffer>,
    retired: Vec<(u64, vk::Buffer, Allocation)>,
    frames_in_flight: u64,
    /// The texture pool's generation the materials were last resolved against.
    resolved: Option<u64>,
    /// Edits that change how instances resolve: a material's kind changed, or a material
    /// was removed (its ids now mean the default material).
    revision: u64,
    /// The texture LOD bias and the first index of its material samplers.
    lod_bias: f32,
    samplers: u32,
    /// Material types' parameters, [`PARAMS_BYTES`] per slot: made when the first material
    /// of a type is added, as large as the table.
    params: Option<TableBuffer>,
    /// A material of a type was ever added: the parameters' buffer exists from then on.
    typed: bool,
}

impl MaterialTable {
    /// A table holding the default material; frames in flight bound how long replaced
    /// buffers are kept.
    pub fn new(frames_in_flight: usize) -> Self {
        let mut table = Self {
            slots: Vec::new(),
            free_slots: Vec::new(),
            dirty: Vec::new(),
            table: None,
            retired: Vec::new(),
            frames_in_flight: frames_in_flight.max(1) as u64,
            resolved: None,
            revision: 0,
            lod_bias: 0.0,
            samplers: Bindless::material_sampler(Wrap::Repeat, Wrap::Repeat),
            params: None,
            typed: false,
        };
        table.add(MaterialDesc::default());
        table
    }

    /// Biases the level of detail of every material texture by `bias` (negative: sharper;
    /// 0 by default). A temporal upscaler reconstructs detail finer than a render pixel,
    /// so textures should be sampled as if at the output resolution: log2(render width /
    /// output width) (`engine_upscale::texture_lod_bias`; upscalers may ask for more,
    /// `engine_fsr::Fsr::texture_lod_bias`). The bias lives in the samplers (ADR 0060):
    /// every view drawing with these materials gets it, the alpha tests of the depth and
    /// shadow passes included, and the shaders pay nothing for it. Changing it rewrites
    /// every material in the next [`Self::begin_frame`]: set it when the render or output
    /// size changes, not every frame.
    ///
    /// # Errors
    ///
    /// When a sampler cannot be created.
    pub fn set_texture_lod_bias(&mut self, bindless: &Bindless, bias: f32) -> Result<(), RhiError> {
        let samplers = bindless.material_samplers(bias)?;
        self.lod_bias = bias;
        if samplers != self.samplers {
            self.samplers = samplers;
            // Resolved again (with the other samplers) in the next frame.
            self.resolved = None;
        }
        Ok(())
    }

    /// The bias set by [`Self::set_texture_lod_bias`].
    pub fn texture_lod_bias(&self) -> f32 {
        self.lod_bias
    }

    /// Adds a material; it reaches the GPU in the next [`Self::begin_frame`].
    pub fn add(&mut self, desc: MaterialDesc) -> MaterialId {
        self.add_typed(MaterialTypeId::GLTF, desc, None)
    }

    /// Adds a material of a type (ADR 0092) with its parameters; `fallback` describes it
    /// for what does not run the type's code (its first frames, GI's ray hits) and gives
    /// its alpha mode and sides. Parameters beyond [`PARAMS_BYTES`] are cut.
    pub fn add_typed(
        &mut self,
        ty: MaterialTypeId,
        fallback: MaterialDesc,
        params: Option<MaterialParams>,
    ) -> MaterialId {
        let index = match self.free_slots.pop() {
            Some(index) => index,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    desc: None,
                    gpu: GpuMaterial::default(),
                    kind: MaterialKind::default(),
                    params: None,
                    dirty: false,
                });
                (self.slots.len() - 1) as u32
            }
        };
        let generation = self.slots[index as usize].generation;
        self.store(index, ty, fallback, params);
        MaterialId { index, generation }
    }

    /// Replaces a material's description (stale ids are ignored); it becomes a glTF
    /// material.
    pub fn set(&mut self, id: MaterialId, desc: MaterialDesc) {
        if self.get(id).is_some() {
            self.store(id.index, MaterialTypeId::GLTF, desc, None);
        }
    }

    /// Replaces a material's type, fallback and parameters (stale ids are ignored).
    pub fn set_typed(
        &mut self,
        id: MaterialId,
        ty: MaterialTypeId,
        fallback: MaterialDesc,
        params: Option<MaterialParams>,
    ) {
        if self.get(id).is_some() {
            self.store(id.index, ty, fallback, params);
        }
    }

    /// A material's type; `None` for stale ids.
    pub fn material_type(&self, id: MaterialId) -> Option<MaterialTypeId> {
        self.get(id).map(|_| self.slots[id.index as usize].kind.ty)
    }

    /// A material of a type's parameters (ADR 0092); `None` for stale ids and glTF
    /// materials.
    pub fn typed_params(&self, id: MaterialId) -> Option<&MaterialParams> {
        self.get(id)?;
        self.slots[id.index as usize].params.as_deref()
    }

    fn store(
        &mut self,
        index: u32,
        ty: MaterialTypeId,
        desc: MaterialDesc,
        params: Option<MaterialParams>,
    ) {
        let slot = &mut self.slots[index as usize];
        let old_kind = slot.desc.map(|_| slot.kind);
        slot.desc = Some(desc);
        // A type's material does no subsurface scattering (its own code shades it).
        let typed = ty != MaterialTypeId::GLTF;
        self.typed |= typed;
        slot.params = typed.then(|| {
            let mut p = params.unwrap_or_default();
            p.bytes.truncate(PARAMS_BYTES);
            Box::new(p)
        });
        slot.kind = MaterialKind {
            ty,
            masked: matches!(desc.alpha, AlphaMode::Mask { .. }),
            double_sided: desc.double_sided,
            blended: desc.alpha == AlphaMode::Blend,
            subsurface: !typed && desc.subsurface.is_some() && desc.alpha != AlphaMode::Blend,
        };
        if old_kind.is_some_and(|k| k != slot.kind) {
            self.revision += 1;
        }
        if !slot.dirty {
            slot.dirty = true;
            self.dirty.push(index);
        }
    }

    /// Counts the edits that change how a material id resolves for drawing: a material's
    /// kind changed ([`MaterialKind`]: masked, blended, double-sided, subsurface), or a
    /// material was removed. Scenes that cache resolved materials
    /// ([`crate::gpu_scene::GpuScene`]) resolve again when it changes.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Removes a material: draws that still use its id draw with the default material.
    /// The default material itself cannot be removed.
    pub fn remove(&mut self, id: MaterialId) {
        if id.index == 0 || self.get(id).is_none() {
            return;
        }
        let slot = &mut self.slots[id.index as usize];
        slot.desc = None;
        slot.params = None;
        slot.generation = slot.generation.wrapping_add(1);
        self.free_slots.push(id.index);
        self.revision += 1;
    }

    /// A material's description; `None` for stale ids.
    pub fn get(&self, id: MaterialId) -> Option<&MaterialDesc> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        slot.desc.as_ref()
    }

    /// The textures of the material in table slot `slot` (what [`crate::feedback`]
    /// reports per slot): base color, metallic-roughness, normal, occlusion, emission and
    /// subsurface thickness; `None` for missing ones and for free slots.
    pub fn slot_textures(&self, slot: u32) -> [Option<TextureId>; 6] {
        let Some(desc) = self.slots.get(slot as usize).and_then(|s| s.desc) else {
            return [None; 6];
        };
        [
            desc.base_color_texture,
            desc.metallic_roughness_texture,
            desc.normal_texture,
            desc.occlusion_texture,
            desc.emissive_texture,
            desc.subsurface.and_then(|s| s.thickness_texture),
        ]
        .map(|t| t.map(|t| t.texture))
    }

    /// The slot of a material in the table (what [`crate::feedback`] reports), `None` for
    /// stale ids.
    pub fn slot_of(&self, id: MaterialId) -> Option<u32> {
        self.get(id).map(|_| id.index)
    }

    /// The default material (slot 0).
    pub fn default_material(&self) -> MaterialId {
        MaterialId {
            index: 0,
            generation: self.slots[0].generation,
        }
    }

    /// Live materials, the default one included.
    pub fn len(&self) -> usize {
        self.slots.len() - self.free_slots.len()
    }

    /// Always false: the default material is always there.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Frees what finished frames no longer use, resolves textures that became ready or
    /// were removed, uploads changed materials, and imports the table into `fg` for the
    /// passes that shade with it. Call once per frame after the texture pool's
    /// [`TexturePool::begin_frame`] (so textures uploaded this frame are used at once)
    /// and before the passes that draw. Allocates only on frames that change
    /// materials.
    pub fn begin_frame<'g>(
        &'g mut self,
        fg: &mut FrameGraph<'g>,
        device: &Device,
        allocator: &mut Allocator,
        textures: &TexturePool,
        frame_number: u64,
    ) -> Result<MaterialFrame<'g>, RhiError> {
        engine_profiler::zone!("render::MaterialTable::begin_frame");
        let completed = frame_number.saturating_sub(self.frames_in_flight);
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].0 > completed {
                i += 1;
                continue;
            }
            let (_, buffer, allocation) = self.retired.swap_remove(i);
            // SAFETY: the frames that used it have finished.
            unsafe { allocator.destroy_buffer(buffer, allocation) };
        }

        // Grow: a new buffer for every slot; everything is uploaded again.
        let needed = self.slots.len() as u32;
        if self.table.as_ref().is_none_or(|t| t.capacity < needed) {
            let capacity = needed.next_power_of_two().max(64);
            let (buffer, allocation) = allocator.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(u64::from(capacity) * MATERIAL_BYTES)
                    .usage(
                        vk::BufferUsageFlags::STORAGE_BUFFER
                            | vk::BufferUsageFlags::TRANSFER_DST
                            | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
                    ),
                MemoryLocation::Device,
            )?;
            // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
            let address = unsafe {
                device.raw().get_buffer_device_address(
                    &vk::BufferDeviceAddressInfo::default().buffer(buffer),
                )
            };
            let old = self.table.replace(TableBuffer {
                buffer,
                allocation: Some(allocation),
                address,
                capacity,
                state: BufferState::idle(),
            });
            if let Some(mut old) = old
                && let Some(allocation) = old.allocation.take()
            {
                self.retired.push((frame_number, old.buffer, allocation));
            }
            self.dirty.clear();
            for (index, slot) in self.slots.iter_mut().enumerate() {
                slot.dirty = slot.desc.is_some();
                if slot.dirty {
                    self.dirty.push(index as u32);
                }
            }
        }

        // Material types' parameters: a buffer as large as the table, once a typed
        // material exists; every typed material uploads into a new one.
        let capacity = self.table.as_ref().map_or(0, |t| t.capacity);
        if self.typed && self.params.as_ref().is_none_or(|p| p.capacity < capacity) {
            let (buffer, allocation) = allocator.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(u64::from(capacity) * PARAMS_BYTES as u64)
                    .usage(
                        vk::BufferUsageFlags::STORAGE_BUFFER
                            | vk::BufferUsageFlags::TRANSFER_DST
                            | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
                    ),
                MemoryLocation::Device,
            )?;
            // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
            let address = unsafe {
                device.raw().get_buffer_device_address(
                    &vk::BufferDeviceAddressInfo::default().buffer(buffer),
                )
            };
            let old = self.params.replace(TableBuffer {
                buffer,
                allocation: Some(allocation),
                address,
                capacity,
                state: BufferState::idle(),
            });
            if let Some(mut old) = old
                && let Some(allocation) = old.allocation.take()
            {
                self.retired.push((frame_number, old.buffer, allocation));
            }
            for (index, slot) in self.slots.iter_mut().enumerate() {
                if slot.params.is_some() && !slot.dirty {
                    slot.dirty = true;
                    self.dirty.push(index as u32);
                }
            }
        }

        // Resolve every material again when textures changed; changed ones upload (and
        // typed ones with textures among their parameters).
        if self.resolved != Some(textures.generation()) {
            self.resolved = Some(textures.generation());
            for (index, slot) in self.slots.iter_mut().enumerate() {
                let Some(desc) = slot.desc else {
                    continue;
                };
                let gpu = resolve(&desc, textures, self.samplers);
                let params_textures = slot.params.as_ref().is_some_and(|p| !p.textures.is_empty());
                if gpu != slot.gpu || params_textures {
                    slot.gpu = gpu;
                    if !slot.dirty {
                        slot.dirty = true;
                        self.dirty.push(index as u32);
                    }
                }
            }
        }

        let Some(table) = self.table.as_mut() else {
            unreachable!("created above");
        };
        let handle = fg.import_buffer(
            "materials",
            ImportedBuffer {
                buffer: table.buffer,
                size: u64::from(table.capacity) * MATERIAL_BYTES,
                address: table.address,
                initial: table.state,
                final_state: Some(MATERIAL_READ),
            },
        );
        table.state = MATERIAL_READ;
        let params = self.params.as_mut().map(|p| {
            let handle = fg.import_buffer(
                "material parameters",
                ImportedBuffer {
                    buffer: p.buffer,
                    size: u64::from(p.capacity) * PARAMS_BYTES as u64,
                    address: p.address,
                    initial: p.state,
                    final_state: Some(MATERIAL_READ),
                },
            );
            p.state = MATERIAL_READ;
            handle
        });
        if !self.dirty.is_empty() {
            self.upload(fg, allocator, textures, handle, params, frame_number)?;
        }
        Ok(MaterialFrame {
            table: self,
            buffer: handle,
            params,
        })
    }

    /// Copies the dirty materials into the table through a staging buffer freed with the
    /// frame.
    fn upload(
        &mut self,
        fg: &mut FrameGraph<'_>,
        allocator: &mut Allocator,
        textures: &TexturePool,
        target: Buffer,
        params_target: Option<Buffer>,
        frame_number: u64,
    ) -> Result<(), RhiError> {
        self.dirty.sort_unstable();
        self.dirty.dedup();
        let count = self.dirty.len() as u64;
        // The typed materials' parameters follow the records in the staging buffer.
        let typed: Vec<u32> = match params_target {
            Some(_) => self
                .dirty
                .iter()
                .copied()
                .filter(|&i| self.slots[i as usize].params.is_some())
                .collect(),
            None => Vec::new(),
        };
        let params_offset = count * MATERIAL_BYTES;
        let (staging, mut memory) = allocator.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(params_offset + typed.len() as u64 * PARAMS_BYTES as u64)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC),
            MemoryLocation::Upload,
        )?;
        // Runs of consecutive slots become one copy each.
        let mut regions: Vec<vk::BufferCopy> = Vec::new();
        let mut param_regions: Vec<vk::BufferCopy> = Vec::with_capacity(typed.len());
        // `typed` lists the dirty typed slots in the dirty slots' order.
        let mut next_typed = 0usize;
        {
            let Some(bytes) = memory.mapped_slice_mut() else {
                // SAFETY: never used.
                unsafe { allocator.destroy_buffer(staging, memory) };
                return Err(RhiError::Vulkan {
                    call: "staging buffer not mapped",
                    result: vk::Result::ERROR_MEMORY_MAP_FAILED,
                });
            };
            for (k, &index) in self.dirty.iter().enumerate() {
                let slot = &mut self.slots[index as usize];
                slot.dirty = false;
                if let Some(desc) = slot.desc {
                    slot.gpu = resolve(&desc, textures, self.samplers);
                }
                let src = k as u64 * MATERIAL_BYTES;
                // SAFETY: `GpuMaterial` is plain `repr(C)` data without padding bytes
                // (asserted size, all fields 4-byte scalars).
                let data = unsafe {
                    core::slice::from_raw_parts(
                        (&slot.gpu as *const GpuMaterial).cast::<u8>(),
                        MATERIAL_BYTES as usize,
                    )
                };
                bytes[src as usize..(src + MATERIAL_BYTES) as usize].copy_from_slice(data);
                if typed.get(next_typed) == Some(&index)
                    && let Some(params) = slot.params.as_deref()
                {
                    let k = next_typed;
                    next_typed += 1;
                    let src = params_offset + k as u64 * PARAMS_BYTES as u64;
                    let out = &mut bytes[src as usize..src as usize + PARAMS_BYTES];
                    out.fill(0);
                    out[..params.bytes.len()].copy_from_slice(&params.bytes);
                    for &(offset, texture) in &params.textures {
                        let offset = offset as usize & !3;
                        if offset + 4 > PARAMS_BYTES {
                            continue;
                        }
                        let value = textures.index(texture.texture).map_or(NO_TEXTURE, |i| {
                            i | (Bindless::material_sampler_in(
                                self.samplers,
                                texture.wrap[0],
                                texture.wrap[1],
                            ) << 24)
                        });
                        out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
                    }
                    param_regions.push(vk::BufferCopy {
                        src_offset: src,
                        dst_offset: u64::from(index) * PARAMS_BYTES as u64,
                        size: PARAMS_BYTES as u64,
                    });
                }
                let dst = u64::from(index) * MATERIAL_BYTES;
                match regions.last_mut() {
                    Some(r) if r.src_offset + r.size == src && r.dst_offset + r.size == dst => {
                        r.size += MATERIAL_BYTES;
                    }
                    _ => regions.push(vk::BufferCopy {
                        src_offset: src,
                        dst_offset: dst,
                        size: MATERIAL_BYTES,
                    }),
                }
            }
        }
        self.dirty.clear();
        let source = fg.import_buffer(
            "material staging",
            ImportedBuffer {
                buffer: staging,
                size: params_offset + typed.len() as u64 * PARAMS_BYTES as u64,
                address: 0,
                initial: BufferState::idle(),
                final_state: None,
            },
        );
        let mut pass = fg
            .add_pass("material upload")
            .read(source, Access::TransferRead)
            .write(target, Access::TransferWrite);
        if let Some(params) = params_target.filter(|_| !param_regions.is_empty()) {
            pass = pass.write(params, Access::TransferWrite);
        }
        pass.execute(move |ctx| {
            // SAFETY: recording outside a rendering scope; every region lies inside both
            // buffers (slots are below the table's capacity, and the parameters' buffer is
            // as large).
            unsafe {
                ctx.device.cmd_copy_buffer(
                    ctx.cmd,
                    ctx.buffer(source),
                    ctx.buffer(target),
                    &regions,
                );
                if let Some(params) = params_target.filter(|_| !param_regions.is_empty()) {
                    ctx.device.cmd_copy_buffer(
                        ctx.cmd,
                        ctx.buffer(source),
                        ctx.buffer(params),
                        &param_regions,
                    );
                }
            }
        });
        self.retired.push((frame_number, staging, memory));
        Ok(())
    }

    /// Frees the table.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses it (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if let Some(mut table) = self.table.take()
                && let Some(allocation) = table.allocation.take()
            {
                allocator.destroy_buffer(table.buffer, allocation);
            }
            if let Some(mut params) = self.params.take()
                && let Some(allocation) = params.allocation.take()
            {
                allocator.destroy_buffer(params.buffer, allocation);
            }
            for (_, buffer, allocation) in self.retired.drain(..) {
                allocator.destroy_buffer(buffer, allocation);
            }
        }
    }
}

/// A material's GPU form: its textures' bindless indices with their samplers
/// ([`NO_TEXTURE`] until they are uploaded).
fn resolve(desc: &MaterialDesc, textures: &TexturePool, samplers: u32) -> GpuMaterial {
    let texture = |r: Option<TextureRef>| {
        r.and_then(|r| {
            let index = textures.index(r.texture)?;
            Some(index | (Bindless::material_sampler_in(samplers, r.wrap[0], r.wrap[1]) << 24))
        })
        .unwrap_or(NO_TEXTURE)
    };
    GpuMaterial {
        base_color: desc.base_color,
        emissive: desc.emissive,
        normal_scale: desc.normal_scale,
        metallic: desc.metallic.clamp(0.0, 1.0),
        roughness: desc.roughness.clamp(0.0, 1.0),
        occlusion_strength: desc.occlusion_strength.clamp(0.0, 1.0),
        alpha_cutoff: match desc.alpha {
            AlphaMode::Opaque | AlphaMode::Blend => 0.0,
            AlphaMode::Mask { cutoff } => cutoff,
        },
        base_color_texture: texture(desc.base_color_texture),
        metallic_roughness_texture: texture(desc.metallic_roughness_texture),
        normal_texture: texture(desc.normal_texture),
        occlusion_texture: texture(desc.occlusion_texture),
        emissive_texture: texture(desc.emissive_texture),
        subsurface: desc
            .subsurface
            .filter(|_| desc.alpha != AlphaMode::Blend)
            .map_or(0, |s| {
                let mask = (s.mask.clamp(0.0, 1.0) * 255.0).round() as u32;
                let transmission = match s.transmission {
                    Transmission::None => 0,
                    Transmission::Thin => 1,
                    Transmission::Thick => 2,
                };
                let texturing = u32::from(s.texturing == SubsurfaceTexturing::PreAndPostScatter);
                (u32::from(s.profile) + 1).min(255)
                    | mask << 8
                    | transmission << 16
                    | texturing << 18
            }),
        thickness_texture: texture(desc.subsurface.and_then(|s| s.thickness_texture)),
        thickness: desc.subsurface.map_or(0.0, |s| s.thickness.max(0.0)),
    }
}

/// The material table in one frame's graph, for the passes that shade with it.
#[derive(Debug, Clone, Copy)]
pub struct MaterialFrame<'g> {
    table: &'g MaterialTable,
    /// The table: `Material` structs (80 bytes each), indexed by slot.
    pub buffer: Buffer,
    /// Material types' parameters ([`PARAMS_BYTES`] per slot), once a typed material exists.
    pub params: Option<Buffer>,
}

impl MaterialFrame<'_> {
    /// A material's slot in the table and its kind; the default material (slot 0) for
    /// stale ids.
    #[inline]
    pub fn resolve(&self, id: MaterialId) -> (u32, MaterialKind) {
        match self.table.slots.get(id.index as usize) {
            Some(slot) if slot.generation == id.generation && slot.desc.is_some() => {
                (id.index, slot.kind)
            }
            _ => (0, self.table.slots[0].kind),
        }
    }

    /// Material slots the table's buffer holds (slots index it; every live material's is
    /// below this).
    pub fn capacity(&self) -> u32 {
        self.table.table.as_ref().map_or(0, |t| t.capacity)
    }

    /// The table's [`MaterialTable::revision`].
    pub fn revision(&self) -> u64 {
        self.table.revision
    }

    /// Device address of the table.
    pub fn address(&self) -> vk::DeviceAddress {
        self.table.table.as_ref().map_or(0, |t| t.address)
    }

    /// Device address of the material types' parameters and the bytes per slot (0 and 0
    /// without typed materials).
    pub fn params(&self) -> (vk::DeviceAddress, u32) {
        match (&self.table.params, self.params) {
            (Some(p), Some(_)) => (p.address, PARAMS_BYTES as u32),
            _ => (0, 0),
        }
    }

    /// Declares that a pass's fragment shaders read the table (and the types' parameters).
    pub fn read<'a, 'g>(
        &self,
        pass: crate::graph::PassBuilder<'a, 'g>,
    ) -> crate::graph::PassBuilder<'a, 'g> {
        let pass = pass.read(self.buffer, Access::StorageRead(ShaderStages::FRAGMENT));
        match self.params {
            Some(params) => pass.read(params, Access::StorageRead(ShaderStages::FRAGMENT)),
            None => pass,
        }
    }
}
