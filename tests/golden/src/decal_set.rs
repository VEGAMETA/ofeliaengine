//! Decals over the procedural scene (ADR 0082) for `Shot::decals`: a scorch mark across
//! the corner of the floor and the wall, a wet patch and a painted arrow over it on the
//! floor, a ring wrapping a sphere, a metal plate on the wall. Their textures are made
//! here, from integer hashes (the same bytes on every machine).

use core::cell::Cell;
use engine_core::math::{Mat4, Quat, Vec3};
use engine_render::forward::Decal;
use engine_render::graph::FrameGraph;
use engine_render::texture::GpuTexture;
use engine_render::upload::UploadRing;
use engine_rhi::ash::vk;
use engine_rhi::{Allocator, Bindless, Device};

/// Texels per side of the decal textures.
const SIZE: u32 = 128;

/// The decals and their textures.
pub(crate) struct DecalSet {
    textures: Vec<(GpuTexture, Vec<u8>)>,
    uploaded: Cell<bool>,
    /// The decals, for `SceneInput::decals`.
    pub(crate) decals: Vec<Decal>,
}

impl DecalSet {
    /// Makes the textures (uploaded by the first [`Self::upload`]) and the decals.
    ///
    /// # Panics
    ///
    /// When a texture cannot be made.
    pub(crate) fn new(device: &Device, allocator: &mut Allocator, bindless: &Bindless) -> Self {
        let images = [
            (vk::Format::R8G8B8A8_SRGB, blot_albedo()),
            (vk::Format::R8G8B8A8_UNORM, normals(&blot_height())),
            (vk::Format::R8G8B8A8_SRGB, arrow_albedo()),
            (vk::Format::R8G8B8A8_SRGB, ring_albedo()),
            (vk::Format::R8G8B8A8_UNORM, normals(&plate_height())),
        ];
        let textures: Vec<(GpuTexture, Vec<u8>)> = images
            .into_iter()
            .map(|(format, pixels)| {
                let texture = GpuTexture::new(device, allocator, bindless, format, SIZE, SIZE)
                    .expect("decal texture");
                (texture, pixels)
            })
            .collect();
        let index = |k: usize| Some(textures[k].0.sampled_index());
        let down = Quat::from_rotation_x(-core::f32::consts::FRAC_PI_2);
        let decals = vec![
            // Across the corner: projected diagonally, so the floor and the wall both face
            // it at 45 degrees.
            Decal {
                world_from_decal: Mat4::from_scale_rotation_translation(
                    Vec3::new(2.2, 2.2, 1.6),
                    Quat::from_rotation_x(-core::f32::consts::FRAC_PI_4),
                    Vec3::new(-5.4, 0.0, -2.9),
                ),
                albedo: index(0),
                normal: index(1),
                normal_scale: 0.6,
                roughness: 0.9,
                surface: 0.7,
                ..Decal::default()
            },
            // A wet patch: darker and glossy, no texture.
            Decal {
                world_from_decal: Mat4::from_scale_rotation_translation(
                    Vec3::new(1.4, 1.0, 0.5),
                    Quat::from_rotation_y(0.4) * down,
                    Vec3::new(0.3, 0.0, 1.7),
                ),
                tint: [0.25, 0.25, 0.27, 0.9],
                roughness: 0.12,
                surface: 1.0,
                ..Decal::default()
            },
            // Paint over the patch's edge, drawn after it.
            Decal {
                world_from_decal: Mat4::from_scale_rotation_translation(
                    Vec3::new(0.8, 1.6, 0.5),
                    down,
                    Vec3::new(-0.35, 0.0, 1.6),
                ),
                albedo: index(2),
                tint: [0.95, 0.75, 0.1, 1.0],
                roughness: 0.5,
                surface: 0.6,
                order: 1,
                ..Decal::default()
            },
            // A ring around the front of the middle sphere, fading at its sides.
            Decal {
                world_from_decal: Mat4::from_scale_rotation_translation(
                    Vec3::new(0.7, 0.7, 1.0),
                    Quat::IDENTITY,
                    Vec3::new(0.0, 0.5, -0.75),
                ),
                albedo: index(3),
                ..Decal::default()
            },
            // A riveted metal plate on the wall.
            Decal {
                world_from_decal: Mat4::from_scale_rotation_translation(
                    Vec3::new(1.2, 0.8, 0.4),
                    Quat::IDENTITY,
                    Vec3::new(2.4, 1.0, -2.9),
                ),
                normal: index(4),
                tint: [0.6, 0.6, 0.62, 1.0],
                roughness: 0.25,
                metalness: 1.0,
                surface: 1.0,
                ..Decal::default()
            },
        ];
        Self {
            textures,
            uploaded: Cell::new(false),
            decals,
        }
    }

    /// Uploads the textures the first time.
    ///
    /// # Panics
    ///
    /// When the ring has no room.
    pub(crate) fn upload<'g>(&'g self, fg: &mut FrameGraph<'g>, ring: &mut UploadRing) {
        if self.uploaded.replace(true) {
            return;
        }
        for (texture, pixels) in &self.textures {
            texture.upload(fg, ring, pixels).expect("decal upload");
        }
    }

    /// Frees the textures.
    ///
    /// # Safety
    ///
    /// The GPU no longer uses them.
    pub(crate) unsafe fn destroy(
        &mut self,
        device: &Device,
        allocator: &mut Allocator,
        bindless: &Bindless,
    ) {
        for (texture, _) in &mut self.textures {
            // SAFETY: guaranteed by the caller.
            unsafe { texture.destroy(device, allocator, bindless, 0) };
        }
    }
}

/// A value in [0, 1) for integer coordinates.
fn hash(x: i32, y: i32, seed: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8DA6_B343)
        ^ (y as u32).wrapping_mul(0xD816_3841)
        ^ seed.wrapping_mul(0xCB1A_B31F);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5BD1_E995);
    h ^= h >> 15;
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// Smooth value noise in [0, 1), cells one unit wide.
fn noise(x: f32, y: f32, seed: u32) -> f32 {
    let (x0, y0) = (x.floor(), y.floor());
    let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
    let (sx, sy) = (smooth(x - x0), smooth(y - y0));
    let (i, j) = (x0 as i32, y0 as i32);
    let top = hash(i, j, seed) + (hash(i + 1, j, seed) - hash(i, j, seed)) * sx;
    let bottom = hash(i, j + 1, seed) + (hash(i + 1, j + 1, seed) - hash(i, j + 1, seed)) * sx;
    top + (bottom - top) * sy
}

/// Two octaves of [`noise`].
fn fbm(x: f32, y: f32, seed: u32) -> f32 {
    (noise(x, y, seed) * 2.0 + noise(x * 2.0, y * 2.0, seed + 1)) / 3.0
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Every texel's (u, v) in [-1, 1] (v down), through `f`, as one image.
fn image(f: impl Fn(f32, f32) -> [f32; 4], srgb: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let u = (x as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0;
            let v = (y as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0;
            let texel = f(u, v);
            for (k, c) in texel.into_iter().enumerate() {
                let c = c.clamp(0.0, 1.0);
                let c = if srgb && k < 3 { to_srgb(c) } else { c };
                out.push((c * 255.0).round() as u8);
            }
        }
    }
    out
}

fn to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// A heat-darkened blot with a ragged rim.
fn blot_albedo() -> Vec<u8> {
    image(
        |u, v| {
            let r = (u * u + v * v).sqrt();
            let edge = 0.6 + 0.35 * fbm(u * 3.0 + 7.0, v * 3.0 + 3.0, 1);
            let alpha = smoothstep(edge, edge - 0.25, r);
            let t = smoothstep(0.05, 0.8, r / edge);
            let core = [0.02, 0.018, 0.015];
            let rim = [0.2, 0.11, 0.05];
            let grain = 0.8 + 0.4 * noise(u * 24.0, v * 24.0, 2);
            [
                (core[0] + (rim[0] - core[0]) * t) * grain,
                (core[1] + (rim[1] - core[1]) * t) * grain,
                (core[2] + (rim[2] - core[2]) * t) * grain,
                alpha,
            ]
        },
        true,
    )
}

/// The blot's blistered surface, as heights in [0, 1] (texel order).
fn blot_height() -> Vec<f32> {
    heights(|u, v| {
        let r = (u * u + v * v).sqrt();
        fbm(u * 8.0, v * 8.0, 3) * smoothstep(0.95, 0.4, r)
    })
}

/// An arrow pointing up the image (the decal's +y), worn in places.
fn arrow_albedo() -> Vec<u8> {
    image(
        |u, v| {
            let head = (-0.9..=-0.2).contains(&v) && u.abs() <= (v + 0.9) / 0.7 * 0.55;
            let shaft = (-0.2..=0.9).contains(&v) && u.abs() <= 0.18;
            let worn = noise(u * 10.0 + 5.0, v * 10.0, 4) > 0.8;
            let alpha = if head || shaft {
                if worn { 0.35 } else { 1.0 }
            } else {
                0.0
            };
            [1.0, 1.0, 1.0, alpha]
        },
        true,
    )
}

/// A white ring.
fn ring_albedo() -> Vec<u8> {
    image(
        |u, v| {
            let r = (u * u + v * v).sqrt();
            let texel = 2.0 / SIZE as f32;
            let alpha = smoothstep(0.55 - texel, 0.55, r) * smoothstep(0.85, 0.85 - texel, r);
            [1.0, 1.0, 1.0, alpha]
        },
        true,
    )
}

/// The plate: bevelled edges and four rivets, as heights.
fn plate_height() -> Vec<f32> {
    heights(|u, v| {
        let edge = (1.0 - u.abs()).min(1.0 - v.abs());
        let mut h = smoothstep(0.0, 0.12, edge) * 0.6;
        for (cx, cy) in [(-0.8, -0.7), (0.8, -0.7), (-0.8, 0.7), (0.8, 0.7)] {
            let d2 = (u - cx) * (u - cx) + (v - cy) * (v - cy);
            let r2 = 0.07f32 * 0.07;
            if d2 < r2 {
                h += (1.0 - d2 / r2).sqrt() * 0.4;
            }
        }
        h
    })
}

fn heights(f: impl Fn(f32, f32) -> f32) -> Vec<f32> {
    let mut out = Vec::with_capacity((SIZE * SIZE) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let u = (x as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0;
            let v = (y as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0;
            out.push(f(u, v));
        }
    }
    out
}

/// A tangent-space normal map (x right, y up the image) of `heights`, one unit of height
/// rising over a tenth of the texture.
fn normals(heights: &[f32]) -> Vec<u8> {
    let n = SIZE as i32;
    let at = |x: i32, y: i32| heights[(y.clamp(0, n - 1) * n + x.clamp(0, n - 1)) as usize];
    let scale = SIZE as f32 / 10.0 / 2.0;
    let mut out = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let dx = (at(x + 1, y) - at(x - 1, y)) * scale;
            // v grows down the image, the normal map's y up.
            let dy = (at(x, y - 1) - at(x, y + 1)) * scale;
            let normal = Vec3::new(-dx, -dy, 1.0).normalize();
            for c in normal.to_array() {
                out.push(((c * 0.5 + 0.5) * 255.0).round() as u8);
            }
            out.push(255);
        }
    }
    out
}
