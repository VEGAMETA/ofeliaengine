//! HDR panoramas as an environment's sky (ADR 0083, docs/research/hdri-environments.md),
//! set on an [`Environment`] with [`Environment::set_panorama`]. A [`Panorama`] is made
//! either
//!
//! - from an equirectangular panorama as `.hdr` files hold it (`engine_assets::hdr`;
//!   [`Panorama::new`]): its RGBE texels in a buffer the GPU reads (device-local and
//!   host-visible where available, written once; the shaders decode them, losing nothing of
//!   their range), from which the first frame that draws it makes its background cube (faces
//!   of a quarter of the panorama's width by default: its resolution at the faces'
//!   centres), with its mips;
//! - or from a cube cooked offline (`engine_assets::panorama`; [`Panorama::from_cooked`]):
//!   BC6H levels, an eighth of the memory, uploaded through the frame's upload ring over the
//!   first frames that draw it (the previous sky stays until it is complete).
//!
//! Its sun, if one was found (`engine_assets::hdr::find_sun`), is left out of the
//! environment's lighting cube (its cone filled with the sky around it): the scene takes
//! the sun's light from a directional light ([`Panorama::sun`]) that casts shadows.
//!
//! The background cube stays in the panorama's own frame: turning it
//! ([`Panorama::set_rotation`]) turns the directions its readers sample it with (the sky
//! pass, the atmosphere's composite, the lighting cube's fill), so only the lighting is
//! remade.
//!
//! [`Environment`]: crate::environment::Environment
//! [`Environment::set_panorama`]: crate::environment::Environment::set_panorama

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::environment::{CUBE_FORMAT, Image, ImageKind};
use crate::graph::{Access, FrameGraph, ShaderStages, Texture};
use crate::upload::UploadRing;
use core::cell::{Cell, RefCell};
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
/// Samples of the background per side of a lighting texel, at the mip whose texels are as
/// large as the samples: 2 × 2 of the mip twice as fine as the lighting cube (its mips are
/// box-filtered: the same as 4 × 4 of the next finer one, at a quarter of the reads; the
/// reads of a turned panorama cross the background's tiles, measured 3.6 times slower on
/// the Radeon iGPU at 4 × 4).
const LIGHT_SAMPLES: u32 = 2;

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
    /// Unit direction towards it: in the panorama's frame in a [`PanoramaDesc`] or a
    /// [`CookedPanoramaDesc`], in the world (turned with the panorama) from
    /// [`Panorama::sun`].
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

/// What [`Panorama::from_cooked`] makes a panorama from: a cube cooked offline
/// (`engine_assets::panorama::CookedPanorama`).
#[derive(Debug, Clone, Copy)]
pub struct CookedPanoramaDesc<'a> {
    /// Face size of level 0 (a power of two).
    pub size: u32,
    /// Its levels from the full size down to 1 × 1, each its six faces of BC6H blocks
    /// (unsigned), face after face (+X, −X, +Y, −Y, +Z, −Z).
    pub levels: &'a [Vec<u8>],
    /// Its sun, in the panorama's frame.
    pub sun: Option<PanoramaSun>,
}

/// Where a panorama's background cube comes from.
enum Source {
    /// RGBE texels the GPU turns into the cube.
    Texels {
        buffer: vk::Buffer,
        allocation: Option<Allocation>,
        address: vk::DeviceAddress,
        width: u32,
        height: u32,
    },
    /// A cooked cube: its levels, uploaded face by face (`next`), then dropped.
    Cooked {
        levels: RefCell<Vec<Vec<u8>>>,
        next: Cell<u32>,
    },
}

/// An HDR panorama made ready to be an environment's sky (see the module documentation).
pub struct Panorama {
    device: engine_rhi::ash::Device,
    source: Source,
    pub(crate) background: Image,
    background_size: u32,
    background_levels: u32,
    sun: Option<PanoramaSun>,
    rotation: Cell<f32>,
    /// Whether the background cube holds the panorama.
    converted: Cell<bool>,
    /// Whether it turned since its environment's lighting was made.
    pub(crate) turned: Cell<bool>,
    /// `panoramaCube`, `panoramaLight`.
    shaders: [ComputeShader<PanoramaPush>; 2],
}

impl core::fmt::Debug for Panorama {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Panorama")
            .field("cooked", &matches!(self.source, Source::Cooked { .. }))
            .field("background_size", &self.background_size)
            .field("sun", &self.sun)
            .field("rotation", &self.rotation.get())
            .finish_non_exhaustive()
    }
}

fn shaders(
    device: &Device,
    bindless: &Bindless,
) -> Result<[ComputeShader<PanoramaPush>; 2], RhiError> {
    let source = |spirv: &[u8], entry| ComputeSource::new(spirv, entry, &[]);
    ComputeSource::create_all(
        device,
        bindless,
        [
            source(PANORAMA_CUBE, c"panoramaCube")?,
            source(PANORAMA_LIGHT, c"panoramaLight")?,
        ],
    )
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
        let shaders = shaders(device, bindless)?;
        let bytes = count as u64 * 4;
        let (buffer, mut allocation) = allocator.create_buffer(
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
                unsafe { allocator.destroy_buffer(buffer, allocation) };
                return Err(RhiError::Vulkan {
                    call: "panorama texels not mapped",
                    result: vk::Result::ERROR_MEMORY_MAP_FAILED,
                });
            }
        }
        // SAFETY: created with SHADER_DEVICE_ADDRESS usage and bound.
        let address = unsafe {
            device
                .raw()
                .get_buffer_device_address(&vk::BufferDeviceAddressInfo::default().buffer(buffer))
        };
        let size = match desc.background_size {
            0 => desc
                .width
                .div_ceil(4)
                .next_power_of_two()
                .clamp(16, MAX_BACKGROUND_SIZE),
            size => size,
        };
        let levels = size.ilog2() + 1;
        let background = match Image::new(
            device,
            allocator,
            bindless,
            CUBE_FORMAT,
            size,
            size,
            levels,
            ImageKind::Cube,
        ) {
            Ok(image) => image,
            Err(e) => {
                // SAFETY: never used by the GPU.
                unsafe { allocator.destroy_buffer(buffer, allocation) };
                return Err(e);
            }
        };
        Ok(Self::with(
            device,
            Source::Texels {
                buffer,
                allocation: Some(allocation),
                address,
                width: desc.width,
                height: desc.height,
            },
            background,
            (size, levels),
            desc.sun,
            shaders,
        ))
    }

    /// Makes the background cube of a cooked panorama and keeps its levels until the first
    /// frames that draw it have uploaded them.
    ///
    /// # Errors
    ///
    /// If memory runs out or a pipeline cannot be made.
    ///
    /// # Panics
    ///
    /// If the levels do not make a BC6H cube of face `desc.size` down to 1 × 1.
    pub fn from_cooked(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
        desc: &CookedPanoramaDesc<'_>,
    ) -> Result<Self, RhiError> {
        engine_profiler::zone!("render::Panorama::from_cooked");
        let size = desc.size;
        assert!(size.is_power_of_two(), "a cooked panorama of face {size}");
        let levels = size.ilog2() + 1;
        assert_eq!(desc.levels.len(), levels as usize, "cooked panorama levels");
        for (k, level) in desc.levels.iter().enumerate() {
            assert_eq!(
                level.len(),
                6 * face_bytes(size, k as u32),
                "cooked panorama level {k}"
            );
        }
        let shaders = shaders(device, bindless)?;
        let background = Image::sampled(
            device,
            allocator,
            bindless,
            vk::Format::BC6H_UFLOAT_BLOCK,
            (size, size),
            levels,
            ImageKind::Cube,
        )?;
        Ok(Self::with(
            device,
            Source::Cooked {
                levels: RefCell::new(desc.levels.to_vec()),
                next: Cell::new(0),
            },
            background,
            (size, levels),
            desc.sun,
            shaders,
        ))
    }

    fn with(
        device: &Device,
        source: Source,
        background: Image,
        (background_size, background_levels): (u32, u32),
        sun: Option<PanoramaSun>,
        shaders: [ComputeShader<PanoramaPush>; 2],
    ) -> Self {
        Self {
            device: device.raw().clone(),
            source,
            background,
            background_size,
            background_levels,
            sun: sun.map(|s| PanoramaSun {
                direction: s.direction.normalize_or(Vec3::Y),
                ..s
            }),
            rotation: Cell::new(0.0),
            converted: Cell::new(false),
            turned: Cell::new(false),
            shaders,
        }
    }

    /// Face size of the background cube.
    pub fn background_size(&self) -> u32 {
        self.background_size
    }

    /// Whether it was cooked offline ([`Self::from_cooked`]).
    pub fn is_cooked(&self) -> bool {
        matches!(self.source, Source::Cooked { .. })
    }

    /// Whether the background cube is ready (made, or uploaded): the environment draws the
    /// panorama from then on.
    pub fn is_ready(&self) -> bool {
        self.converted.get()
    }

    /// GPU memory: the texels (of a panorama made with [`Self::new`]) and the background
    /// cube with its mips.
    pub fn memory(&self) -> u64 {
        let texels = match &self.source {
            Source::Texels { width, height, .. } => u64::from(*width) * u64::from(*height) * 4,
            Source::Cooked { .. } => 0,
        };
        let cube: u64 = (0..self.background_levels)
            .map(|level| match self.source {
                Source::Texels { .. } => u64::from(self.background_size >> level).pow(2) * 8,
                Source::Cooked { .. } => face_bytes(self.background_size, level) as u64,
            })
            .sum();
        texels + 6 * cube
    }

    /// Turns the panorama about +Y by `radians` (counter-clockwise seen from above, the
    /// world's +X towards −Z), its sun with it; the next frame that draws it remakes the
    /// environment's lighting (a few compute passes; the background cube stays).
    pub fn set_rotation(&self, radians: f32) {
        if radians != self.rotation.get() {
            self.rotation.set(radians);
            self.turned.set(true);
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
            direction: Quat::from_rotation_y(self.rotation.get()) * s.direction,
            ..s
        })
    }

    /// Declares this frame's work on the background cube: none once it is ready; the pass
    /// that makes it from the texels (mip 0, then each mip from the one above with
    /// `downsample`, the environment's); or the upload of the next faces of a cooked cube
    /// through `ring` (as many as fit in half of its free space, at least one a frame).
    /// Returns the background cube's texture and whether it became ready this frame, or
    /// `None` while a cooked cube is still uploading.
    pub(crate) fn prepare<'g, P: bytemuck::Pod>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        ring: &mut UploadRing,
        downsample: &'g ComputeShader<P>,
        downsample_push: impl Fn(u32, u32, u32) -> P + 'g,
    ) -> Option<(Texture, bool)> {
        if self.converted.get() {
            return Some((self.background.import(fg, "panorama background"), false));
        }
        match &self.source {
            Source::Texels {
                address,
                width,
                height,
                ..
            } => {
                let background = self.background.import(fg, "panorama background");
                let push = PanoramaPush {
                    texels: *address,
                    width: *width,
                    height: *height,
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
                self.converted.set(true);
                Some((background, true))
            }
            Source::Cooked { levels, next } => {
                let pieces = 6 * self.background_levels;
                let mut regions = Vec::new();
                let budget = ring.available() / 2;
                let mut staged_bytes = 0;
                let held = levels.borrow();
                while next.get() < pieces {
                    let piece = next.get();
                    let (level, face) = (piece / 6, piece % 6);
                    let bytes = face_bytes(self.background_size, level);
                    if !regions.is_empty() && staged_bytes + bytes as u64 > budget {
                        break;
                    }
                    let data = &held[level as usize][face as usize * bytes..][..bytes];
                    let Some(staged) = ring.stage(data) else {
                        break;
                    };
                    let s = (self.background_size >> level).max(1);
                    regions.push((
                        staged.buffer,
                        vk::BufferImageCopy {
                            buffer_offset: staged.offset,
                            image_subresource: vk::ImageSubresourceLayers {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                mip_level: level,
                                base_array_layer: face,
                                layer_count: 1,
                            },
                            image_extent: vk::Extent3D {
                                width: s,
                                height: s,
                                depth: 1,
                            },
                            ..Default::default()
                        },
                    ));
                    staged_bytes += bytes as u64;
                    next.set(piece + 1);
                }
                drop(held);
                let done = next.get() == pieces;
                if regions.is_empty() {
                    return None;
                }
                let background = self.background.import(fg, "panorama background");
                fg.add_pass("panorama upload")
                    .write(background, Access::TransferWrite)
                    .execute(move |ctx| {
                        for (buffer, region) in &regions {
                            // SAFETY: recording outside a rendering scope; the region lies
                            // within the image, its blocks within the ring's allocation.
                            unsafe {
                                ctx.device.cmd_copy_buffer_to_image(
                                    ctx.cmd,
                                    *buffer,
                                    ctx.image(background),
                                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                                    core::slice::from_ref(region),
                                );
                            }
                        }
                    });
                if !done {
                    return None;
                }
                levels.borrow_mut().clear();
                levels.borrow_mut().shrink_to_fit();
                self.converted.set(true);
                Some((background, true))
            }
        }
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
        let (sin, cos) = self.rotation.get().sin_cos();
        PanoramaLight {
            shader: &self.shaders[1],
            push: PanoramaPush {
                turn: [sin, cos],
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
            if let Source::Texels {
                buffer, allocation, ..
            } = &mut self.source
                && let Some(allocation) = allocation.take()
            {
                allocator.destroy_buffer(*buffer, allocation);
            }
        }
    }
}

/// Bytes of one face of level `level` of a BC6H cube of face `size`: whole 4 × 4 blocks.
fn face_bytes(size: u32, level: u32) -> usize {
    let s = (size >> level).max(1);
    (s.div_ceil(4) * s.div_ceil(4) * 16) as usize
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
