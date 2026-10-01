//! Compressed clips (the runtime format) and their sampling.
//!
//! A clip is sampled uniformly in time. Each joint's rotation, translation and scale is
//! one of:
//!
//! - **default**: equal to the rest pose (or to identity in an additive clip) and not
//!   stored;
//! - **constant**: one value, stored at full precision;
//! - **animated**: one value per sample, range-reduced over the clip (each component
//!   relative to its minimum and extent) and quantized to the track's own bit width
//!   (3–16 bits per component, or 32 for raw floats). A rotation drops one component of
//!   its quaternion, the one whose magnitude stays largest over the clip (made positive
//!   and recomputed from the other three, so it is never close to 0 where recomputing
//!   it loses precision); a rotation without such a component (none stays above 0.25)
//!   or stored raw keeps all four.
//!
//! Samples are stored frame by frame (every animated track of sample 0, then of sample
//! 1...), so sampling a time reads two contiguous bit strings. The bit widths are chosen by
//! the compressor ([`crate::compress`]) to keep the error of every joint, measured in
//! object space on virtual vertices around it, under a threshold: the approach of
//! Frechette's Animation Compression Library.

use crate::FormatError;
use crate::bytes::{Reader, Writer};
use crate::pose::{Pose, Transform, nlerp};
use engine_core::math::{Quat, Vec3A, Vec4};

/// Which component of a joint's transform a track animates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Channel {
    /// Rotation.
    Rotation = 0,
    /// Translation.
    Translation = 1,
    /// Scale.
    Scale = 2,
}

impl Channel {
    /// All three, in storage order.
    pub const ALL: [Channel; 3] = [Channel::Rotation, Channel::Translation, Channel::Scale];

    fn from_u8(v: u8) -> Option<Self> {
        Self::ALL.get(v as usize).copied()
    }
}

/// A track with one value for the whole clip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ConstantTrack {
    pub(crate) joint: u16,
    pub(crate) channel: Channel,
    /// x, y, z (and w for rotations).
    pub(crate) value: Vec4,
}

/// A track with one quantized value per sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AnimatedTrack {
    /// Range minimum per stored component.
    pub(crate) min: Vec4,
    /// Range extent / (2^bits − 1) per stored component (unused for raw floats).
    pub(crate) scale: Vec4,
    pub(crate) joint: u16,
    pub(crate) channel: Channel,
    /// Bits per component: 1-16, or 32 (raw `f32`).
    pub(crate) bits: u8,
    /// Rotations: the quaternion component not stored (0-3: x, y, z, w; recomputed,
    /// positive), or [`ALL_FOUR`]; translations and scales: 3 (x, y, z).
    pub(crate) layout: u8,
    /// First bit of the track's first component within a frame.
    pub(crate) offset: u32,
}

/// [`AnimatedTrack::layout`] of a rotation that stores all four components.
pub(crate) const ALL_FOUR: u8 = 4;

/// Bits per component an animated track may use.
pub(crate) const RAW_BITS: u8 = 32;

/// A compressed animation clip of one skeleton (see the module documentation).
#[derive(Debug, Clone, PartialEq)]
pub struct CompressedClip {
    pub(crate) name: String,
    pub(crate) duration: f32,
    pub(crate) sample_count: u32,
    pub(crate) joint_count: u16,
    pub(crate) additive: bool,
    pub(crate) constants: Vec<ConstantTrack>,
    /// Sorted by joint, then channel: the order of their bits within a frame.
    pub(crate) animated: Vec<AnimatedTrack>,
    pub(crate) frame_bits: u32,
    /// The frames, then 8 bytes of padding (64-bit reads at any bit).
    pub(crate) data: Vec<u8>,
}

/// Reads `bits` (≤ 32) bits at bit `at` of `data`; 0 past the end.
#[inline(always)]
pub(crate) fn read_bits(data: &[u8], at: usize, bits: u32) -> u32 {
    let byte = at >> 3;
    let word = match data.get(byte..byte + 8) {
        Some(b) => u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
        None => 0,
    };
    ((word >> (at & 7)) & ((1u64 << bits) - 1)) as u32
}

/// The quantized value of `v` in `bits` bits over `[min, min + scale · (2^bits − 1)]`.
#[inline]
pub(crate) fn quantize(v: f32, min: f32, scale: f32, bits: u8) -> u32 {
    if bits == RAW_BITS {
        return v.to_bits();
    }
    let max = ((1u32 << bits) - 1) as f32;
    if scale <= 0.0 {
        return 0;
    }
    ((v - min) / scale).round().clamp(0.0, max) as u32
}

/// The value of a quantized component.
#[inline(always)]
pub(crate) fn dequantize(q: u32, min: f32, scale: f32, bits: u8) -> f32 {
    if bits == RAW_BITS {
        f32::from_bits(q)
    } else {
        min + scale * q as f32
    }
}

/// Components stored per sample: four for rotations stored whole, else three.
#[inline(always)]
pub(crate) fn component_count(layout: u8) -> u32 {
    if layout == ALL_FOUR { 4 } else { 3 }
}

/// The stored components of a quaternion in `layout` (the first three, or four).
#[inline]
pub(crate) fn stored_components(q: Vec4, layout: u8) -> Vec4 {
    match layout {
        0 => Vec4::new(q.y, q.z, q.w, 0.0),
        1 => Vec4::new(q.x, q.z, q.w, 0.0),
        2 => Vec4::new(q.x, q.y, q.w, 0.0),
        3 => Vec4::new(q.x, q.y, q.z, 0.0),
        _ => q,
    }
}

/// The rotation of stored components in `layout`: the dropped one recomputed (≥ 0). Not
/// normalized (sampling's nlerp normalizes).
#[inline(always)]
pub(crate) fn rotation_from_stored(v: Vec4, layout: u8) -> Quat {
    if layout == ALL_FOUR {
        return Quat::from_vec4(v);
    }
    let d = (1.0 - (v.x * v.x + v.y * v.y + v.z * v.z)).max(0.0).sqrt();
    let q = match layout {
        0 => Vec4::new(d, v.x, v.y, v.z),
        1 => Vec4::new(v.x, d, v.y, v.z),
        2 => Vec4::new(v.x, v.y, d, v.z),
        _ => Vec4::new(v.x, v.y, v.z, d),
    };
    Quat::from_vec4(q)
}

impl AnimatedTrack {
    /// The stored components of a frame starting at bit `frame` (the fourth 0 for
    /// tracks of three).
    #[inline(always)]
    fn read(&self, data: &[u8], frame: usize) -> Vec4 {
        let at = frame + self.offset as usize;
        let bits = u32::from(self.bits);
        let n = component_count(self.layout);
        if self.bits == RAW_BITS {
            let c = |k: u32| f32::from_bits(read_bits(data, at + 32 * k as usize, 32));
            return Vec4::new(c(0), c(1), c(2), if n == 4 { c(3) } else { 0.0 });
        }
        let c = |k: u32| read_bits(data, at + (k * bits) as usize, bits) as f32;
        let q = Vec4::new(c(0), c(1), c(2), if n == 4 { c(3) } else { 0.0 });
        self.min + self.scale * q
    }

    /// Bits per sample.
    pub(crate) fn sample_bits(&self) -> u32 {
        component_count(self.layout) * u32::from(self.bits)
    }
}

impl CompressedClip {
    /// Name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Length in seconds.
    pub fn duration(&self) -> f32 {
        self.duration
    }

    /// Uniform samples over the duration (the first at 0, the last at the end).
    pub fn sample_count(&self) -> u32 {
        self.sample_count
    }

    /// Samples per second.
    pub fn sample_rate(&self) -> f32 {
        if self.duration > 0.0 {
            (self.sample_count.saturating_sub(1)) as f32 / self.duration
        } else {
            0.0
        }
    }

    /// Joints of the skeleton it animates.
    pub fn joint_count(&self) -> usize {
        self.joint_count as usize
    }

    /// Whether it holds deltas over a base pose, for additive layers (its default tracks
    /// are identity, not the rest pose).
    pub fn is_additive(&self) -> bool {
        self.additive
    }

    /// Bytes in memory: the frames and the track tables.
    pub fn size_bytes(&self) -> usize {
        self.data.len()
            + self.animated.len() * size_of::<AnimatedTrack>()
            + self.constants.len() * size_of::<ConstantTrack>()
            + size_of::<Self>()
    }

    /// (constant, animated) track counts; the rest of the `3 × joints` are default.
    pub fn track_counts(&self) -> (usize, usize) {
        (self.constants.len(), self.animated.len())
    }

    /// The two samples around `time` and the weight of the second.
    #[inline]
    fn frames(&self, time: f32) -> (usize, usize, f32) {
        let last = self.sample_count.saturating_sub(1);
        if last == 0 || self.duration <= 0.0 {
            return (0, 0, 0.0);
        }
        let f = (time / self.duration).clamp(0.0, 1.0) * last as f32;
        let f0 = (f as u32).min(last);
        let f1 = (f0 + 1).min(last);
        let fb = self.frame_bits as usize;
        (f0 as usize * fb, f1 as usize * fb, f - f0 as f32)
    }

    /// The pose at `time` (clamped to the clip) into `out`. Joints the clip does not
    /// animate get `rest` (identity for additive clips). Allocates nothing when `out` has
    /// the skeleton's length.
    pub fn sample(&self, time: f32, rest: &Pose, out: &mut Pose) {
        engine_profiler::zone!("anim::sample");
        if self.additive {
            out.translations.fill(Vec3A::ZERO);
            out.rotations.fill(Quat::IDENTITY);
            out.scales.fill(Vec3A::ONE);
        } else {
            out.copy_from(rest);
        }
        for c in &self.constants {
            let j = c.joint as usize;
            if j >= out.len() {
                continue;
            }
            match c.channel {
                Channel::Rotation => out.rotations[j] = Quat::from_vec4(c.value),
                Channel::Translation => out.translations[j] = c.value.truncate().into(),
                Channel::Scale => out.scales[j] = c.value.truncate().into(),
            }
        }
        let (a, b, t) = self.frames(time);
        let data = &self.data[..];
        for track in &self.animated {
            let j = track.joint as usize;
            if j >= out.len() {
                continue;
            }
            let (va, vb) = (track.read(data, a), track.read(data, b));
            match track.channel {
                Channel::Rotation => {
                    out.rotations[j] = nlerp(
                        rotation_from_stored(va, track.layout),
                        rotation_from_stored(vb, track.layout),
                        t,
                    );
                }
                Channel::Translation => out.translations[j] = va.truncate().lerp(vb.truncate(), t).into(),
                Channel::Scale => out.scales[j] = va.truncate().lerp(vb.truncate(), t).into(),
            }
        }
    }

    /// Joint `j`'s transform at `time` alone (root motion, single bones): `rest` for its
    /// default components.
    pub fn sample_joint(&self, j: usize, time: f32, rest: Transform) -> Transform {
        let mut out = if self.additive {
            Transform::IDENTITY
        } else {
            rest
        };
        for c in self.constants.iter().filter(|c| c.joint as usize == j) {
            match c.channel {
                Channel::Rotation => out.rotation = Quat::from_vec4(c.value),
                Channel::Translation => out.translation = c.value.truncate().into(),
                Channel::Scale => out.scale = c.value.truncate().into(),
            }
        }
        let (a, b, t) = self.frames(time);
        let first = self.animated.partition_point(|k| (k.joint as usize) < j);
        for track in self.animated[first..]
            .iter()
            .take_while(|k| k.joint as usize == j)
        {
            let (va, vb) = (track.read(&self.data, a), track.read(&self.data, b));
            match track.channel {
                Channel::Rotation => {
                    out.rotation = nlerp(
                        rotation_from_stored(va, track.layout),
                        rotation_from_stored(vb, track.layout),
                        t,
                    );
                }
                Channel::Translation => out.translation = va.truncate().lerp(vb.truncate(), t).into(),
                Channel::Scale => out.scale = va.truncate().lerp(vb.truncate(), t).into(),
            }
        }
        out
    }

    /// The cooked bytes (see [`Self::from_bytes`]).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u32(CLIP_MAGIC);
        w.str(&self.name);
        w.f32(self.duration);
        w.u32(self.sample_count);
        w.u16(self.joint_count);
        w.u8(u8::from(self.additive));
        w.u32(self.constants.len() as u32);
        for c in &self.constants {
            w.u16(c.joint);
            w.u8(c.channel as u8);
            w.f32s(&c.value.to_array());
        }
        w.u32(self.animated.len() as u32);
        for t in &self.animated {
            w.u16(t.joint);
            w.u8(t.channel as u8);
            w.u8(t.bits);
            w.u8(t.layout);
            w.u32(t.offset);
            w.f32s(&t.min.to_array());
            w.f32s(&t.scale.to_array());
        }
        w.u32(self.frame_bits);
        w.u32(self.data.len() as u32);
        w.raw(&self.data);
        w.bytes
    }

    /// A clip from [`Self::to_bytes`], validated: every track inside the frames, the data
    /// as long as the frames say.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut r = Reader { bytes };
        if r.u32()? != CLIP_MAGIC {
            return Err(FormatError("not a clip"));
        }
        let name = r.str()?;
        let duration = r.f32()?;
        let sample_count = r.u32()?;
        let joint_count = r.u16()?;
        let additive = r.u8()? != 0;
        if duration < 0.0 || sample_count == 0 {
            return Err(FormatError("bad duration"));
        }
        let n = r.u32()? as usize;
        if n > 3 * joint_count as usize {
            return Err(FormatError("too many constant tracks"));
        }
        let mut constants = Vec::with_capacity(n);
        for _ in 0..n {
            let joint = r.u16()?;
            let channel = Channel::from_u8(r.u8()?).ok_or(FormatError("bad channel"))?;
            let value = Vec4::from_array(r.f32s::<4>()?);
            if joint >= joint_count {
                return Err(FormatError("track of a missing joint"));
            }
            constants.push(ConstantTrack {
                joint,
                channel,
                value,
            });
        }
        let n = r.u32()? as usize;
        if n > 3 * joint_count as usize {
            return Err(FormatError("too many animated tracks"));
        }
        let mut animated = Vec::with_capacity(n);
        for _ in 0..n {
            let joint = r.u16()?;
            let channel = Channel::from_u8(r.u8()?).ok_or(FormatError("bad channel"))?;
            let bits = r.u8()?;
            let layout = r.u8()?;
            let offset = r.u32()?;
            let min = Vec4::from_array(r.f32s::<4>()?);
            let scale = Vec4::from_array(r.f32s::<4>()?);
            let layout_ok = match channel {
                Channel::Rotation => layout <= ALL_FOUR,
                Channel::Translation | Channel::Scale => layout == 3,
            };
            if joint >= joint_count
                || !(bits == RAW_BITS || (1..=16).contains(&bits))
                || !layout_ok
            {
                return Err(FormatError("bad animated track"));
            }
            animated.push(AnimatedTrack {
                min,
                scale,
                joint,
                channel,
                bits,
                layout,
                offset,
            });
        }
        let frame_bits = r.u32()?;
        let len = r.u32()? as usize;
        let data = r.take(len)?.to_vec();
        r.finish()?;
        if !animated.is_sorted_by_key(|t| (t.joint, t.channel)) {
            return Err(FormatError("animated tracks out of order"));
        }
        if animated
            .iter()
            .any(|t| u64::from(t.offset) + u64::from(t.sample_bits()) > u64::from(frame_bits))
        {
            return Err(FormatError("track outside its frame"));
        }
        let needed = (u64::from(sample_count) * u64::from(frame_bits)).div_ceil(8) + 8;
        if (data.len() as u64) < needed {
            return Err(FormatError("frames truncated"));
        }
        Ok(Self {
            name,
            duration,
            sample_count,
            joint_count,
            additive,
            constants,
            animated,
            frame_bits,
            data,
        })
    }
}

const CLIP_MAGIC: u32 = u32::from_le_bytes(*b"CLP1");
