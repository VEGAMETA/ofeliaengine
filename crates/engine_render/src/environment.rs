//! Image-based lighting (ADR 0046, docs/research/image-based-lighting.md): light from the
//! whole sky, for the ambient term of the forward renderer and its background.
//!
//! An [`Environment`] keeps, across frames:
//!
//! - the sky's radiance in a cube map with its mip chain;
//! - that radiance prefiltered with the GGX lobe, one roughness per mip (split sum, Karis
//!   2013);
//! - 9 spherical-harmonic coefficients of the irradiance (diffuse light for any normal);
//! - the split sum's DFG table (computed once).
//!
//! They are rebuilt on the GPU only when the sky changes ([`Environment::set_sky`]), in
//! compute passes of that frame's graph. The forward renderer reads them when a view has an
//! environment ([`crate::forward::ForwardView::environment`]).

use crate::atmosphere::{Atmosphere, AtmosphereFrame, AtmosphereTables, AtmosphereView, HeightFog};
use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::graph::{
    Access, Buffer, BufferState, FrameGraph, ImportedBuffer, ImportedTexture, ShaderStages,
    Texture, TextureDesc, TextureState,
};
use crate::upload::UploadRing;
use core::cell::Cell;
use engine_core::math::Vec3;
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError, SamplerKind};

const SKY_CUBE: &[u8] = include_bytes!("../shaders/environment.skyCube.spv");
const DOWNSAMPLE: &[u8] = include_bytes!("../shaders/environment.downsample.spv");
const PREFILTER: &[u8] = include_bytes!("../shaders/environment.prefilter.spv");
const IRRADIANCE: &[u8] = include_bytes!("../shaders/environment.irradiance.spv");
const DFG: &[u8] = include_bytes!("../shaders/environment.dfg.spv");

/// Format of the cube maps.
pub const CUBE_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// Face size of the radiance cube (mip 0); its mips go down to 1 × 1.
pub const SOURCE_SIZE: u32 = 128;
const SOURCE_LEVELS: u32 = SOURCE_SIZE.ilog2() + 1;
/// Face size of the prefiltered cube's mip 0 (roughness 0).
pub const PREFILTERED_SIZE: u32 = 128;
/// Mips of the prefiltered cube: perceptual roughness `mip / (levels − 1)`.
pub const PREFILTERED_LEVELS: u32 = 6;
/// Side of the DFG table (n·v × perceptual roughness).
pub const DFG_SIZE: u32 = 64;
const DFG_SAMPLES: u32 = 512;
const PREFILTER_SAMPLES: u32 = 64;
/// Mip of the radiance cube the harmonics are projected from (16 × 16 faces).
const IRRADIANCE_LEVEL: u32 = 3;

/// A sky of three colors (radiance, linear): the zenith, the horizon and the ground.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sky {
    /// Straight up.
    pub zenith: Vec3,
    /// At the horizon, where the zenith's color ends.
    pub horizon: Vec3,
    /// Below the horizon (a few degrees down and further).
    pub ground: Vec3,
}

impl Default for Sky {
    /// A clear day's sky around a sun of intensity 3.
    fn default() -> Self {
        Self {
            zenith: Vec3::new(0.12, 0.25, 0.6),
            horizon: Vec3::new(0.55, 0.62, 0.72),
            ground: Vec3::new(0.12, 0.11, 0.1),
        }
    }
}

impl Sky {
    /// The same radiance from every direction (for tests: a white furnace).
    pub fn uniform(radiance: Vec3) -> Self {
        Self {
            zenith: radiance,
            horizon: radiance,
            ground: radiance,
        }
    }
}

/// `Push` in `environment.slang` (96 bytes).
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Push {
    zenith: [f32; 4],
    horizon: [f32; 4],
    ground: [f32; 4],
    sh: u64,
    source: u32,
    target: u32,
    read: u32,
    sampler: u32,
    size: u32,
    level: u32,
    levels: u32,
    source_size: u32,
    samples: u32,
    mode: u32,
}

/// The shape of an [`Image`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageKind {
    /// A 2D image.
    Flat,
    /// Six square faces, sampled as a cube.
    Cube,
    /// A 3D image of this depth.
    Volume(u32),
    /// This many cubes (six faces each), sampled as a cube array (reflection probes,
    /// ADR 0073).
    CubeArray(u32),
}

/// An image owned by the environment (or its atmosphere): sampled through one view,
/// written through one storage view per mip.
#[derive(Debug)]
pub(crate) struct Image {
    image: vk::Image,
    allocation: Option<Allocation>,
    view: vk::ImageView,
    /// Bindless index of the sampled view.
    pub(crate) sampled: u32,
    mip_views: Vec<vk::ImageView>,
    /// Bindless indices of the storage views, one per mip.
    pub(crate) storage: Vec<u32>,
    desc: TextureDesc,
    state: Cell<TextureState>,
}

impl Image {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        format: vk::Format,
        width: u32,
        height: u32,
        levels: u32,
        kind: ImageKind,
    ) -> Result<Self, RhiError> {
        let cube = matches!(kind, ImageKind::Cube | ImageKind::CubeArray(_));
        let layers = match kind {
            ImageKind::Cube => 6,
            ImageKind::CubeArray(count) => 6 * count.max(1),
            _ => 1,
        };
        let (image_type, depth) = match kind {
            ImageKind::Volume(depth) => (vk::ImageType::TYPE_3D, depth),
            _ => (vk::ImageType::TYPE_2D, 1),
        };
        let (image, allocation) = allocator.create_image(
            &vk::ImageCreateInfo::default()
                .flags(if cube {
                    vk::ImageCreateFlags::CUBE_COMPATIBLE
                } else {
                    vk::ImageCreateFlags::empty()
                })
                .image_type(image_type)
                .format(format)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth,
                })
                .mip_levels(levels)
                .array_layers(layers)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(
                    vk::ImageUsageFlags::SAMPLED
                        | vk::ImageUsageFlags::STORAGE
                        | vk::ImageUsageFlags::TRANSFER_SRC,
                )
                .initial_layout(vk::ImageLayout::UNDEFINED),
            MemoryLocation::Device,
        )?;
        let mut this = Self {
            image,
            allocation: Some(allocation),
            view: vk::ImageView::null(),
            sampled: 0,
            mip_views: Vec::with_capacity(levels as usize),
            storage: Vec::with_capacity(levels as usize),
            desc: TextureDesc {
                mip_levels: levels,
                layers,
                ..TextureDesc::new_2d(format, width, height)
            },
            state: Cell::new(TextureState::undefined()),
        };
        let view = |kind, base: u32, count: u32| {
            // SAFETY: a view of the image just created.
            unsafe {
                device.raw().create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(kind)
                        .format(format)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: base,
                            level_count: count,
                            base_array_layer: 0,
                            layer_count: layers,
                        }),
                    None,
                )
            }
            .map_err(RhiError::vk("vkCreateImageView"))
        };
        let full = || RhiError::Vulkan {
            call: "bindless set full",
            result: vk::Result::ERROR_OUT_OF_POOL_MEMORY,
        };
        let (sampled_kind, storage_kind) = match kind {
            ImageKind::Cube => (vk::ImageViewType::CUBE, vk::ImageViewType::TYPE_2D_ARRAY),
            ImageKind::CubeArray(_) => (
                vk::ImageViewType::CUBE_ARRAY,
                vk::ImageViewType::TYPE_2D_ARRAY,
            ),
            ImageKind::Flat => (vk::ImageViewType::TYPE_2D, vk::ImageViewType::TYPE_2D),
            ImageKind::Volume(_) => (vk::ImageViewType::TYPE_3D, vk::ImageViewType::TYPE_3D),
        };
        // Built step by step so a failure frees what exists (`destroy` skips null views).
        let result = (|| -> Result<(), RhiError> {
            this.view = view(sampled_kind, 0, levels)?;
            this.sampled = bindless.add_sampled_image(this.view).ok_or_else(full)?;
            for level in 0..levels {
                let v = view(storage_kind, level, 1)?;
                this.mip_views.push(v);
                this.storage
                    .push(bindless.add_storage_image(v).ok_or_else(full)?);
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(this),
            Err(e) => {
                // SAFETY: never used by the GPU.
                unsafe { this.destroy(device.raw(), allocator, bindless, 0) };
                Err(e)
            }
        }
    }

    /// Imports the image into `fg`, left readable by any shader.
    pub(crate) fn import<'g>(&self, fg: &mut FrameGraph<'g>, name: &'static str) -> Texture {
        let final_state = TextureState::shader_read(ShaderStages::ALL);
        let texture = fg.import_texture(
            name,
            ImportedTexture {
                image: self.image,
                view: self.view,
                desc: self.desc,
                initial: self.state.get(),
                final_state: Some(final_state),
                sampled_index: Some(self.sampled),
                storage_index: None,
            },
        );
        self.state.set(final_state);
        texture
    }

    /// The state the next import starts from.
    pub(crate) fn state(&self) -> TextureState {
        self.state.get()
    }

    /// Sets the state the next import starts from: what it was before an import whose
    /// frame did not use the image (the graph leaves an unused import as it was).
    pub(crate) fn set_state(&self, state: TextureState) {
        self.state.set(state);
    }

    /// # Safety
    ///
    /// The GPU no longer uses the image after `frame`.
    pub(crate) unsafe fn destroy(
        &mut self,
        device: &engine_rhi::ash::Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        frame: u64,
    ) {
        if self.view != vk::ImageView::null() {
            bindless.remove_sampled_image(self.sampled, frame);
        }
        for &index in &self.storage {
            bindless.remove_storage_image(index, frame);
        }
        // SAFETY: guaranteed by the caller.
        unsafe {
            for view in self.mip_views.drain(..).chain([self.view]) {
                if view != vk::ImageView::null() {
                    device.destroy_image_view(view, None);
                }
            }
            if let Some(allocation) = self.allocation.take() {
                allocator.destroy_image(self.image, allocation);
            }
        }
        self.view = vk::ImageView::null();
    }
}

/// What a frame's shading reads from its environment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EnvironmentFrame {
    /// The radiance cube (the background).
    pub source: Texture,
    /// The prefiltered cube (specular).
    pub prefiltered: Texture,
    /// The DFG table.
    pub dfg: Texture,
    /// The irradiance harmonics (9 × 16 bytes).
    pub sh: Buffer,
    /// Bindless indices of the three textures, and the harmonics' address.
    pub source_index: u32,
    pub prefiltered_index: u32,
    pub dfg_index: u32,
    pub sh_address: u64,
    /// Radiance multiplier.
    pub intensity: f32,
    /// The atmosphere and height fog, when the environment has either (ADR 0069): the
    /// background, aerial perspective and fog are then drawn by the atmosphere's
    /// full-screen pass, and transparent surfaces use their atmosphere pipelines.
    pub atmosphere: Option<AtmosphereFrame>,
    /// The directional light's color this frame when the atmosphere drives it.
    pub sun_color: Option<Vec3>,
    /// Whether this frame rebuilt the image-based light (the sky changed): reflection
    /// probes capture it (ADR 0073).
    pub rebuilt: bool,
}

/// Image-based lighting from a sky (see the module documentation). Rebuilding after
/// [`Self::set_sky`] costs one frame a few compute passes. A frame's graph imports it once
/// per view that uses it, so give each view of a frame its own environment for now (split
/// screen with one shared environment is a follow-up).
pub struct Environment {
    device: engine_rhi::ash::Device,
    /// Sky into the cube, downsample, prefilter, irradiance, DFG table.
    shaders: [ComputeShader<Push>; 5],
    source: Image,
    prefiltered: Image,
    dfg: Image,
    sh: vk::Buffer,
    sh_allocation: Option<Allocation>,
    sh_address: vk::DeviceAddress,
    sh_state: Cell<BufferState>,
    sky: Cell<Sky>,
    intensity: Cell<f32>,
    debug_directions: Cell<bool>,
    dirty: Cell<bool>,
    dfg_ready: Cell<bool>,
    /// The physical sky (ADR 0069), when on; its tables once it was first turned on.
    atmosphere: Cell<Option<Atmosphere>>,
    tables: Option<AtmosphereTables>,
    fog: Cell<Option<HeightFog>>,
}

impl core::fmt::Debug for Environment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Environment")
            .field("sky", &self.sky.get())
            .field("intensity", &self.intensity.get())
            .field("atmosphere", &self.atmosphere.get())
            .field("fog", &self.fog.get())
            .finish_non_exhaustive()
    }
}

impl Environment {
    /// Creates the environment's images and pipelines; the images are filled by the first
    /// frame that uses it.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        sky: Sky,
    ) -> Result<Self, RhiError> {
        let source = |spirv: &[u8], entry| ComputeSource::new(spirv, entry, &[]);
        let shaders = ComputeSource::create_all(
            device,
            bindless,
            [
                source(SKY_CUBE, c"skyCube")?,
                source(DOWNSAMPLE, c"downsample")?,
                source(PREFILTER, c"prefilter")?,
                source(IRRADIANCE, c"irradiance")?,
                source(DFG, c"dfg")?,
            ],
        )?;
        let source = Image::new(
            device,
            allocator,
            bindless,
            CUBE_FORMAT,
            SOURCE_SIZE,
            SOURCE_SIZE,
            SOURCE_LEVELS,
            ImageKind::Cube,
        )?;
        let prefiltered = Image::new(
            device,
            allocator,
            bindless,
            CUBE_FORMAT,
            PREFILTERED_SIZE,
            PREFILTERED_SIZE,
            PREFILTERED_LEVELS,
            ImageKind::Cube,
        )?;
        let dfg = Image::new(
            device,
            allocator,
            bindless,
            vk::Format::R16G16_SFLOAT,
            DFG_SIZE,
            DFG_SIZE,
            1,
            ImageKind::Flat,
        )?;
        let (sh, sh_allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default().size(9 * 16).usage(
                vk::BufferUsageFlags::STORAGE_BUFFER
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS
                    | vk::BufferUsageFlags::TRANSFER_SRC,
            ),
            MemoryLocation::Device,
        )?;
        // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
        let sh_address = unsafe {
            device
                .raw()
                .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(sh))
        };
        Ok(Self {
            device: device.raw().clone(),
            shaders,
            source,
            prefiltered,
            dfg,
            sh,
            sh_allocation: Some(sh_allocation),
            sh_address,
            sh_state: Cell::new(BufferState::idle()),
            sky: Cell::new(sky),
            intensity: Cell::new(1.0),
            debug_directions: Cell::new(false),
            dirty: Cell::new(true),
            dfg_ready: Cell::new(false),
            atmosphere: Cell::new(None),
            tables: None,
            fog: Cell::new(None),
        })
    }

    /// Changes the sky; the next frame rebuilds the lighting from it. With an atmosphere
    /// ([`Self::set_atmosphere`]) the gradient is kept for when it is turned off.
    pub fn set_sky(&self, sky: Sky) {
        if sky != self.sky.get() {
            self.sky.set(sky);
            self.dirty
                .set(self.dirty.get() || self.atmosphere.get().is_none());
        }
    }

    /// Turns the physical sky and atmosphere on with `atmosphere`, or off (`None`, back to
    /// the gradient [`Sky`]); see [`crate::atmosphere`]. The first time it is turned on its
    /// tables and passes are made (four small images, five compute pipelines), so an
    /// environment that never uses it pays nothing. Changing the parameters rebuilds the
    /// tables and the image-based light in the next frame; moving the sun (the directional
    /// light) rebuilds the sky-view table, and the image-based light once it moved by 0.1°.
    pub fn set_atmosphere(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        atmosphere: Option<Atmosphere>,
    ) -> Result<(), RhiError> {
        if atmosphere.is_some() && self.tables.is_none() {
            self.tables = Some(AtmosphereTables::new(device, allocator, bindless)?);
        }
        if atmosphere.is_none() && self.atmosphere.get().is_some() {
            // Back to the gradient.
            self.dirty.set(true);
        }
        if atmosphere.is_some()
            && let Some(tables) = &self.tables
        {
            tables.forget_cube();
        }
        self.atmosphere.set(atmosphere);
        Ok(())
    }

    /// The physical sky's parameters, when it is on.
    pub fn atmosphere(&self) -> Option<Atmosphere> {
        self.atmosphere.get()
    }

    /// Turns exponential height fog on with `fog`, or off (`None`, the default). It is lit
    /// by this environment's sky light and the directional light, and drawn with the sky by
    /// the atmosphere's full-screen pass, with or without the physical sky.
    pub fn set_fog(&self, fog: Option<HeightFog>) {
        self.fog.set(fog);
    }

    /// The height fog, when it is on.
    pub fn fog(&self) -> Option<HeightFog> {
        self.fog.get()
    }

    /// The sky.
    pub fn sky(&self) -> Sky {
        self.sky.get()
    }

    /// Scales everything the environment contributes (ambient light and background).
    pub fn set_intensity(&self, intensity: f32) {
        self.intensity.set(intensity.max(0.0));
    }

    /// The radiance multiplier.
    pub fn intensity(&self) -> f32 {
        self.intensity.get()
    }

    /// Debugging: the sky's color becomes its direction (`d × 0.5 + 0.5`), to check how
    /// the cube maps are oriented.
    pub fn set_debug_directions(&self, enabled: bool) {
        if enabled != self.debug_directions.get() {
            self.debug_directions.set(enabled);
            self.dirty.set(true);
        }
    }

    /// Imports the environment into `fg`, first declaring the passes that rebuild it if
    /// the sky changed (and the DFG table on the first frame), and with an atmosphere or
    /// fog those of the frame's atmosphere (`view`: the camera and the light), writing its
    /// constants into `ring`. The frame must execute with `bindless`. `None` when `ring`
    /// is full.
    pub(crate) fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        ring: &mut UploadRing,
        bindless: &'g Bindless,
        view: &AtmosphereView,
    ) -> Option<EnvironmentFrame> {
        engine_profiler::zone!("render::Environment::add_passes");
        let source = self.source.import(fg, "environment radiance");
        let prefiltered = self.prefiltered.import(fg, "environment prefiltered");
        let dfg = self.dfg.import(fg, "environment dfg");
        let read = BufferState {
            stages: vk::PipelineStageFlags2::FRAGMENT_SHADER,
            access: vk::AccessFlags2::SHADER_STORAGE_READ,
        };
        let sh = fg.import_buffer(
            "environment harmonics",
            ImportedBuffer {
                buffer: self.sh,
                size: 9 * 16,
                address: self.sh_address,
                initial: self.sh_state.get(),
                final_state: Some(read),
            },
        );
        self.sh_state.set(read);
        let sampler = Bindless::sampler(SamplerKind::LinearClamp);
        let [sky_cube, downsample, prefilter, irradiance, dfg_table] = &self.shaders;
        if !self.dfg_ready.replace(true) {
            let target = self.dfg.storage[0];
            fg.add_compute("environment dfg", bindless)
                .async_compute()
                .write(dfg, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(
                    dfg_table,
                    Dispatch::Threads([DFG_SIZE, DFG_SIZE, 1]),
                    move |_| {
                        Some(Push {
                            target,
                            size: DFG_SIZE,
                            samples: DFG_SAMPLES,
                            ..Push::default()
                        })
                    },
                );
        }
        // The physical sky: its tables, and whether it redraws the radiance cube.
        let fog = self.fog.get();
        let physical = self.atmosphere.get().zip(self.tables.as_ref());
        let debug = self.debug_directions.get();
        let mut rebuild = self.dirty.replace(false);
        let mut cube_from_atmosphere = false;
        if let Some((atmosphere, tables)) = physical {
            if rebuild {
                tables.forget_cube();
            }
            if !debug && tables.cube_stale(&atmosphere, view) {
                rebuild = true;
                cube_from_atmosphere = true;
            }
        }
        let sun_color = physical.map(|(a, t)| t.sun_color(&a, view));
        let atmosphere = if physical.is_some() || fog.is_some() {
            let mut constants = crate::atmosphere::constants(
                physical.as_ref().map(|(a, _)| a),
                fog.as_ref(),
                view,
                sun_color.unwrap_or(view.light),
                self.sh_address,
                self.source.sampled,
                self.intensity.get(),
                physical.map_or([0; 4], |(_, t)| t.indices()),
            );
            if let Some((_, t)) = physical {
                constants.set_sky_view_sun(t.sky_view_sun(constants.sky_view_sun()));
            }
            let address = ring.push(&[constants])?;
            let textures = physical.map(|(a, t)| {
                t.add_passes(
                    fg,
                    bindless,
                    &a,
                    &constants,
                    address,
                    cube_from_atmosphere.then(|| (source, self.source.storage[0])),
                )
            });
            Some(AtmosphereFrame {
                constants: address,
                textures,
            })
        } else {
            None
        };
        if rebuild {
            let sky = self.sky.get();
            let base = Push {
                zenith: sky.zenith.extend(0.0).to_array(),
                horizon: sky.horizon.extend(0.0).to_array(),
                ground: sky.ground.extend(0.0).to_array(),
                sampler,
                mode: u32::from(self.debug_directions.get()),
                ..Push::default()
            };
            let storage: &'g [u32] = &self.source.storage;
            // With the atmosphere, its pass drew mip 0 already: only the mips remain.
            let access = if cube_from_atmosphere {
                Access::StorageReadWrite(ShaderStages::COMPUTE)
            } else {
                Access::StorageWrite(ShaderStages::COMPUTE)
            };
            fg.add_compute("environment sky", bindless)
                .async_compute()
                .write(source, access)
                .record(move |rec| {
                    if !cube_from_atmosphere {
                        let push = Push {
                            target: storage[0],
                            size: SOURCE_SIZE,
                            ..base
                        };
                        rec.dispatch(
                            sky_cube,
                            Dispatch::Threads([SOURCE_SIZE, SOURCE_SIZE, 6]),
                            &push,
                        );
                    }
                    for level in 1..SOURCE_LEVELS {
                        // Each mip reads the one above, written by the previous dispatch.
                        if level > 1 || !cube_from_atmosphere {
                            rec.barrier();
                        }
                        let size = SOURCE_SIZE >> level;
                        let push = Push {
                            target: storage[level as usize],
                            read: storage[level as usize - 1],
                            size,
                            ..base
                        };
                        rec.dispatch(downsample, Dispatch::Threads([size, size, 6]), &push);
                    }
                });
            let targets: &'g [u32] = &self.prefiltered.storage;
            let source_index = self.source.sampled;
            fg.add_compute("environment prefilter", bindless)
                .async_compute()
                .read(source, Access::Sampled(ShaderStages::COMPUTE))
                .write(prefiltered, Access::StorageWrite(ShaderStages::COMPUTE))
                .record(move |rec| {
                    for level in 0..PREFILTERED_LEVELS {
                        let size = PREFILTERED_SIZE >> level;
                        let push = Push {
                            source: source_index,
                            target: targets[level as usize],
                            size,
                            level,
                            levels: PREFILTERED_LEVELS,
                            source_size: SOURCE_SIZE,
                            samples: PREFILTER_SAMPLES,
                            ..base
                        };
                        rec.dispatch(prefilter, Dispatch::Threads([size, size, 6]), &push);
                    }
                });
            fg.add_compute("environment irradiance", bindless)
                .async_compute()
                .read(source, Access::Sampled(ShaderStages::COMPUTE))
                .write(sh, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(irradiance, Dispatch::Groups([1, 1, 1]), move |ctx| {
                    Some(Push {
                        source: source_index,
                        sh: ctx.address(sh),
                        size: SOURCE_SIZE >> IRRADIANCE_LEVEL,
                        level: IRRADIANCE_LEVEL,
                        ..base
                    })
                });
        }
        Some(EnvironmentFrame {
            source,
            prefiltered,
            dfg,
            sh,
            source_index: self.source.sampled,
            prefiltered_index: self.prefiltered.sampled,
            dfg_index: self.dfg.sampled,
            sh_address: self.sh_address,
            intensity: self.intensity.get(),
            atmosphere,
            sun_color: sun_color.filter(|_| physical.is_some_and(|(a, _)| a.drives_sun)),
            rebuilt: rebuild,
        })
    }

    /// Frees the images and the buffer.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the device is idle).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.source.destroy(&self.device, allocator, bindless, 0);
            self.prefiltered
                .destroy(&self.device, allocator, bindless, 0);
            self.dfg.destroy(&self.device, allocator, bindless, 0);
            if let Some(tables) = self.tables.as_mut() {
                tables.destroy(&self.device, allocator, bindless);
            }
            if let Some(allocation) = self.sh_allocation.take() {
                allocator.destroy_buffer(self.sh, allocation);
            }
        }
    }
}
