//! The clip compressor: uniform resampling, constant and default track removal, range
//! reduction and a bit width per track chosen against an object-space error bound
//! (see [`crate::clip`] for the format).
//!
//! **Error metric** (after ACL's `qvvf_transform_error_metric`): every joint carries
//! virtual vertices, its origin and three points at its *shell distance* along its axes
//! (roughly where the skin around it lies). The error of a joint at a time is the largest
//! distance between its virtual vertices placed by the source animation and by the
//! compressed one, both through the whole hierarchy in object space, so a parent's
//! rotation error is measured where it shows: at the tips of its children. It is measured
//! at every sample and halfway between samples (where sampling interpolates).
//!
//! **Bit allocation:** joints in hierarchy order. A joint's animated tracks start at the
//! lowest width; while its error exceeds the threshold, the one-step increase (of one of
//! its tracks, or, when its own tracks cannot help any more, of an ancestor's) that
//! removes the most error per added bit is taken.

use crate::clip::{
    AnimatedTrack, Channel, CompressedClip, ConstantTrack, RAW_BITS, dequantize, quantize,
    rotation_from_xyz,
};
use crate::pose::{Pose, Transform, nlerp, safe_scale};
use crate::raw::RawClip;
use crate::skeleton::Skeleton;
use engine_core::math::{Affine3A, Quat, Vec3, Vec3A, Vec4};

/// What an additive clip's deltas are taken against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdditiveBase {
    /// The clip's own first sample (a pose layered on top of anything: a lean, a breath).
    FirstSample,
    /// The skeleton's rest pose.
    RestPose,
}

/// How to compress.
#[derive(Debug, Clone, PartialEq)]
pub struct CompressSettings {
    /// Largest allowed error of any virtual vertex, in object-space units (0.1 mm for
    /// meters: ACL's default of 0.01 cm).
    pub error_threshold: f32,
    /// Smallest shell distance (virtual vertices' distance from their joint), in
    /// object-space units. A joint's own distance is the larger of this and its longest
    /// bone to a child, unless [`Self::joint_shell_distances`] gives it.
    pub shell_distance: f32,
    /// Per-joint shell distances (for example the farthest skinned vertex of each joint,
    /// measured by the cooker); empty to derive them from the skeleton.
    pub joint_shell_distances: Vec<f32>,
    /// Samples per second, or `None` for the source's own key rate (its smallest key
    /// interval), doubled while interpolating between samples misses more than half the
    /// error threshold.
    pub sample_rate: Option<f32>,
    /// Highest sample rate chosen automatically.
    pub max_sample_rate: f32,
    /// Make an additive clip (deltas over a base).
    pub additive: Option<AdditiveBase>,
}

impl Default for CompressSettings {
    fn default() -> Self {
        Self {
            error_threshold: 1e-4,
            shell_distance: 0.03,
            joint_shell_distances: Vec::new(),
            sample_rate: None,
            max_sample_rate: 120.0,
            additive: None,
        }
    }
}

/// What [`compress`] did.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CompressionStats {
    /// Bytes of the source keys (times and values, as glTF stores them).
    pub source_bytes: usize,
    /// Bytes of every joint's full transform as `f32` at every sample (the uncompressed
    /// uniform clip: 40 bytes per joint and sample).
    pub uniform_bytes: usize,
    /// Bytes of the compressed clip in memory ([`CompressedClip::size_bytes`]).
    pub compressed_bytes: usize,
    /// Largest error measured through the runtime decoder, object-space units.
    pub max_error: f32,
    /// The joint with the largest error.
    pub worst_joint: usize,
    /// Samples per second.
    pub sample_rate: f32,
    /// Samples.
    pub sample_count: u32,
    /// Tracks equal to the rest pose (identity when additive): not stored.
    pub default_tracks: usize,
    /// Tracks with one value.
    pub constant_tracks: usize,
    /// Tracks with a value per sample.
    pub animated_tracks: usize,
    /// Mean bits per component of the animated tracks.
    pub mean_bits: f32,
}

/// Bit widths tried, smallest first.
const BIT_RATES: [u8; 15] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, RAW_BITS];

/// How a joint's component is stored while compressing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Default,
    Constant,
    /// Index into `BIT_RATES`.
    Animated(usize),
}

/// A component's stored values over the samples (rotations as x, y, z, w with w ≥ 0;
/// deltas for additive clips).
struct TrackValues {
    values: Vec<Vec4>,
    min: Vec3A,
    max: Vec3A,
}

struct Compressor<'a> {
    skeleton: &'a Skeleton,
    threshold: f32,
    samples: usize,
    checkpoints: usize,
    /// Virtual vertex distance per joint.
    shell: Vec<f32>,
    /// Base pose of additive clips (stored values are deltas over it).
    base: Option<Vec<Transform>>,
    /// Stored values per joint and channel.
    tracks: Vec<[TrackValues; 3]>,
    kinds: Vec<[Kind; 3]>,
    /// Source object transforms per checkpoint, then joint.
    object_source: Vec<Affine3A>,
    /// Decoded local transforms per joint, then checkpoint (current bit choice).
    decoded: Vec<Vec<Affine3A>>,
    /// Decoded object transforms per joint, then checkpoint (joints done so far).
    object: Vec<Vec<Affine3A>>,
}

fn default_value(channel: Channel, rest: Transform, additive: bool) -> Vec4 {
    match (channel, additive) {
        (Channel::Rotation, true) => Vec4::from(Quat::IDENTITY),
        (Channel::Translation, true) => Vec4::ZERO,
        (Channel::Scale, true) => Vec4::new(1.0, 1.0, 1.0, 0.0),
        (Channel::Rotation, false) => Vec4::from(positive_w(rest.rotation)),
        (Channel::Translation, false) => Vec3::from(rest.translation).extend(0.0),
        (Channel::Scale, false) => Vec3::from(rest.scale).extend(0.0),
    }
}

fn positive_w(q: Quat) -> Quat {
    if q.w < 0.0 { -q } else { q }
}

/// Distance that a difference between two stored values moves a virtual vertex at
/// `shell` (an upper bound for rotations and scales, exact for translations).
fn value_distance(channel: Channel, a: Vec4, b: Vec4, shell: f32) -> f32 {
    match channel {
        Channel::Rotation => {
            let d = a.dot(b).abs().min(1.0);
            2.0 * d.acos() * shell
        }
        Channel::Translation => (a - b).truncate().length(),
        Channel::Scale => (a - b).truncate().abs().max_element() * shell,
    }
}

impl Compressor<'_> {
    fn local_transform(&self, j: usize, stored: [Vec4; 3]) -> Transform {
        let r = Quat::from_vec4(stored[0]).normalize();
        let t = Vec3A::from(stored[1].truncate());
        let s = Vec3A::from(stored[2].truncate());
        match &self.base {
            Some(base) => {
                let b = base[j];
                Transform {
                    translation: b.translation + t,
                    rotation: (b.rotation * r).normalize(),
                    scale: b.scale * s,
                }
            }
            None => Transform {
                translation: t,
                rotation: r,
                scale: s,
            },
        }
    }

    /// The decoded stored value of one component at sample `s` with `kind`.
    fn decode_value(&self, j: usize, channel: Channel, kind: Kind, s: usize) -> Vec4 {
        let track = &self.tracks[j][channel as usize];
        match kind {
            Kind::Default | Kind::Constant => track.values[0],
            Kind::Animated(rate) => {
                let bits = BIT_RATES[rate];
                let (min, scale) = range(track, bits);
                let v = track.values[s].truncate();
                let q = Vec3::new(
                    dequantize(quantize(v.x, min.x, scale.x, bits), min.x, scale.x, bits),
                    dequantize(quantize(v.y, min.y, scale.y, bits), min.y, scale.y, bits),
                    dequantize(quantize(v.z, min.z, scale.z, bits), min.z, scale.z, bits),
                );
                match channel {
                    Channel::Rotation => Vec4::from(rotation_from_xyz(q.into())),
                    _ => q.extend(0.0),
                }
            }
        }
    }

    /// Joint `j`'s decoded local transforms at every checkpoint with `kinds`.
    fn decode_joint(&self, j: usize, kinds: [Kind; 3]) -> Vec<Affine3A> {
        let stored: Vec<[Vec4; 3]> = (0..self.samples)
            .map(|s| Channel::ALL.map(|c| self.decode_value(j, c, kinds[c as usize], s)))
            .collect();
        (0..self.checkpoints)
            .map(|c| {
                let (s, odd) = (c / 2, c % 2 == 1);
                let value = if odd && s + 1 < self.samples {
                    let (a, b) = (stored[s], stored[s + 1]);
                    [
                        Vec4::from(nlerp(Quat::from_vec4(a[0]), Quat::from_vec4(b[0]), 0.5)),
                        a[1].lerp(b[1], 0.5),
                        a[2].lerp(b[2], 0.5),
                    ]
                } else {
                    stored[s.min(self.samples - 1)]
                };
                self.local_transform(j, value).to_affine()
            })
            .collect()
    }

    fn virtual_vertices(&self, j: usize) -> [Vec3A; 4] {
        let d = self.shell[j];
        [Vec3A::ZERO, Vec3A::X * d, Vec3A::Y * d, Vec3A::Z * d]
    }

    /// Largest error of joint `j` with ancestor-or-self `a`'s local transforms replaced by
    /// `replaced` (the joints between use their current decoding).
    fn error(&self, j: usize, a: usize, replaced: &[Affine3A]) -> f32 {
        let mut path = Vec::new();
        let mut k = j;
        while k != a {
            path.push(k);
            match self.skeleton.parent(k) {
                Some(p) => k = p,
                None => break,
            }
        }
        path.reverse();
        let start = self.skeleton.parent(a);
        let vv = self.virtual_vertices(j);
        let joints = self.skeleton.joint_count();
        let mut worst = 0.0f32;
        for c in 0..self.checkpoints {
            let mut m = start.map_or(Affine3A::IDENTITY, |p| self.object[p][c]) * replaced[c];
            for &k in &path {
                m *= self.decoded[k][c];
            }
            let source = self.object_source[c * joints + j];
            for &v in &vv {
                let e = (source.transform_point3a(v) - m.transform_point3a(v)).length();
                worst = worst.max(e);
            }
        }
        worst
    }

    /// Recomputes the decoded object transforms of joints `from..to` (done joints).
    fn update_objects(&mut self, from: usize, to: usize) {
        for k in from..to {
            let parent = self.skeleton.parent(k);
            for c in 0..self.checkpoints {
                let p = parent.map_or(Affine3A::IDENTITY, |p| self.object[p][c]);
                self.object[k][c] = p * self.decoded[k][c];
            }
        }
    }

    /// Chooses joint `j`'s bit widths (and raises its ancestors' when needed).
    fn optimize(&mut self, j: usize) {
        loop {
            let error = self.error(j, j, &self.decoded[j]);
            if error <= self.threshold {
                break;
            }
            // Candidates: one step up of one of j's own tracks; if none of them helps,
            // of an ancestor's.
            let mut best: Option<(f32, f32, usize, usize, Vec<Affine3A>)> = None;
            let mut ancestors = Vec::new();
            let mut k = Some(j);
            while let Some(a) = k {
                ancestors.push(a);
                k = self.skeleton.parent(a);
            }
            for (round, joints) in [&ancestors[..1], &ancestors[1..]].into_iter().enumerate() {
                for &a in joints {
                    for channel in 0..3 {
                        let Kind::Animated(rate) = self.kinds[a][channel] else {
                            continue;
                        };
                        if rate + 1 >= BIT_RATES.len() {
                            continue;
                        }
                        let mut kinds = self.kinds[a];
                        kinds[channel] = Kind::Animated(rate + 1);
                        let decoded = self.decode_joint(a, kinds);
                        let e = self.error(j, a, &decoded);
                        let bits = f32::from(BIT_RATES[rate + 1] - BIT_RATES[rate]);
                        let score = (error - e) / bits;
                        if best
                            .as_ref()
                            .is_none_or(|b| score > b.0 || (score == b.0 && e < b.1))
                        {
                            best = Some((score, e, a, channel, decoded));
                        }
                    }
                }
                if round == 0 && best.as_ref().is_some_and(|b| b.0 > 0.0) {
                    break;
                }
            }
            let Some((_, _, a, channel, decoded)) = best else {
                // Every track on the way is raw: the source cannot be met (its own
                // interpolation error); the stats report what remains.
                break;
            };
            if let Kind::Animated(rate) = self.kinds[a][channel] {
                self.kinds[a][channel] = Kind::Animated(rate + 1);
            }
            self.decoded[a] = decoded;
            if a != j {
                self.update_objects(a, j);
            }
        }
        self.update_objects(j, j + 1);
    }
}

/// Range minimum and step of a track's components for `bits`.
fn range(track: &TrackValues, bits: u8) -> (Vec3A, Vec3A) {
    if bits == RAW_BITS {
        return (Vec3A::ZERO, Vec3A::ZERO);
    }
    let steps = ((1u32 << bits) - 1) as f32;
    (track.min, (track.max - track.min) / steps)
}

/// Compresses `raw`, an animation of `skeleton` (see the module documentation).
pub fn compress(
    raw: &RawClip,
    skeleton: &Skeleton,
    settings: &CompressSettings,
) -> (CompressedClip, CompressionStats) {
    engine_profiler::zone!("anim::compress");
    let joints = skeleton.joint_count();
    let rest = skeleton.rest();
    let duration = if raw.duration.is_finite() {
        raw.duration.max(0.0)
    } else {
        0.0
    };
    let threshold = settings.error_threshold.max(1e-9);

    // Shell distances: the given ones, or the longest bone to a child (at least the
    // minimum).
    let mut longest = vec![0.0f32; joints];
    for j in 0..joints {
        if let Some(p) = skeleton.parent(j) {
            longest[p] = longest[p].max(rest.translations[j].length());
        }
    }
    let shell: Vec<f32> = (0..joints)
        .map(|j| {
            settings
                .joint_shell_distances
                .get(j)
                .copied()
                .unwrap_or(longest[j])
                .max(settings.shell_distance)
        })
        .collect();

    // Sample rate: the source's key rate, doubled while interpolation alone misses.
    let mut rate = settings
        .sample_rate
        .or_else(|| raw.min_key_interval().map(|d| 1.0 / d))
        .unwrap_or(30.0)
        .clamp(1.0, settings.max_sample_rate.max(1.0));
    let mut compressor;
    loop {
        let samples = if duration > 0.0 {
            (duration * rate).round().max(1.0) as usize + 1
        } else {
            1
        };
        compressor = build(
            raw, skeleton, settings, &shell, samples, duration, threshold,
        );
        if settings.sample_rate.is_some() || samples < 2 || rate * 2.0 > settings.max_sample_rate {
            break;
        }
        // Error of the samples themselves (every track raw): interpolation only.
        let raw_kinds: Vec<[Kind; 3]> = compressor
            .kinds
            .iter()
            .map(|k| {
                k.map(|k| {
                    if matches!(k, Kind::Animated(_)) {
                        Kind::Animated(BIT_RATES.len() - 1)
                    } else {
                        k
                    }
                })
            })
            .collect();
        let mut worst = 0.0f32;
        for j in 0..joints {
            compressor.decoded[j] = compressor.decode_joint(j, raw_kinds[j]);
            compressor.update_objects(j, j + 1);
            let e = compressor.error(j, j, &compressor.decoded[j]);
            worst = worst.max(e);
        }
        if worst <= threshold * 0.5 {
            break;
        }
        rate *= 2.0;
    }

    for j in 0..joints {
        compressor.decoded[j] = compressor.decode_joint(j, compressor.kinds[j]);
    }
    for j in 0..joints {
        compressor.optimize(j);
    }

    let clip = encode(
        raw,
        &compressor,
        joints,
        duration,
        settings.additive.is_some(),
    );
    let mut stats = CompressionStats {
        source_bytes: raw.source_bytes(),
        uniform_bytes: compressor.samples * joints * 40,
        compressed_bytes: clip.size_bytes(),
        sample_rate: clip.sample_rate(),
        sample_count: clip.sample_count,
        ..CompressionStats::default()
    };
    let mut bits = 0usize;
    for kinds in &compressor.kinds {
        for k in kinds {
            match k {
                Kind::Default => stats.default_tracks += 1,
                Kind::Constant => stats.constant_tracks += 1,
                Kind::Animated(r) => {
                    stats.animated_tracks += 1;
                    bits += usize::from(BIT_RATES[*r]);
                }
            }
        }
    }
    stats.mean_bits = bits as f32 / stats.animated_tracks.max(1) as f32;
    let (error, worst) = measure(&clip, skeleton, &compressor);
    stats.max_error = error;
    stats.worst_joint = worst;
    (clip, stats)
}

/// Samples, stored values, track kinds and the source's object transforms.
fn build<'a>(
    raw: &RawClip,
    skeleton: &'a Skeleton,
    settings: &CompressSettings,
    shell: &[f32],
    samples: usize,
    duration: f32,
    threshold: f32,
) -> Compressor<'a> {
    let joints = skeleton.joint_count();
    let rest = skeleton.rest();
    let checkpoints = 2 * samples - 1;
    let time = |c: usize| {
        if samples > 1 {
            duration * c as f32 / (checkpoints - 1) as f32
        } else {
            0.0
        }
    };
    // Source local transforms per checkpoint, then joint.
    let source: Vec<Transform> = (0..checkpoints)
        .flat_map(|c| (0..joints).map(move |j| (c, j)))
        .map(|(c, j)| raw.sample_joint(j, time(c), rest.get(j)))
        .collect();
    let mut object_source = vec![Affine3A::IDENTITY; checkpoints * joints];
    for c in 0..checkpoints {
        for j in 0..joints {
            let local = source[c * joints + j].to_affine();
            object_source[c * joints + j] = match skeleton.parent(j) {
                Some(p) => object_source[c * joints + p] * local,
                None => local,
            };
        }
    }
    let base: Option<Vec<Transform>> = settings.additive.map(|b| match b {
        AdditiveBase::FirstSample => source[..joints].to_vec(),
        AdditiveBase::RestPose => (0..joints).map(|j| rest.get(j)).collect(),
    });
    let additive = base.is_some();
    let mut tracks = Vec::with_capacity(joints);
    let mut kinds = Vec::with_capacity(joints);
    for j in 0..joints {
        let values = |channel: Channel| -> Vec<Vec4> {
            (0..samples)
                .map(|s| {
                    let t = source[2 * s * joints + j];
                    let b = base.as_ref().map(|b| b[j]);
                    match channel {
                        Channel::Rotation => {
                            let r = b.map_or(t.rotation, |b| b.rotation.conjugate() * t.rotation);
                            Vec4::from(positive_w(r.normalize()))
                        }
                        Channel::Translation => {
                            Vec3::from(t.translation - b.map_or(Vec3A::ZERO, |b| b.translation))
                                .extend(0.0)
                        }
                        Channel::Scale => {
                            let s = b.map_or(t.scale, |b| t.scale / safe_scale(b.scale));
                            Vec3::from(s).extend(0.0)
                        }
                    }
                })
                .collect()
        };
        let mut joint_tracks = Channel::ALL.map(|c| {
            let values = values(c);
            let (mut min, mut max) = (Vec3A::splat(f32::MAX), Vec3A::splat(f32::MIN));
            for v in &values {
                min = min.min(v.truncate().into());
                max = max.max(v.truncate().into());
            }
            TrackValues { values, min, max }
        });
        let joint_kinds = Channel::ALL.map(|c| {
            let track = &joint_tracks[c as usize];
            let tolerance = threshold * 0.05;
            let default = default_value(c, rest.get(j), additive);
            let near = |reference: Vec4| {
                track
                    .values
                    .iter()
                    .all(|&v| value_distance(c, v, reference, shell[j]) <= tolerance)
            };
            if near(default) {
                Kind::Default
            } else if near(track.values[0]) {
                Kind::Constant
            } else {
                Kind::Animated(0)
            }
        });
        // Default tracks decode to the default value itself.
        for c in Channel::ALL {
            if joint_kinds[c as usize] == Kind::Default {
                let default = default_value(c, rest.get(j), additive);
                joint_tracks[c as usize].values[0] = default;
            }
        }
        tracks.push(joint_tracks);
        kinds.push(joint_kinds);
    }
    Compressor {
        skeleton,
        threshold,
        samples,
        checkpoints,
        shell: shell.to_vec(),
        base,
        tracks,
        kinds,
        object_source,
        decoded: vec![Vec::new(); joints],
        object: vec![vec![Affine3A::IDENTITY; checkpoints]; joints],
    }
}

/// Writes the clip: constants, animated tracks with their offsets, the frames.
fn encode(
    raw: &RawClip,
    c: &Compressor<'_>,
    joints: usize,
    duration: f32,
    additive: bool,
) -> CompressedClip {
    let mut constants = Vec::new();
    let mut animated = Vec::new();
    let mut offset = 0u32;
    for j in 0..joints {
        for channel in Channel::ALL {
            let track = &c.tracks[j][channel as usize];
            match c.kinds[j][channel as usize] {
                Kind::Default => {}
                Kind::Constant => constants.push(ConstantTrack {
                    joint: j as u16,
                    channel,
                    value: track.values[0],
                }),
                Kind::Animated(rate) => {
                    let bits = BIT_RATES[rate];
                    let (min, scale) = range(track, bits);
                    animated.push(AnimatedTrack {
                        min,
                        scale,
                        joint: j as u16,
                        channel,
                        bits,
                        offset,
                    });
                    offset += 3 * u32::from(bits);
                }
            }
        }
    }
    let frame_bits = offset;
    let total_bits = c.samples as u64 * u64::from(frame_bits);
    let mut data = vec![0u8; total_bits.div_ceil(8) as usize + 8];
    let mut put = |at: u64, value: u32, bits: u32| {
        for b in 0..bits {
            if value >> b & 1 != 0 {
                let bit = at + u64::from(b);
                data[(bit >> 3) as usize] |= 1 << (bit & 7);
            }
        }
    };
    for s in 0..c.samples {
        let frame = s as u64 * u64::from(frame_bits);
        for t in &animated {
            let v = c.tracks[t.joint as usize][t.channel as usize].values[s].truncate();
            let at = frame + u64::from(t.offset);
            let bits = u32::from(t.bits);
            for (k, (x, (min, scale))) in [
                (v.x, (t.min.x, t.scale.x)),
                (v.y, (t.min.y, t.scale.y)),
                (v.z, (t.min.z, t.scale.z)),
            ]
            .into_iter()
            .enumerate()
            {
                put(
                    at + k as u64 * u64::from(bits),
                    quantize(x, min, scale, t.bits),
                    bits,
                );
            }
        }
    }
    CompressedClip {
        name: raw.name.clone(),
        duration,
        sample_count: c.samples as u32,
        joint_count: joints as u16,
        additive,
        constants,
        animated,
        frame_bits,
        data,
    }
}

/// The largest error of `clip` through the runtime decoder at every checkpoint, and the
/// joint where it is.
fn measure(clip: &CompressedClip, skeleton: &Skeleton, c: &Compressor<'_>) -> (f32, usize) {
    let joints = skeleton.joint_count();
    let mut pose = Pose::identity(joints);
    let mut object = vec![Affine3A::IDENTITY; joints];
    let (mut worst, mut worst_joint) = (0.0f32, 0);
    for k in 0..c.checkpoints {
        let time = if c.samples > 1 {
            clip.duration * k as f32 / (c.checkpoints - 1) as f32
        } else {
            0.0
        };
        clip.sample(time, skeleton.rest(), &mut pose);
        for j in 0..joints {
            let stored = pose.get(j);
            let local = match &c.base {
                Some(base) => Transform {
                    translation: base[j].translation + stored.translation,
                    rotation: (base[j].rotation * stored.rotation).normalize(),
                    scale: base[j].scale * stored.scale,
                },
                None => stored,
            }
            .to_affine();
            object[j] = skeleton.parent(j).map_or(local, |p| object[p] * local);
            let source = c.object_source[k * joints + j];
            for v in c.virtual_vertices(j) {
                let e = (source.transform_point3a(v) - object[j].transform_point3a(v)).length();
                if e > worst {
                    worst = e;
                    worst_joint = j;
                }
            }
        }
    }
    (worst, worst_joint)
}
