//! HDR panoramas as an environment's sky (ADR 0083, docs/research/hdri-environments.md):
//! an equirectangular panorama, as `.hdr` files hold it (`engine_assets::hdr`), set on an
//! [`Environment`] with [`Environment::set_panorama`].
//!
//! A [`Panorama`] keeps:
//!
//! - its RGBE texels in a buffer the GPU reads (device-local and host-visible where
//!   available), written once when it is made: the shaders decode them, losing nothing of
//!   their range;
//! - a background cube made from them on the GPU by the first frame that draws it (faces of
//!   a quarter of the panorama's width by default: its resolution at the faces' centres),
//!   with its mips;
//! - its sun, if one was found (`engine_assets::hdr::find_sun`): the environment's lighting
//!   cube fills the sun's cone with the sky around it, and the scene takes the sun's light
//!   from a directional light ([`Panorama::sun`]) that casts shadows.
//!
//! Turning it ([`Panorama::set_rotation`]) remakes the background and the lighting from the
//! texels it keeps.
//!
//! [`Environment`]: crate::environment::Environment
//! [`Environment::set_panorama`]: crate::environment::Environment::set_panorama

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::environment::{CUBE_FORMAT, Image, ImageKind};
use crate::graph::{Access, FrameGraph, ShaderStages, Texture};
use core::cell::Cell;
use engine_core::math::{Quat, Vec3};
use engine_rhi::ash::vk;
use engine_rhi::{Allocation, Allocator, Bindless, Device, MemoryLocation, RhiError, SamplerKind};

const PANORAMA_CUBE: &[u8] = include_bytes!("../shaders/panorama.panoramaCube.spv");
const PANORAMA_LIGHT: &[u8] = include_bytes!("../shaders/panorama.panoramaLight.spv");

/// The largest face of a background cube sized from its panorama
/// ([`PanoramaDesc::background_size`] 0): a quarter of the panorama's width, rounded up to
/// a power of two, at most this (a 4K panorama's resolution).
pub const MAX_BACKGROUND_SIZE: u32 = 1024;
/// Samples of the panorama per side of a background texel.
const BACKGROUND_SAMPLES: u32 = 2;
/// Samples of the background per side of a lighting texel.
const LIGHT_SAMPLES: u32 = 4;

/// `PanoramaPush` in `panorama.slang` (80 bytes).
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct PanoramaPush {
    texels: u64,
    width: u32,
    height: u32,
    turn: [f32; 2],
    target: u32,
    size: u32,
    sun: [f32; 4],
    sun_sky: [f32; 4],
    source: u32,
    sampler: u32,
    level: f32,
    samples: u32,
}

/// A panorama's sun, as `engine_assets::hdr::find_sun` finds it (the renderer does not
/// depend on the asset crate): the directional light that replaces it, and what fills its
/// place in the lighting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanoramaSun {
    /// Unit direction towards it: in the panorama's frame in a [`PanoramaDesc`], in the
    /// world (turned with the panorama) from [`Panorama::sun`].
    pub direction: Vec3,
    /// Its illuminance at normal incidence, linear RGB in the panorama's units: a
    /// directional light of this color (times the environment's intensity) replaces it.
    pub illuminance: Vec3,
    /// Angular radius of its cone, in radians.
    pub cone: f32,
    /// The radiance the lighting cube takes inside the cone: the sky around it.
    pub sky: Vec3,
}

/// What [`Panorama::new`] makes a panorama from.
#[derive(Debug, Clone, Copy)]
pub struct PanoramaDesc<'a> {
    /// Pixels per row.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// RGBE pixels (`engine_assets::hdr::HdrImage::pixels`), rows from the top (+Y): the
    /// middle column faces −Z, three quarters of the width +X.
    pub pixels: &'a [[u8; 4]],
    /// Its sun, if one was found: the lighting leaves it out, for a directional light.
    /// `None`: the lighting keeps all of the panorama.
    pub sun: Option<PanoramaSun>,
    /// Face size of the background cube; 0: a quarter of the width, rounded up to a power
    /// of two, at most [`MAX_BACKGROUND_SIZE`].
    pub background_size: u32,
}

/// An equirectangular HDR panorama made ready to be an environment's sky (see the module
/// documentation).
pub struct Panorama {
    device: engine_rhi::ash::Device,
    width: u32,
    height: u32,
    texels: vk::Buffer,
    texels_allocation: Option<Allocation>,
    texels_address: vk::DeviceAddress,
    pub(crate) background: Image,
    background_size: u32,
    sun: Option<PanoramaSun>,
    rotation: Cell<f32>,
    /// Whether the background cube holds the panorama at its rotation.
    pub(crate) converted: Cell<bool>,
    /// `panoramaCube`, `panoramaLight`.
    shaders: [ComputeShader<PanoramaPush>; 2],
}

impl core::fmt::Debug for Panorama {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Panorama")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("background_size", &self.background_size)
            .field("sun", &self.sun)
            .field("rotation", &self.rotation.get())
            .finish_non_exhaustive()
    }
}

impl Panorama {
    /// Writes the panorama's texels into a buffer the GPU reads, and makes its background
    /// cube (filled by the first frame that draws it) and its two compute pipelines.
    ///
    /// # Errors
    ///
    /// If memory runs out or a pipeline cannot be made.
    ///
    /// # Panics
    ///
    /// If `desc.pixels` does not hold `width × height` pixels, or the panorama is empty.
    pub fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        desc: &PanoramaDesc<'_>,
    ) -> Result<Self, RhiError> {
        engine_profiler::zone!("render::Panorama::new");
        let count = desc.width as usize * desc.height as usize;
        assert!(
            count > 0 && desc.pixels.len() == count,
            "a panorama of {} × {} pixels with {} of them",
            desc.width,
            desc.height,
            desc.pixels.len()
        );
        let source = |spirv: &[u8], entry| ComputeSource::new(spirv, entry, &[]);
        let shaders = ComputeSource::create_all(
            device,
            bindless,
            [
                source(PANORAMA_CUBE, c"panoramaCube")?,
                source(PANORAMA_LIGHT, c"panoramaLight")?,
            ],
        )?;
        let bytes = count as u64 * 4;
        let (texels, mut allocation) = allocator.create_buffer(
            &vk::BufferCreateInfo::default().size(bytes).usage(
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
            ),
            MemoryLocation::Dynamic,
        )?;
        match allocation.mapped_slice_mut() {
            Some(mapped) => {
                mapped[..bytes as usize].copy_from_slice(bytemuck::cast_slice(desc.pixels))
            }
            None => {
                // SAFETY: never used by the GPU.
                unsafe { allocator.destroy_buffer(texels, allocation) };
                return Err(RhiError::Vulkan {
                    call: "panorama texels not mapped",
                    result: vk::Result::ERROR_MEMORY_MAP_FAILED,
                });
            }
        }
        // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
        let texels_address = unsafe {
            device
                .raw()
                .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(texels))
        };
        let background_size = match desc.background_size {
            0 => desc
                .width
                .div_ceil(4)
                .next_power_of_two()
                .clamp(16, MAX_BACKGROUND_SIZE),
            size => size,
        };
        let background = match Image::new(
            device,
            allocator,
            bindless,
            CUBE_FORMAT,
            background_size,
            background_size,
            background_size.ilog2() + 1,
            ImageKind::Cube,
        ) {
            Ok(image) => image,
            Err(e) => {
                // SAFETY: never used by the GPU.
                unsafe { allocator.destroy_buffer(texels, allocation) };
                return Err(e);
            }
        };
        Ok(Self {
            device: device.raw().clone(),
            width: desc.width,
            height: desc.height,
            texels,
            texels_allocation: Some(allocation),
            texels_address,
            background,
            background_size,
            sun: desc.sun.map(|s| PanoramaSun {
                direction: s.direction.normalize_or(Vec3::Y),
                ..s
            }),
            rotation: Cell::new(0.0),
            converted: Cell::new(false),
            shaders,
        })
    }

    /// The panorama's width and height in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Face size of the background cube.
    pub fn background_size(&self) -> u32 {
        self.background_size
    }

    /// GPU memory: the texels and the background cube with its mips.
    pub fn memory(&self) -> u64 {
        let faces: u64 = (0..=self.background_size.ilog2())
            .map(|level| u64::from(self.background_size >> level).pow(2))
            .sum();
        u64::from(self.width) * u64::from(self.height) * 4 + faces * 6 * 8
    }

    /// Turns the panorama about +Y by `radians` (counter-clockwise seen from above, the
    /// world's +X towards −Z), its sun with it; the next frame that draws it remakes its
    /// background and the environment's lighting (a few compute passes).
    pub fn set_rotation(&self, radians: f32) {
        if radians != self.rotation.get() {
            self.rotation.set(radians);
            self.converted.set(false);
        }
    }

    /// The turn about +Y, in radians.
    pub fn rotation(&self) -> f32 {
        self.rotation.get()
    }

    /// Its sun in the world (turned with the panorama), if it has one: light the scene
    /// with a directional light travelling along `−direction`, of color `illuminance` times
    /// the environment's intensity.
    pub fn sun(&self) -> Option<PanoramaSun> {
        self.sun.map(|s| PanoramaSun {
            direction: self.turn() * s.direction,
            ..s
        })
    }

    fn turn(&self) -> Quat {
        Quat::from_rotation_y(self.rotation.get())
    }

    /// Declares the pass that makes the background cube from the texels: mip 0, then each
    /// mip from the one above (`downsample`, the environment's).
    pub(crate) fn add_background_pass<'g, P: bytemuck::Pod>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        background: Texture,
        downsample: &'g ComputeShader<P>,
        downsample_push: impl Fn(u32, u32, u32) -> P + 'g,
    ) {
        let (sin, cos) = self.rotation.get().sin_cos();
        let push = PanoramaPush {
            texels: self.texels_address,
            width: self.width,
            height: self.height,
            turn: [sin, cos],
            target: self.background.storage[0],
            size: self.background_size,
            samples: BACKGROUND_SAMPLES,
            ..PanoramaPush::default()
        };
        let storage: &'g [u32] = &self.background.storage;
        let size = self.background_size;
        let shader = &self.shaders[0];
        fg.add_compute("panorama background", bindless)
            .async_compute()
            .write(background, Access::StorageWrite(ShaderStages::COMPUTE))
            .record(move |rec| {
                rec.dispatch(shader, Dispatch::Threads([size, size, 6]), &push);
                for level in 1..storage.len() {
                    rec.barrier();
                    let s = size >> level;
                    let p = downsample_push(storage[level], storage[level - 1], s);
                    rec.dispatch(downsample, Dispatch::Threads([s, s, 6]), &p);
                }
            });
    }

    /// The push constants that fill mip 0 of a lighting cube of face `size` (storage index
    /// `target`) from the background cube, its sun's cone left out.
    pub(crate) fn light_push(&self, target: u32, size: u32) -> PanoramaLight<'_> {
        let sun = self.sun();
        // The background's mip whose texels are as large as the samples (LIGHT_SAMPLES² per
        // lighting texel).
        let level = (self.background_size as f32 / (size * LIGHT_SAMPLES) as f32)
            .log2()
            .max(0.0);
        PanoramaLight {
            shader: &self.shaders[1],
            push: PanoramaPush {
                target,
                size,
                sun: sun.map_or([0.0, 0.0, 0.0, 2.0], |s| {
                    s.direction.extend(s.cone.cos()).to_array()
                }),
                sun_sky: sun.map_or([0.0; 4], |s| s.sky.extend(0.0).to_array()),
                source: self.background.sampled,
                sampler: Bindless::sampler(SamplerKind::LinearClamp),
                level,
                samples: LIGHT_SAMPLES,
                ..PanoramaPush::default()
            },
        }
    }

    /// Frees the texels, the background cube and the pipelines.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them (the frames that drew it have finished).
    pub unsafe fn destroy(&mut self, allocator: &mut Allocator, bindless: &Bindless) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            self.background
                .destroy(&self.device, allocator, bindless, 0);
            if let Some(allocation) = self.texels_allocation.take() {
                allocator.destroy_buffer(self.texels, allocation);
            }
        }
    }
}

/// The lighting cube's first pass from a panorama ([`Panorama::light_push`]).
pub(crate) struct PanoramaLight<'a> {
    shader: &'a ComputeShader<PanoramaPush>,
    push: PanoramaPush,
}

impl PanoramaLight<'_> {
    /// Records it: mip 0 of the lighting cube.
    pub(crate) fn dispatch(&self, rec: &mut crate::compute::ComputeRecorder<'_, '_>) {
        let size = self.push.size;
        rec.dispatch(self.shader, Dispatch::Threads([size, size, 6]), &self.push);
    }
}
