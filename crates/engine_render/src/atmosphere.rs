//! A physically based sky and atmosphere, and exponential height fog (ADR 0069,
//! docs/research/atmosphere.md), for an [`crate::environment::Environment`].
//!
//! The atmosphere is Hillaire's (EGSR 2020): a planet under Rayleigh and Mie scattering
//! and ozone absorption, rendered from four tables made in compute passes:
//!
//! - **transmittance** (256 × 64) and **multiple scattering** (32 × 32): from the
//!   atmosphere's parameters alone, rebuilt only when they change;
//! - **sky view** (192 × 108): the sky around the camera, rebuilt when the sun's height
//!   or the camera's altitude changes;
//! - **aerial perspective** (32 × 32 × 32 froxels): light scattered between the camera and
//!   what it sees, every frame (a few µs).
//!
//! The environment's radiance cube for image-based light is drawn from the sky-view table,
//! so ambient light matches the sky; the directional light's color can follow the sun's
//! height ([`Atmosphere::drives_sun`]). One full-screen pass after the opaque surfaces draws
//! the sky and lays the aerial perspective and the fog over everything else; transparent
//! surfaces apply both in their own pipelines. The forward shading of opaque surfaces does
//! not change at all.
//!
//! [`HeightFog`] is exponential height fog: density `d · exp(−falloff · (y − base))`,
//! integrated analytically along each view ray, lit by the sky's average radiance and by
//! the sun through a Henyey-Greenstein phase function (not shadowed).

use crate::compute::{ComputeShader, ComputeSource, Dispatch};
use crate::environment::{EnvironmentFrame, Image, ImageKind};
use crate::graph::{Access, FrameGraph, LoadOp, ShaderStages, StoreOp, Texture};
use core::cell::Cell;
use engine_core::math::{Mat4, Vec3};
use engine_rhi::ash::vk;
use engine_rhi::{Allocator, Bindless, Device, Pipeline, RhiError};

const TRANSMITTANCE_LUT: &[u8] = include_bytes!("../shaders/atmosphere.transmittanceLut.spv");
const MULTIPLE_SCATTERING_LUT: &[u8] =
    include_bytes!("../shaders/atmosphere.multipleScatteringLut.spv");
const SKY_VIEW_LUT: &[u8] = include_bytes!("../shaders/atmosphere.skyViewLut.spv");
const AERIAL_PERSPECTIVE: &[u8] = include_bytes!("../shaders/atmosphere.aerialPerspective.spv");
const ATMOSPHERE_CUBE: &[u8] = include_bytes!("../shaders/atmosphere.atmosphereCube.spv");
/// The full-screen pass's shaders (the forward renderer makes its pipeline).
pub(crate) const COMPOSITE_VERTEX: &[u8] =
    include_bytes!("../shaders/atmosphere.compositeVertex.spv");
pub(crate) const COMPOSITE_FRAGMENT: &[u8] =
    include_bytes!("../shaders/atmosphere.compositeFragment.spv");

/// Size of the transmittance table (cos zenith × altitude).
pub const TRANSMITTANCE_SIZE: [u32; 2] = [256, 64];
/// Size of the multiple scattering table (cos sun zenith × altitude).
pub const MULTIPLE_SCATTERING_SIZE: u32 = 32;
/// Size of the sky-view table (azimuth from the sun × zenith angle).
pub const SKY_VIEW_SIZE: [u32; 2] = [192, 108];
/// Froxels of the aerial perspective volume per side (slices squared-distributed over
/// [`AERIAL_SLICES`] × [`AERIAL_KM_PER_SLICE`] km).
pub const AERIAL_SLICES: u32 = 32;
/// See [`AERIAL_SLICES`].
pub const AERIAL_KM_PER_SLICE: f32 = 4.0;
/// Format of every table.
pub const LUT_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// The sky-view table follows the camera's altitude in steps of this many km.
const SKY_VIEW_ALTITUDE_STEP: f32 = 0.01;
/// The radiance cube (image-based light, a costlier rebuild) follows the camera's altitude
/// in steps of this many km...
const CUBE_ALTITUDE_STEP: f32 = 0.1;
/// ... and the sun once it moved by more than this angle (0.1°, cos).
const CUBE_SUN_STEP_COS: f32 = 0.999_998_5;

/// `ATMOSPHERE_*` in `atmosphere_common.slang`.
const FLAG_SKY: u32 = 1;
const FLAG_FOG: u32 = 2;

/// A planet's atmosphere (Hillaire 2020, after Bruneton 2017's parameters). Distances in
/// kilometres, coefficients per kilometre. [`Atmosphere::earth`] (the default) is Earth's
/// on a clear day.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Atmosphere {
    /// Radius of the ground, km.
    pub ground_radius: f32,
    /// Radius of the top of the atmosphere, km.
    pub top_radius: f32,
    /// Rayleigh scattering at the ground (air molecules; per km, per channel: blue
    /// scatters most).
    pub rayleigh_scattering: Vec3,
    /// Height over which Rayleigh density falls by e, km.
    pub rayleigh_scale_height: f32,
    /// Mie scattering at the ground (aerosols, haze; per km).
    pub mie_scattering: Vec3,
    /// Mie absorption at the ground (per km).
    pub mie_absorption: Vec3,
    /// Height over which Mie density falls by e, km.
    pub mie_scale_height: f32,
    /// Anisotropy of Mie scattering (Cornette-Shanks phase function): towards 1, light
    /// keeps going forward (the glow around the sun).
    pub mie_anisotropy: f32,
    /// Ozone absorption at its peak (per km): blue skies at dusk.
    pub ozone_absorption: Vec3,
    /// Altitude of the ozone layer's peak, km.
    pub ozone_peak_altitude: f32,
    /// Half width of the ozone layer (its density falls linearly to 0), km.
    pub ozone_half_width: f32,
    /// Albedo of the ground (light it bounces into the sky, and the ground's color in the
    /// sky below the horizon).
    pub ground_albedo: Vec3,
    /// Scale of multiple scattering (1: physical, 0: single scattering only).
    pub multiple_scattering: f32,
    /// Angular radius of the sun's disk, radians.
    pub sun_angular_radius: f32,
    /// Altitude of the world's origin above the ground, km: the camera is at this altitude
    /// plus its `y`.
    pub origin_altitude: f32,
    /// World units per metre ÷ 1: metres per world unit (1 for a scene in metres).
    pub meters_per_unit: f32,
    /// Distance scale of the aerial perspective (Unreal's "aerial perspective view distance
    /// scale"): above 1, small scenes look as far away as this many times their size.
    pub aerial_perspective_scale: f32,
    /// The directional light is the sun: its color is the light outside the atmosphere,
    /// dimmed and reddened by the atmosphere at the camera (to nothing once the sun has
    /// set). Off, the light's color is used as it is.
    pub drives_sun: bool,
}

impl Default for Atmosphere {
    fn default() -> Self {
        Self::earth()
    }
}

impl Atmosphere {
    /// Earth's atmosphere on a clear day: the parameters of Hillaire's reference
    /// implementation (themselves Bruneton 2017's, integrated over the visible spectrum),
    /// ground albedo 0.3.
    pub fn earth() -> Self {
        Self {
            ground_radius: 6360.0,
            top_radius: 6460.0,
            rayleigh_scattering: Vec3::new(5.802e-3, 13.558e-3, 33.1e-3),
            rayleigh_scale_height: 8.0,
            mie_scattering: Vec3::splat(3.996e-3),
            mie_absorption: Vec3::splat(0.444e-3),
            mie_scale_height: 1.2,
            mie_anisotropy: 0.8,
            ozone_absorption: Vec3::new(0.650e-3, 1.881e-3, 0.085e-3),
            ozone_peak_altitude: 25.0,
            ozone_half_width: 15.0,
            ground_albedo: Vec3::splat(0.3),
            multiple_scattering: 1.0,
            sun_angular_radius: 0.004675,
            origin_altitude: 0.0,
            meters_per_unit: 1.0,
            aerial_perspective_scale: 1.0,
            drives_sun: true,
        }
    }

    /// Whether the tables depend on anything that differs between `self` and `other`
    /// (the transmittance and multiple scattering tables).
    fn same_tables(&self, other: &Self) -> bool {
        self.ground_radius == other.ground_radius
            && self.top_radius == other.top_radius
            && self.rayleigh_scattering == other.rayleigh_scattering
            && self.rayleigh_scale_height == other.rayleigh_scale_height
            && self.mie_scattering == other.mie_scattering
            && self.mie_absorption == other.mie_absorption
            && self.mie_scale_height == other.mie_scale_height
            && self.mie_anisotropy == other.mie_anisotropy
            && self.ozone_absorption == other.ozone_absorption
            && self.ozone_peak_altitude == other.ozone_peak_altitude
            && self.ozone_half_width == other.ozone_half_width
            && self.ground_albedo == other.ground_albedo
            && self.multiple_scattering == other.multiple_scattering
    }

    /// Extinction (per km) at `altitude` km: Rayleigh, Mie (scattering + absorption) and
    /// ozone.
    pub fn extinction(&self, altitude: f32) -> Vec3 {
        let h = altitude.max(0.0);
        let rayleigh = (-h / self.rayleigh_scale_height).exp();
        let mie = (-h / self.mie_scale_height).exp();
        let ozone = (1.0 - (h - self.ozone_peak_altitude).abs() / self.ozone_half_width).max(0.0);
        self.rayleigh_scattering * rayleigh
            + (self.mie_scattering + self.mie_absorption) * mie
            + self.ozone_absorption * ozone
    }

    /// Transmittance from a point `altitude` km above the ground to the top of the
    /// atmosphere, along a direction whose zenith angle has cosine `cos_zenith` (through
    /// the ground as if it were air, as the GPU table does), integrated in double
    /// precision over 512 steps.
    pub fn transmittance(&self, altitude: f32, cos_zenith: f32) -> Vec3 {
        let r = f64::from(self.ground_radius + altitude.max(0.0));
        let mu = f64::from(cos_zenith.clamp(-1.0, 1.0));
        let top = f64::from(self.top_radius);
        let distance = (-r * mu + (r * r * (mu * mu - 1.0) + top * top).max(0.0).sqrt()).max(0.0);
        const STEPS: usize = 512;
        let dt = distance / STEPS as f64;
        let mut depth = [0.0f64; 3];
        for i in 0..STEPS {
            let t = (i as f64 + 0.5) * dt;
            // Altitude of the point t along the ray.
            let radius = (r * r + t * t + 2.0 * r * mu * t).sqrt();
            let e = self.extinction((radius - f64::from(self.ground_radius)) as f32);
            depth[0] += f64::from(e.x) * dt;
            depth[1] += f64::from(e.y) * dt;
            depth[2] += f64::from(e.z) * dt;
        }
        Vec3::new(
            (-depth[0]).exp() as f32,
            (-depth[1]).exp() as f32,
            (-depth[2]).exp() as f32,
        )
    }

    /// The sun's light reaching `altitude` km: `illuminance` (outside the atmosphere) ×
    /// the transmittance towards `to_sun` (unit, +Y up), × the part of its disk above the
    /// horizon.
    pub fn sun_light(&self, illuminance: Vec3, to_sun: Vec3, altitude: f32) -> Vec3 {
        let altitude = altitude.max(0.0);
        let r = self.ground_radius + altitude;
        let mu = to_sun.normalize_or(Vec3::Y).y.clamp(-1.0, 1.0);
        // The horizon's elevation (negative above the ground), and the sun's above it in
        // units of the disk's radius.
        let horizon = -(1.0 - (self.ground_radius / r).powi(2)).max(0.0).sqrt();
        let elevation = mu.asin() - horizon.asin();
        let x = (elevation / self.sun_angular_radius.max(1e-6)).clamp(-1.0, 1.0);
        // Area of the disk above a chord at x (in radii), over the disk's.
        let visible = 0.5 + (x * (1.0 - x * x).sqrt() + x.asin()) / core::f32::consts::PI;
        if visible <= 0.0 {
            return Vec3::ZERO;
        }
        illuminance * self.transmittance(altitude, mu.max(horizon)) * visible
    }
}

/// Exponential height fog: extinction `density · exp(−falloff · (y − base_height))` per
/// world unit, thickest below `base_height`, thinning above it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeightFog {
    /// Extinction per world unit at `base_height` (0.01: light loses 63% in 100 units).
    pub density: f32,
    /// World height where the density is `density`.
    pub base_height: f32,
    /// How fast it thins with height, per world unit (0: uniform fog everywhere).
    pub falloff: f32,
    /// Single-scattering albedo: the fog's color (the part of light it scatters rather than
    /// absorbs).
    pub albedo: Vec3,
    /// Henyey-Greenstein anisotropy of the sun's light in the fog: towards 1 it glows
    /// around the sun, 0 scatters evenly.
    pub sun_anisotropy: f32,
    /// No fog nearer than this (world units).
    pub start_distance: f32,
    /// Most the fog may hide (1: everything far enough disappears).
    pub max_opacity: f32,
}

impl Default for HeightFog {
    /// Ground fog for a scene in metres: 1% per metre at height 0, thinning by e every
    /// 10 m.
    fn default() -> Self {
        Self {
            density: 0.01,
            base_height: 0.0,
            falloff: 0.1,
            albedo: Vec3::splat(0.95),
            sun_anisotropy: 0.6,
            start_distance: 0.0,
            max_opacity: 1.0,
        }
    }
}

impl HeightFog {
    /// Optical depth of the fog between `from` and `to` (world positions), as the shaders
    /// integrate it (from `start_distance` on, from `from`'s side).
    pub fn optical_depth(&self, from: Vec3, to: Vec3) -> f32 {
        let delta = to - from;
        let distance = delta.length();
        let len = distance - self.start_distance;
        if len <= 0.0 {
            return 0.0;
        }
        let d = delta / distance;
        let y = from.y + d.y * self.start_distance - self.base_height;
        let x = (self.falloff * d.y * len).max(-80.0);
        let g = if x.abs() > 1e-4 {
            (1.0 - (-x).exp()) / x
        } else {
            1.0 - 0.5 * x
        };
        self.density * (-self.falloff * y).clamp(-80.0, 80.0).exp() * len * g
    }

    /// How much of what is at `to` shows through the fog from `from` (with
    /// [`Self::max_opacity`]).
    pub fn transmittance(&self, from: Vec3, to: Vec3) -> f32 {
        (-self.optical_depth(from, to))
            .exp()
            .max(1.0 - self.max_opacity)
    }
}

/// `AtmosphereFrame` in `atmosphere_common.slang` (416 bytes).
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub(crate) struct AtmosphereConstants {
    world_from_clip: [f32; 16],
    aerial_world_from_clip: [f32; 16],
    rayleigh: [f32; 4],
    mie_scattering: [f32; 4],
    mie_extinction: [f32; 4],
    ozone: [f32; 4],
    ground_albedo: [f32; 4],
    planet: [f32; 4],
    sun_direction: [f32; 4],
    sun_illuminance: [f32; 4],
    camera_world: [f32; 4],
    camera_planet: [f32; 4],
    view: [f32; 4],
    fog_albedo: [f32; 4],
    fog: [f32; 4],
    fog_phase: [f32; 4],
    sun_color: [f32; 4],
    sh: u64,
    transmittance: u32,
    multiple_scattering: u32,
    sky_view: u32,
    aerial: u32,
    sky_cube: u32,
    sampler: u32,
    flags: u32,
    _pad: u32,
    size: [u32; 2],
}

const _: () = assert!(core::mem::size_of::<AtmosphereConstants>() == 416);

/// `Push` in `atmosphere.slang` (24 bytes).
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub(crate) struct AtmospherePush {
    pub frame: u64,
    pub target: u32,
    pub depth: u32,
    pub size: u32,
    pub _pad: u32,
}

/// The atmosphere's tables and passes, owned by an environment that has an atmosphere.
pub(crate) struct AtmosphereTables {
    /// Transmittance, multiple scattering, sky view, aerial perspective, cube.
    shaders: [ComputeShader<AtmospherePush>; 5],
    transmittance: Image,
    multiple_scattering: Image,
    sky_view: Image,
    aerial: Image,
    /// Parameters the transmittance and multiple scattering tables hold, if made.
    tables_of: Cell<Option<Atmosphere>>,
    /// The sky-view table's cos sun zenith and camera radius, if made.
    sky_view_of: Cell<Option<(f32, f32)>>,
    /// The radiance cube's sun direction, illuminance and camera radius, if made from the
    /// atmosphere.
    cube_of: Cell<Option<(Vec3, Vec3, f32)>>,
    /// The directional light's color for a sun direction, illuminance, altitude and
    /// atmosphere, kept while they do not change.
    sun_of: Cell<Option<(Vec3, Vec3, f32, Atmosphere, Vec3)>>,
}

impl core::fmt::Debug for AtmosphereTables {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AtmosphereTables").finish_non_exhaustive()
    }
}

/// What a view needs from the frame to draw its environment's atmosphere and fog.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AtmosphereView {
    /// World from clip of the drawn (jittered) projection.
    pub world_from_clip: Mat4,
    /// The same without jitter.
    pub unjittered_world_from_clip: Mat4,
    /// Camera position.
    pub camera: Vec3,
    /// Unit direction towards the sun.
    pub to_sun: Vec3,
    /// The directional light's color.
    pub light: Vec3,
    /// The color target's size.
    pub size: [u32; 2],
}

/// The graph's handles of the atmosphere's tables in a frame, for debugging views and
/// tests: each is a [`LUT_FORMAT`] image ([`TRANSMITTANCE_SIZE`],
/// [`MULTIPLE_SCATTERING_SIZE`]², [`SKY_VIEW_SIZE`], and the [`AERIAL_SLICES`]³ volume).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtmosphereTextures {
    /// Transmittance to the top of the atmosphere.
    pub transmittance: Texture,
    /// Multiple scattering.
    pub multiple_scattering: Texture,
    /// The sky around the camera.
    pub sky_view: Texture,
    /// Aerial perspective froxels (rgb: light scattered towards the camera per unit sun
    /// illuminance; a: opacity).
    pub aerial: Texture,
}

/// What the frame's passes read: the constants' address, the tables, and what the
/// full-screen pass samples.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AtmosphereFrame {
    /// Device address of this frame's [`AtmosphereConstants`].
    pub constants: u64,
    /// The tables, when the physical atmosphere is on (else fog alone).
    pub textures: Option<AtmosphereTextures>,
}

impl AtmosphereTables {
    pub(crate) fn new(
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
    ) -> Result<Self, RhiError> {
        let source = |spirv: &[u8], entry| ComputeSource::new(spirv, entry, &[]);
        let shaders = ComputeSource::create_all(
            device,
            bindless,
            [
                source(TRANSMITTANCE_LUT, c"transmittanceLut")?,
                source(MULTIPLE_SCATTERING_LUT, c"multipleScatteringLut")?,
                source(SKY_VIEW_LUT, c"skyViewLut")?,
                source(AERIAL_PERSPECTIVE, c"aerialPerspective")?,
                source(ATMOSPHERE_CUBE, c"atmosphereCube")?,
            ],
        )?;
        let flat = |allocator: &mut Allocator, [w, h]: [u32; 2]| {
            Image::new(
                device,
                allocator,
                bindless,
                LUT_FORMAT,
                w,
                h,
                1,
                ImageKind::Flat,
            )
        };
        let mut images: Vec<Image> = Vec::with_capacity(4);
        let result = (|| -> Result<(), RhiError> {
            images.push(flat(allocator, TRANSMITTANCE_SIZE)?);
            images.push(flat(
                allocator,
                [MULTIPLE_SCATTERING_SIZE, MULTIPLE_SCATTERING_SIZE],
            )?);
            images.push(flat(allocator, SKY_VIEW_SIZE)?);
            images.push(Image::new(
                device,
                allocator,
                bindless,
                LUT_FORMAT,
                AERIAL_SLICES,
                AERIAL_SLICES,
                1,
                ImageKind::Volume(AERIAL_SLICES),
            )?);
            Ok(())
        })();
        if let Err(e) = result {
            for mut image in images {
                // SAFETY: never used by the GPU.
                unsafe { image.destroy(device.raw(), allocator, bindless, 0) };
            }
            return Err(e);
        }
        let mut images = images.into_iter();
        let mut next = || {
            images.next().ok_or(RhiError::Vulkan {
                call: "atmosphere tables",
                result: vk::Result::ERROR_UNKNOWN,
            })
        };
        Ok(Self {
            shaders,
            transmittance: next()?,
            multiple_scattering: next()?,
            sky_view: next()?,
            aerial: next()?,
            tables_of: Cell::new(None),
            sky_view_of: Cell::new(None),
            cube_of: Cell::new(None),
            sun_of: Cell::new(None),
        })
    }

    /// The directional light's color for this frame: the sun through the atmosphere, or
    /// the light as it is.
    pub(crate) fn sun_color(&self, atmosphere: &Atmosphere, view: &AtmosphereView) -> Vec3 {
        if !atmosphere.drives_sun {
            return view.light;
        }
        let altitude = camera_altitude(atmosphere, view.camera);
        if let Some((to_sun, light, a, of, color)) = self.sun_of.get()
            && to_sun == view.to_sun
            && light == view.light
            && a == altitude
            && of == *atmosphere
        {
            return color;
        }
        let color = atmosphere.sun_light(view.light, view.to_sun, altitude);
        self.sun_of.set(Some((
            view.to_sun,
            view.light,
            altitude,
            *atmosphere,
            color,
        )));
        color
    }

    /// Whether the environment's radiance cube must be redrawn from the sky (the sun or the
    /// camera's altitude moved enough, or the tables changed); records the new state.
    pub(crate) fn cube_stale(&self, atmosphere: &Atmosphere, view: &AtmosphereView) -> bool {
        let radius = sky_radius(atmosphere, view.camera);
        let stale = match self.cube_of.get() {
            Some((to_sun, light, r)) => {
                to_sun.dot(view.to_sun) < CUBE_SUN_STEP_COS
                    || light != view.light
                    || (r - radius).abs() >= CUBE_ALTITUDE_STEP
                    || self
                        .tables_of
                        .get()
                        .is_none_or(|t| !t.same_tables(atmosphere))
            }
            None => true,
        };
        if stale {
            self.cube_of.set(Some((view.to_sun, view.light, radius)));
        }
        stale
    }

    /// The cube no longer holds this atmosphere's sky (another sky was drawn into it).
    pub(crate) fn forget_cube(&self) {
        self.cube_of.set(None);
    }

    /// Imports the tables into `fg` and declares the passes that rebuild what changed and
    /// the frame's aerial perspective. `cube`: the environment's radiance cube (mip 0's
    /// storage index), to draw the sky into first. `constants` is this frame's
    /// [`AtmosphereConstants`]; `params` its atmosphere.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_passes<'g>(
        &'g self,
        fg: &mut FrameGraph<'g>,
        bindless: &'g Bindless,
        atmosphere: &Atmosphere,
        constants: &AtmosphereConstants,
        address: u64,
        cube: Option<(Texture, u32)>,
    ) -> AtmosphereTextures {
        engine_profiler::zone!("render::atmosphere::add_passes");
        let transmittance = self.transmittance.import(fg, "atmosphere transmittance");
        let multiple = self
            .multiple_scattering
            .import(fg, "atmosphere multiple scattering");
        let sky_view = self.sky_view.import(fg, "atmosphere sky view");
        let aerial = self.aerial.import(fg, "atmosphere aerial perspective");
        let [
            transmittance_lut,
            multiple_lut,
            sky_view_lut,
            aerial_shader,
            cube_shader,
        ] = &self.shaders;
        let push = |target| AtmospherePush {
            frame: address,
            target,
            ..AtmospherePush::default()
        };
        let tables_stale = self
            .tables_of
            .get()
            .is_none_or(|t| !t.same_tables(atmosphere));
        if tables_stale {
            self.tables_of.set(Some(*atmosphere));
            let target = push(self.transmittance.storage[0]);
            fg.add_compute("atmosphere transmittance", bindless)
                .async_compute()
                .write(transmittance, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(
                    transmittance_lut,
                    Dispatch::Threads([TRANSMITTANCE_SIZE[0], TRANSMITTANCE_SIZE[1], 1]),
                    move |_| Some(target),
                );
            let target = push(self.multiple_scattering.storage[0]);
            fg.add_compute("atmosphere multiple scattering", bindless)
                .async_compute()
                .read(transmittance, Access::Sampled(ShaderStages::COMPUTE))
                .write(multiple, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(
                    multiple_lut,
                    Dispatch::Groups([MULTIPLE_SCATTERING_SIZE, MULTIPLE_SCATTERING_SIZE, 1]),
                    move |_| Some(target),
                );
        }
        let sky_key = (constants.sun_direction[3], constants.camera_planet[3]);
        let sky_stale = tables_stale
            || cube.is_some()
            || self.sky_view_of.get().is_none_or(|(mu, radius)| {
                mu != sky_key.0 || (radius - sky_key.1).abs() >= SKY_VIEW_ALTITUDE_STEP
            });
        if sky_stale {
            self.sky_view_of.set(Some(sky_key));
            let target = push(self.sky_view.storage[0]);
            fg.add_compute("atmosphere sky view", bindless)
                .async_compute()
                .read(transmittance, Access::Sampled(ShaderStages::COMPUTE))
                .read(multiple, Access::Sampled(ShaderStages::COMPUTE))
                .write(sky_view, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(
                    sky_view_lut,
                    Dispatch::Threads([SKY_VIEW_SIZE[0], SKY_VIEW_SIZE[1], 1]),
                    move |_| Some(target),
                );
        }
        if let Some((cube, storage)) = cube {
            let size = crate::environment::SOURCE_SIZE;
            let p = AtmospherePush {
                size,
                ..push(storage)
            };
            fg.add_compute("atmosphere sky cube", bindless)
                .async_compute()
                .read(sky_view, Access::Sampled(ShaderStages::COMPUTE))
                .write(cube, Access::StorageWrite(ShaderStages::COMPUTE))
                .dispatch(cube_shader, Dispatch::Threads([size, size, 6]), move |_| {
                    Some(p)
                });
        }
        let target = push(self.aerial.storage[0]);
        fg.add_compute("atmosphere aerial perspective", bindless)
            .async_compute()
            .read(transmittance, Access::Sampled(ShaderStages::COMPUTE))
            .read(multiple, Access::Sampled(ShaderStages::COMPUTE))
            .write(aerial, Access::StorageWrite(ShaderStages::COMPUTE))
            .dispatch(
                aerial_shader,
                Dispatch::Threads([AERIAL_SLICES, AERIAL_SLICES * AERIAL_SLICES, 1]),
                move |_| Some(target),
            );
        AtmosphereTextures {
            transmittance,
            multiple_scattering: multiple,
            sky_view,
            aerial,
        }
    }

    /// Bindless indices of the sampled tables: transmittance, multiple scattering, sky
    /// view, aerial perspective.
    pub(crate) fn indices(&self) -> [u32; 4] {
        [
            self.transmittance.sampled,
            self.multiple_scattering.sampled,
            self.sky_view.sampled,
            self.aerial.sampled,
        ]
    }

    /// # Safety
    ///
    /// The GPU no longer uses the tables.
    pub(crate) unsafe fn destroy(
        &mut self,
        device: &engine_rhi::ash::Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
    ) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            for image in [
                &mut self.transmittance,
                &mut self.multiple_scattering,
                &mut self.sky_view,
                &mut self.aerial,
            ] {
                image.destroy(device, allocator, bindless, 0);
            }
        }
    }
}

/// The camera's altitude above the ground, km.
fn camera_altitude(atmosphere: &Atmosphere, camera: Vec3) -> f32 {
    (atmosphere.origin_altitude + camera.y * atmosphere.meters_per_unit * 1e-3).max(0.0)
}

/// Radius (km from the planet's center) the sky is seen from: the camera's, quantized for
/// the sky-view table and at least 1 m above the ground.
fn sky_radius(atmosphere: &Atmosphere, camera: Vec3) -> f32 {
    let altitude = camera_altitude(atmosphere, camera).max(0.001);
    let steps = (altitude / SKY_VIEW_ALTITUDE_STEP).round().max(0.1);
    atmosphere.ground_radius + steps * SKY_VIEW_ALTITUDE_STEP
}

/// This frame's constants. `atmosphere`: the physical sky, or `None` for fog alone (over
/// the environment's cube); `indices`: the tables' bindless indices.
#[allow(clippy::too_many_arguments)]
pub(crate) fn constants(
    atmosphere: Option<&Atmosphere>,
    fog: Option<&HeightFog>,
    view: &AtmosphereView,
    sun_color: Vec3,
    sh: u64,
    sky_cube: u32,
    intensity: f32,
    indices: [u32; 4],
) -> AtmosphereConstants {
    let km = atmosphere.map_or(1e-3, |a| a.meters_per_unit * 1e-3);
    let to_sun = view.to_sun.normalize_or(Vec3::Y);
    let mut c = AtmosphereConstants {
        world_from_clip: view.world_from_clip.to_cols_array(),
        aerial_world_from_clip: view.unjittered_world_from_clip.to_cols_array(),
        sun_direction: to_sun.extend(to_sun.y).to_array(),
        sun_illuminance: view.light.extend(0.0).to_array(),
        camera_world: view.camera.extend(km).to_array(),
        view: [1.0, intensity, 0.0, 0.0],
        sun_color: sun_color.extend(0.0).to_array(),
        sh,
        transmittance: indices[0],
        multiple_scattering: indices[1],
        sky_view: indices[2],
        aerial: indices[3],
        sky_cube,
        sampler: Bindless::sampler(engine_rhi::SamplerKind::LinearClamp),
        size: view.size,
        ..AtmosphereConstants::default()
    };
    if let Some(a) = atmosphere {
        c.flags |= FLAG_SKY;
        c.rayleigh = a
            .rayleigh_scattering
            .extend(-1.0 / a.rayleigh_scale_height)
            .to_array();
        c.mie_scattering = a
            .mie_scattering
            .extend(-1.0 / a.mie_scale_height)
            .to_array();
        c.mie_extinction = (a.mie_scattering + a.mie_absorption)
            .extend(a.mie_anisotropy)
            .to_array();
        c.ozone = a.ozone_absorption.extend(a.ozone_peak_altitude).to_array();
        c.ground_albedo = a.ground_albedo.extend(a.ozone_half_width).to_array();
        c.planet = [
            a.ground_radius,
            a.top_radius,
            a.multiple_scattering,
            a.sun_angular_radius.cos(),
        ];
        let altitude = camera_altitude(a, view.camera).max(0.001);
        let planet = Vec3::new(
            view.camera.x * km,
            a.ground_radius + altitude,
            view.camera.z * km,
        );
        c.camera_planet = planet.extend(sky_radius(a, view.camera)).to_array();
        c.view[0] = a.aerial_perspective_scale.max(0.0);
    } else {
        // The disk of the gradient sky (as the forward renderer's sky pass).
        c.planet[3] = 0.999_988_9;
    }
    if let Some(f) = fog {
        c.flags |= FLAG_FOG;
        c.fog_albedo = f.albedo.extend(f.density.max(0.0)).to_array();
        c.fog = [
            f.base_height,
            f.falloff,
            f.start_distance.max(0.0),
            f.max_opacity.clamp(0.0, 1.0),
        ];
        c.fog_phase = [f.sun_anisotropy.clamp(-0.99, 0.99), 0.0, 0.0, 0.0];
    }
    c
}

/// Declares the full-screen pass after the opaque surfaces: the sky where nothing was
/// drawn, aerial perspective and fog over the rest (`pipeline`: `compositeVertex` /
/// `compositeFragment`, premultiplied blending, no depth attachment).
pub(crate) fn add_composite_pass<'g>(
    fg: &mut FrameGraph<'g>,
    bindless: &'g Bindless,
    pipeline: &'g Pipeline,
    color: Texture,
    depth: Texture,
    environment: &EnvironmentFrame,
    atmosphere: &AtmosphereFrame,
) {
    let mut pass = fg
        .add_pass("atmosphere")
        .color(color, LoadOp::Load, StoreOp::Store)
        .read(depth, Access::Sampled(ShaderStages::FRAGMENT))
        .read(environment.source, Access::Sampled(ShaderStages::FRAGMENT))
        .read(environment.sh, Access::StorageRead(ShaderStages::FRAGMENT));
    if let Some(t) = atmosphere.textures {
        pass = pass
            .read(t.transmittance, Access::Sampled(ShaderStages::FRAGMENT))
            .read(t.sky_view, Access::Sampled(ShaderStages::FRAGMENT))
            .read(t.aerial, Access::Sampled(ShaderStages::FRAGMENT));
    }
    let address = atmosphere.constants;
    pass.execute(move |ctx| {
        let Some(depth) = ctx.sampled_index(depth) else {
            return;
        };
        let push = AtmospherePush {
            frame: address,
            depth,
            ..AtmospherePush::default()
        };
        let area = ctx.render_area();
        // SAFETY: recording inside the pass's rendering scope; the constants are in the
        // ring, the tables and the depth are read by the pass.
        unsafe {
            pipeline.bind(ctx.cmd);
            bindless.bind(ctx.cmd, vk::PipelineBindPoint::GRAPHICS);
            ctx.device.cmd_set_viewport(
                ctx.cmd,
                0,
                &[vk::Viewport {
                    x: area.offset.x as f32,
                    y: area.offset.y as f32,
                    width: area.extent.width as f32,
                    height: area.extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            ctx.device.cmd_set_scissor(ctx.cmd, 0, &[area]);
            bindless.push_constants(ctx.cmd, bytemuck::bytes_of(&push));
            ctx.device.cmd_draw(ctx.cmd, 3, 1, 0, 0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zenith_transmittance_has_a_closed_form() {
        // Straight up from the ground the optical depth is each constituent's density
        // integrated over height: Rayleigh H_R (1 − e^(−100/H_R)), Mie H_M (…), ozone the
        // tent's area (its half width).
        let a = Atmosphere::earth();
        let rayleigh = a.rayleigh_scale_height * (1.0 - (-100.0 / a.rayleigh_scale_height).exp());
        let mie = a.mie_scale_height * (1.0 - (-100.0 / a.mie_scale_height).exp());
        let depth = a.rayleigh_scattering * rayleigh
            + (a.mie_scattering + a.mie_absorption) * mie
            + a.ozone_absorption * a.ozone_half_width;
        let expected = Vec3::new((-depth.x).exp(), (-depth.y).exp(), (-depth.z).exp());
        let t = a.transmittance(0.0, 1.0);
        assert!(
            (t - expected).abs().max_element() < 1e-4,
            "{t} vs {expected}"
        );
        // Blue is lost most: (0.940, 0.868, 0.762).
        assert!(t.x > t.y && t.y > t.z);
    }

    #[test]
    fn the_sun_reddens_and_sets() {
        let a = Atmosphere::earth();
        let white = Vec3::ONE;
        let noon = a.sun_light(white, Vec3::Y, 0.0);
        let elevation = |deg: f32| {
            let r = deg.to_radians();
            Vec3::new(r.cos(), r.sin(), 0.0)
        };
        let evening = a.sun_light(white, elevation(5.0), 0.0);
        let sunset = a.sun_light(white, elevation(0.5), 0.0);
        let night = a.sun_light(white, elevation(-2.0), 0.0);
        // At noon nearly white and bright.
        assert!(noon.min_element() > 0.7, "{noon}");
        assert!(noon.x / noon.z < 1.3, "{noon}");
        // Lower, dimmer and redder.
        assert!(evening.length() < noon.length());
        assert!(evening.x / evening.z > 2.0 * noon.x / noon.z, "{evening}");
        assert!(sunset.x / sunset.z > evening.x / evening.z, "{sunset}");
        assert!(sunset.x > sunset.y && sunset.y > sunset.z, "{sunset}");
        assert_eq!(night, Vec3::ZERO);
        // Half the disk above the horizon: half its light through the horizontal air.
        let horizon = a.sun_light(white, elevation(0.0), 0.0);
        let expected = a.transmittance(0.0, 0.0) * 0.5;
        assert!(
            (horizon - expected).abs().max_element() < 1e-3 * expected.x,
            "{horizon} vs {expected}"
        );
    }

    #[test]
    fn fog_optical_depth_matches_numerical_integration() {
        let fog = HeightFog {
            density: 0.03,
            base_height: 2.0,
            falloff: 0.25,
            start_distance: 3.0,
            ..HeightFog::default()
        };
        let cases = [
            (Vec3::new(0.0, 1.0, 0.0), Vec3::new(40.0, 1.0, 0.0)),
            (Vec3::new(0.0, 1.0, 0.0), Vec3::new(30.0, 12.0, -5.0)),
            (Vec3::new(0.0, 15.0, 0.0), Vec3::new(10.0, -2.0, 25.0)),
            (Vec3::new(0.0, 1.0, 0.0), Vec3::new(1.0, 1.5, 1.0)),
        ];
        for (from, to) in cases {
            let delta = to - from;
            let distance = delta.length();
            let d = delta / distance;
            let steps = 100_000;
            let mut numeric = 0.0f64;
            for i in 0..steps {
                let t = (i as f64 + 0.5) / steps as f64 * f64::from(distance);
                if t < f64::from(fog.start_distance) {
                    continue;
                }
                let y = f64::from(from.y) + f64::from(d.y) * t;
                numeric += f64::from(fog.density)
                    * (-f64::from(fog.falloff) * (y - f64::from(fog.base_height))).exp()
                    * f64::from(distance)
                    / steps as f64;
            }
            let analytic = fog.optical_depth(from, to);
            assert!(
                (f64::from(analytic) - numeric).abs() < 1e-3 * numeric.max(1e-3),
                "{from} → {to}: {analytic} vs {numeric}"
            );
        }
        // Uniform fog: density × distance.
        let uniform = HeightFog {
            falloff: 0.0,
            start_distance: 0.0,
            ..fog
        };
        let od = uniform.optical_depth(Vec3::ZERO, Vec3::new(3.0, 4.0, 0.0));
        assert!((od - 0.15).abs() < 1e-6, "{od}");
    }
}
