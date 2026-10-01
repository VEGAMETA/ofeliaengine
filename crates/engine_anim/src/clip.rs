//! Compressed clips (the runtime format) and their sampling.
//!
//! A clip is sampled uniformly in time. Each joint's rotation, translation and scale is
//! one of:
//!
//! - **default**: equal to the rest pose (or to identity in an additive clip) and not
//!   stored;
//! - **constant**: one value, stored at full precision;
//! - **animated**: one value per sample, range-reduced and quantized. A rotation drops one
//!   component of its quaternion, the one whose magnitude stays largest over the clip
//!   (made positive and recomputed from the other three, so it is never close to 0 where
//!   recomputing it loses precision); a rotation without such a component (none stays
//!   above 0.25) or stored raw keeps all four.
//!
//! The samples are cut into **segments** of [`SEGMENT_SAMPLES`] intervals (each segment
//! also holds the first sample of the next, so the two samples around any time are in one
//! segment). Per segment, every animated track has its own range, 8-bit fractions of the
//! track's range over the clip, and every component its own bit width (0, for a component
//! that stays at the range's minimum, 3–16, or 32 for raw floats): a track that moves
//! little in a segment, or about one axis only, costs few bits there. Within a
//! segment the samples are stored frame by frame (every animated track of one sample,
//! then of the next), so sampling a time reads two contiguous bit strings. The bit widths
//! are chosen by the compressor ([`crate::compress`]) to keep the error of every joint,
//! measured in object space on virtual vertices around it, under a threshold: the
//! approach (segments, range reduction, variable bit rates, the error metric) of
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

/// A track with one quantized value per sample: what is the same in every segment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AnimatedTrack {
    /// Range minimum per stored component over the clip.
    pub(crate) min: Vec4,
    /// Range extent per stored component over the clip.
    pub(crate) extent: Vec4,
    pub(crate) joint: u16,
    pub(crate) channel: Channel,
    /// Rotations: the quaternion component not stored (0-3: x, y, z, w; recomputed,
    /// positive), or [`ALL_FOUR`]; translations and scales: 3 (x, y, z).
    pub(crate) layout: u8,
}

/// An animated track in one segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentTrack {
    /// Bits per component: 0 (the range's minimum), 1-16, or 32 (raw `f32`, the range
    /// unused).
    pub(crate) bits: [u8; 4],
    /// The segment's range per component, in 255ths of the clip's range.
    pub(crate) lo: [u8; 4],
    pub(crate) hi: [u8; 4],
}

/// A run of samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Segment {
    /// Its first sample.
    pub(crate) first: u32,
    /// Samples it stores (the first of the next segment included).
    pub(crate) count: u32,
    /// Bits per frame.
    pub(crate) frame_bits: u32,
    /// Byte offset of its first frame in the data.
    pub(crate) data: u32,
}

/// Sample intervals per segment (ACL's default segment length).
pub const SEGMENT_SAMPLES: u32 = 16;

/// [`AnimatedTrack::layout`] of a rotation that stores all four components.
pub(crate) const ALL_FOUR: u8 = 4;

/// Bits per component of a raw track.
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
    pub(crate) segments: Vec<Segment>,
    /// Per segment, one per animated track.
    pub(crate) segment_tracks: Vec<SegmentTrack>,
    /// The segments' frames (each segment starts on a byte), then 8 bytes of padding
    /// (64-bit reads at any bit).
    pub(crate) data: Vec<u8>,
}

/// The segments of `sample_count` samples: `(first, count)`.
pub(crate) fn segment_layout(sample_count: u32) -> impl Iterator<Item = (u32, u32)> {
    let last = sample_count.saturating_sub(1);
    let segments = last.div_ceil(SEGMENT_SAMPLES).max(1);
    (0..segments).map(move |k| {
        let first = k * SEGMENT_SAMPLES;
        (first, (last - first).min(SEGMENT_SAMPLES) + 1)
    })
}

/// The 64 bits from the byte holding bit `at` (0 past the end).
#[inline(always)]
fn read_word(data: &[u8], at: usize) -> u64 {
    let byte = at >> 3;
    if byte + 8 <= data.len() {
        // SAFETY: the 8 bytes from `byte` are inside `data` (checked just above); an
        // unaligned read of plain bytes.
        u64::from_le(unsafe { data.as_ptr().add(byte).cast::<u64>().read_unaligned() })
    } else {
        0
    }
}

/// Reads `bits` (≤ 32) bits at bit `at` of `data`; 0 past the end.
#[inline(always)]
pub(crate) fn read_bits(data: &[u8], at: usize, bits: u32) -> u32 {
    ((read_word(data, at) >> (at & 7)) & ((1u64 << bits) - 1)) as u32
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

/// A segment's range in 255ths of the clip's (`min`, `extent`), containing `[lo, hi]`.
pub(crate) fn segment_fractions(min: Vec4, extent: Vec4, lo: Vec4, hi: Vec4) -> ([u8; 4], [u8; 4]) {
    let mut a = [0u8; 4];
    let mut b = [0u8; 4];
    for k in 0..4 {
        if extent[k] > 0.0 {
            a[k] = ((lo[k] - min[k]) / extent[k] * 255.0)
                .floor()
                .clamp(0.0, 255.0) as u8;
            b[k] = ((hi[k] - min[k]) / extent[k] * 255.0)
                .ceil()
                .clamp(0.0, 255.0) as u8;
            // At least one 255th wide: a component still in a segment keeps a range to
            // quantize its value in.
            if b[k] <= a[k] {
                if a[k] == 255 {
                    a[k] = 254;
                }
                b[k] = a[k] + 1;
            }
        }
    }
    (a, b)
}

/// A segment's range minimum and the step of each component's values over it.
#[inline(always)]
pub(crate) fn segment_range(track: &AnimatedTrack, st: &SegmentTrack) -> (Vec4, Vec4) {
    let f = |v: [u8; 4]| {
        Vec4::new(
            FRACTION[v[0] as usize],
            FRACTION[v[1] as usize],
            FRACTION[v[2] as usize],
            FRACTION[v[3] as usize],
        )
    };
    let (lo, hi) = (f(st.lo), f(st.hi));
    let steps = Vec4::new(
        STEP[(st.bits[0] & 31) as usize],
        STEP[(st.bits[1] & 31) as usize],
        STEP[(st.bits[2] & 31) as usize],
        STEP[(st.bits[3] & 31) as usize],
    );
    (
        track.min + track.extent * lo,
        track.extent * (hi - lo) * steps,
    )
}

/// `k / 255` (a segment range's fractions of the clip's).
static FRACTION: [f32; 256] = {
    let mut t = [0.0f32; 256];
    let mut k = 0;
    while k < 256 {
        t[k] = k as f32 / 255.0;
        k += 1;
    }
    t
};

/// The step of `b`-bit values over a unit range, `1 / (2^b − 1)`; 0 for 0 bits (and for
/// 32 = raw: the index is masked to 0).
static STEP: [f32; 32] = {
    let mut t = [0.0f32; 32];
    let mut b = 1;
    while b < 32 {
        t[b] = 1.0 / ((1u64 << b) - 1) as f32;
        b += 1;
    }
    t
};

/// Bits of one sample of a track: its components' widths.
#[inline(always)]
pub(crate) fn sample_bits(bits: [u8; 4], layout: u8) -> u32 {
    // Components past the track's are 0 bits wide.
    let _ = layout;
    bits.iter().map(|&b| u32::from(b)).sum()
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

/// The stored components of one track at bit `at` (the first component's), with the
/// segment's range minimum and steps (components past the track's have 0 bits: their
/// value is the minimum, 0).
#[inline(always)]
fn read_components(data: &[u8], at: usize, bits: [u8; 4], min: Vec4, scale: Vec4) -> Vec4 {
    let [b0, b1, b2, b3] = bits.map(u32::from);
    let (a0, a1, a2, a3) = (
        at,
        at + b0 as usize,
        at + (b0 + b1) as usize,
        at + (b0 + b1 + b2) as usize,
    );
    if (b0 | b1 | b2 | b3) & u32::from(RAW_BITS) == 0 {
        let shift = (a0 & 7) as u32;
        let q = if shift + b0 + b1 + b2 + b3 <= 64 {
            // The whole sample in one 64-bit read (up to 3 × 16 bits).
            let word = read_word(data, a0) >> shift;
            let field = |from: u32, bits: u32| {
                // `bits` ≤ 16 and `from + bits` ≤ 64 - shift: no shift overflows.
                ((word >> from.min(63)) & ((1u64 << bits) - 1)) as f32
            };
            Vec4::new(
                field(0, b0),
                field(b0, b1),
                field(b0 + b1, b2),
                field(b0 + b1 + b2, b3),
            )
        } else {
            Vec4::new(
                read_bits(data, a0, b0) as f32,
                read_bits(data, a1, b1) as f32,
                read_bits(data, a2, b2) as f32,
                read_bits(data, a3, b3) as f32,
            )
        };
        return min + scale * q;
    }
    // Raw components (rare): their bits as they are.
    let c = |a: usize, b: u32, k: usize| {
        if b == u32::from(RAW_BITS) {
            f32::from_bits(read_bits(data, a, 32))
        } else {
            min[k] + scale[k] * read_bits(data, a, b) as f32
        }
    };
    Vec4::new(c(a0, b0, 0), c(a1, b1, 1), c(a2, b2, 2), c(a3, b3, 3))
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

    /// Bytes of the cooked clip ([`Self::to_bytes`]): what it costs on disk and to load.
    pub fn stored_bytes(&self) -> usize {
        self.to_bytes().len()
    }

    /// Bytes in memory: the frames and the tables.
    pub fn size_bytes(&self) -> usize {
        self.data.len()
            + self.animated.len() * size_of::<AnimatedTrack>()
            + self.constants.len() * size_of::<ConstantTrack>()
            + self.segments.len() * size_of::<Segment>()
            + self.segment_tracks.len() * size_of::<SegmentTrack>()
            + size_of::<Self>()
    }

    /// (constant, animated) track counts; the rest of the `3 × joints` are default.
    pub fn track_counts(&self) -> (usize, usize) {
        (self.constants.len(), self.animated.len())
    }

    /// Segments of [`SEGMENT_SAMPLES`] intervals.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// The segment holding the samples around `time`, the bit offsets of those two
    /// samples' frames, and the weight of the second.
    #[inline]
    fn frames(&self, time: f32) -> (usize, usize, usize, f32) {
        let last = self.sample_count.saturating_sub(1);
        if last == 0 || self.duration <= 0.0 {
            let base = self.segments.first().map_or(0, |s| s.data as usize * 8);
            return (0, base, base, 0.0);
        }
        let f = (time / self.duration).clamp(0.0, 1.0) * last as f32;
        let f0 = (f as u32).min(last);
        let s = ((f0 / SEGMENT_SAMPLES) as usize).min(self.segments.len().saturating_sub(1));
        let Some(seg) = self.segments.get(s) else {
            return (0, 0, 0, 0.0);
        };
        let local0 = (f0 - seg.first).min(seg.count - 1);
        let local1 = (local0 + 1).min(seg.count - 1);
        let base = seg.data as usize * 8;
        let fb = seg.frame_bits as usize;
        (
            s,
            base + local0 as usize * fb,
            base + local1 as usize * fb,
            f - f0 as f32,
        )
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
        if self.animated.is_empty() {
            return;
        }
        let (s, a, b, t) = self.frames(time);
        let n = self.animated.len();
        let Some(tracks) = self.segment_tracks.get(s * n..s * n + n) else {
            return;
        };
        let data = &self.data[..];
        // Tracks follow each other in a frame: each starts where the one before ended.
        let mut next = 0usize;
        for (track, st) in self.animated.iter().zip(tracks) {
            let offset = next;
            next += sample_bits(st.bits, track.layout) as usize;
            let j = track.joint as usize;
            if j >= out.len() {
                continue;
            }
            let (min, scale) = segment_range(track, st);
            let va = read_components(data, a + offset, st.bits, min, scale);
            let vb = read_components(data, b + offset, st.bits, min, scale);
            match track.channel {
                Channel::Rotation => {
                    out.rotations[j] = nlerp(
                        rotation_from_stored(va, track.layout),
                        rotation_from_stored(vb, track.layout),
                        t,
                    );
                }
                Channel::Translation => {
                    out.translations[j] = va.truncate().lerp(vb.truncate(), t).into();
                }
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
        if self.animated.is_empty() {
            return out;
        }
        let (s, a, b, t) = self.frames(time);
        let first = self.animated.partition_point(|k| (k.joint as usize) < j);
        let tracks = &self.segment_tracks[s * self.animated.len()..][..self.animated.len()];
        let mut next: usize = self.animated[..first]
            .iter()
            .zip(tracks)
            .map(|(track, st)| sample_bits(st.bits, track.layout) as usize)
            .sum();
        for (track, st) in self.animated[first..]
            .iter()
            .zip(&tracks[first..])
            .take_while(|(k, _)| k.joint as usize == j)
        {
            let offset = next;
            next += sample_bits(st.bits, track.layout) as usize;
            let (min, scale) = segment_range(track, st);
            let va = read_components(&self.data, a + offset, st.bits, min, scale);
            let vb = read_components(&self.data, b + offset, st.bits, min, scale);
            match track.channel {
                Channel::Rotation => {
                    out.rotation = nlerp(
                        rotation_from_stored(va, track.layout),
                        rotation_from_stored(vb, track.layout),
                        t,
                    );
                }
                Channel::Translation => {
                    out.translation = va.truncate().lerp(vb.truncate(), t).into();
                }
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
            w.u8(t.layout);
            w.f32s(&t.min.to_array());
            w.f32s(&t.extent.to_array());
        }
        w.u32(self.segments.len() as u32);
        for s in &self.segments {
            w.u32(s.frame_bits);
            w.u32(s.data);
        }
        for t in &self.segment_tracks {
            w.raw(&t.bits);
            w.raw(&t.lo);
            w.raw(&t.hi);
        }
        w.u32(self.data.len() as u32);
        w.raw(&self.data);
        w.bytes
    }

    /// A clip from [`Self::to_bytes`], validated: every track inside its frames, the
    /// segments as the sample count lays them out, the data as long as they say.
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
            let layout = r.u8()?;
            let min = Vec4::from_array(r.f32s::<4>()?);
            let extent = Vec4::from_array(r.f32s::<4>()?);
            let layout_ok = match channel {
                Channel::Rotation => layout <= ALL_FOUR,
                Channel::Translation | Channel::Scale => layout == 3,
            };
            if joint >= joint_count || !layout_ok || extent.min_element() < 0.0 {
                return Err(FormatError("bad animated track"));
            }
            animated.push(AnimatedTrack {
                min,
                extent,
                joint,
                channel,
                layout,
            });
        }
        if !animated.is_sorted_by_key(|t| (t.joint, t.channel)) {
            return Err(FormatError("animated tracks out of order"));
        }
        let expected: Vec<(u32, u32)> = segment_layout(sample_count).collect();
        if r.u32()? as usize != expected.len() {
            return Err(FormatError("bad segment count"));
        }
        let mut segments = Vec::with_capacity(expected.len());
        for &(first, count) in &expected {
            let frame_bits = r.u32()?;
            let data = r.u32()?;
            segments.push(Segment {
                first,
                count,
                frame_bits,
                data,
            });
        }
        let mut segment_tracks = Vec::with_capacity(segments.len() * animated.len());
        for s in &segments {
            let mut offset = 0u64;
            for t in &animated {
                let mut bits = [0u8; 4];
                bits.copy_from_slice(r.take(4)?);
                let mut lo = [0u8; 4];
                let mut hi = [0u8; 4];
                lo.copy_from_slice(r.take(4)?);
                hi.copy_from_slice(r.take(4)?);
                if bits.iter().any(|&b| !(b == RAW_BITS || b <= 16))
                    || lo.iter().zip(&hi).any(|(l, h)| l > h)
                {
                    return Err(FormatError("bad segment track"));
                }
                segment_tracks.push(SegmentTrack { bits, lo, hi });
                offset += u64::from(sample_bits(bits, t.layout));
            }
            if offset != u64::from(s.frame_bits) {
                return Err(FormatError("tracks do not fill their frame"));
            }
        }
        let len = r.u32()? as usize;
        let data = r.take(len)?.to_vec();
        r.finish()?;
        let mut next = 0u64;
        for s in &segments {
            if u64::from(s.data) != next {
                return Err(FormatError("segments out of place"));
            }
            next += (u64::from(s.count) * u64::from(s.frame_bits)).div_ceil(8);
        }
        if (data.len() as u64) < next + 8 {
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
            segments,
            segment_tracks,
            data,
        })
    }
}

const CLIP_MAGIC: u32 = u32::from_le_bytes(*b"CLP2");
