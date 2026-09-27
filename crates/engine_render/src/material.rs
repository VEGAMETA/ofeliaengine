//! Materials (ADR 0040): glTF 2.0's metallic-roughness model, every material in one GPU
//! table that shaders index per instance. No descriptor sets per material and no
//! rebinding between draws: a material is 80 bytes of factors and texture indices, and a
//! texture index carries its sampler (the wrap modes) in its top 8 bits.
//!
//! Textures come from a [`TexturePool`]. A missing texture is the same as white, so its
//! factor applies unchanged; until a texture is uploaded, materials sample the pool's
//! white texture of its kind instead (without a normal map, shading uses the
//! interpolated normal). [`MaterialTable::begin_frame`] resolves materials again when
//! textures become ready or are removed.
//!
//! Slot 0 is the default material (grey, dielectric): stale ids draw with it.

use crate::graph::{Access, Buffer, BufferState, FrameGraph, ImportedBuffer, ShaderStages};
use crate::texture_pool::{DefaultTexture, TextureId, TexturePool};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError, Wrap};

/// How a material's alpha is used. Blending (glTF's `BLEND`) comes with the transparency
/// pass.
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
        }
    }
}

/// A material in a [`MaterialTable`]. Ids of removed materials are stale: they draw with
/// the default material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaterialId {
    index: u32,
    generation: u32,
}

/// `Material` in `forward.slang` (80 bytes). Texture fields: bindless index in the low
/// 24 bits, sampler index in the top 8; [`NO_TEXTURE`] for a normal map that is missing
/// or not uploaded yet.
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
    _pad: [u32; 3],
}

/// `NO_TEXTURE` in `forward.slang`.
const NO_TEXTURE: u32 = u32::MAX;

const MATERIAL_BYTES: u64 = size_of::<GpuMaterial>() as u64;
const _: () = assert!(MATERIAL_BYTES == 80);

/// What the forward renderer needs to know of a material to pick its pipeline state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MaterialKind {
    /// Alpha tested.
    pub masked: bool,
    /// Drawn without face culling.
    pub double_sided: bool,
}

#[derive(Debug)]
struct Slot {
    generation: u32,
    desc: Option<MaterialDesc>,
    gpu: GpuMaterial,
    kind: MaterialKind,
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
        };
        table.add(MaterialDesc::default());
        table
    }

    /// Adds a material; it reaches the GPU in the next [`Self::begin_frame`].
    pub fn add(&mut self, desc: MaterialDesc) -> MaterialId {
        let index = match self.free_slots.pop() {
            Some(index) => index,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    desc: None,
                    gpu: GpuMaterial::default(),
                    kind: MaterialKind::default(),
                    dirty: false,
                });
                (self.slots.len() - 1) as u32
            }
        };
        let generation = self.slots[index as usize].generation;
        self.store(index, desc);
        MaterialId { index, generation }
    }

    /// Replaces a material's description (stale ids are ignored).
    pub fn set(&mut self, id: MaterialId, desc: MaterialDesc) {
        if self.get(id).is_some() {
            self.store(id.index, desc);
        }
    }

    fn store(&mut self, index: u32, desc: MaterialDesc) {
        let slot = &mut self.slots[index as usize];
        slot.desc = Some(desc);
        slot.kind = MaterialKind {
            masked: matches!(desc.alpha, AlphaMode::Mask { .. }),
            double_sided: desc.double_sided,
        };
        if !slot.dirty {
            slot.dirty = true;
            self.dirty.push(index);
        }
    }

    /// Removes a material: draws that still use its id draw with the default material.
    /// The default material itself cannot be removed.
    pub fn remove(&mut self, id: MaterialId) {
        if id.index == 0 || self.get(id).is_none() {
            return;
        }
        let slot = &mut self.slots[id.index as usize];
        slot.desc = None;
        slot.generation = slot.generation.wrapping_add(1);
        self.free_slots.push(id.index);
    }

    /// A material's description; `None` for stale ids.
    pub fn get(&self, id: MaterialId) -> Option<&MaterialDesc> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        slot.desc.as_ref()
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
    /// [`TexturePool::begin_frame`] and before the passes that draw. Allocates only on
    /// frames that change materials.
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

        // Resolve every material again when textures changed; changed ones upload.
        if self.resolved != Some(textures.generation()) {
            self.resolved = Some(textures.generation());
            for (index, slot) in self.slots.iter_mut().enumerate() {
                let Some(desc) = slot.desc else {
                    continue;
                };
                let gpu = resolve(&desc, textures);
                if gpu != slot.gpu {
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
        if !self.dirty.is_empty() {
            self.upload(fg, allocator, textures, handle, frame_number)?;
        }
        Ok(MaterialFrame {
            table: self,
            buffer: handle,
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
        frame_number: u64,
    ) -> Result<(), RhiError> {
        self.dirty.sort_unstable();
        self.dirty.dedup();
        let count = self.dirty.len() as u64;
        let (staging, mut memory) = allocator.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(count * MATERIAL_BYTES)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC),
            MemoryLocation::Upload,
        )?;
        // Runs of consecutive slots become one copy each.
        let mut regions: Vec<vk::BufferCopy> = Vec::new();
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
                    slot.gpu = resolve(&desc, textures);
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
                size: count * MATERIAL_BYTES,
                address: 0,
                initial: BufferState::idle(),
                final_state: None,
            },
        );
        fg.add_pass("material upload")
            .read(source, Access::TransferRead)
            .write(target, Access::TransferWrite)
            .execute(move |ctx| {
                // SAFETY: recording outside a rendering scope; every region lies inside
                // both buffers (slots are below the table's capacity).
                unsafe {
                    ctx.device.cmd_copy_buffer(
                        ctx.cmd,
                        ctx.buffer(source),
                        ctx.buffer(target),
                        &regions,
                    );
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
            for (_, buffer, allocation) in self.retired.drain(..) {
                allocator.destroy_buffer(buffer, allocation);
            }
        }
    }
}

/// A material's GPU form: its textures' bindless indices (the defaults until they are
/// uploaded) with their samplers.
fn resolve(desc: &MaterialDesc, textures: &TexturePool) -> GpuMaterial {
    let texture = |r: Option<TextureRef>, fallback: DefaultTexture| {
        let (index, wrap) = r
            .and_then(|r| textures.index(r.texture).map(|index| (index, r.wrap)))
            .unwrap_or((textures.default_index(fallback), [Wrap::Repeat; 2]));
        index | (Bindless::material_sampler(wrap[0], wrap[1]) << 24)
    };
    GpuMaterial {
        base_color: desc.base_color,
        emissive: desc.emissive,
        normal_scale: desc.normal_scale,
        metallic: desc.metallic.clamp(0.0, 1.0),
        roughness: desc.roughness.clamp(0.0, 1.0),
        occlusion_strength: desc.occlusion_strength.clamp(0.0, 1.0),
        alpha_cutoff: match desc.alpha {
            AlphaMode::Opaque => 0.0,
            AlphaMode::Mask { cutoff } => cutoff,
        },
        base_color_texture: texture(desc.base_color_texture, DefaultTexture::White),
        metallic_roughness_texture: texture(
            desc.metallic_roughness_texture,
            DefaultTexture::LinearWhite,
        ),
        normal_texture: desc
            .normal_texture
            .and_then(|r| {
                let index = textures.index(r.texture)?;
                Some(index | (Bindless::material_sampler(r.wrap[0], r.wrap[1]) << 24))
            })
            .unwrap_or(NO_TEXTURE),
        occlusion_texture: texture(desc.occlusion_texture, DefaultTexture::LinearWhite),
        emissive_texture: texture(desc.emissive_texture, DefaultTexture::White),
        _pad: [0; 3],
    }
}

/// The material table in one frame's graph, for the passes that shade with it.
#[derive(Debug, Clone, Copy)]
pub struct MaterialFrame<'g> {
    table: &'g MaterialTable,
    /// The table: `Material` structs (80 bytes each), indexed by slot.
    pub buffer: Buffer,
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

    /// Device address of the table.
    pub fn address(&self) -> vk::DeviceAddress {
        self.table.table.as_ref().map_or(0, |t| t.address)
    }

    /// Declares that a pass's fragment shaders read the table.
    pub fn read<'a, 'g>(
        &self,
        pass: crate::graph::PassBuilder<'a, 'g>,
    ) -> crate::graph::PassBuilder<'a, 'g> {
        pass.read(self.buffer, Access::StorageRead(ShaderStages::FRAGMENT))
    }
}
