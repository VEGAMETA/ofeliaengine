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
//! **Bit allocation**, per segment of the clip (each with its own range per track): joints
//! in hierarchy order. A joint's animated tracks start at the lowest width; while its
//! error exceeds the threshold, the one-step increase (of one of its tracks, or, when its
//! own tracks cannot help any more, of an ancestor's) that removes the most error per
//! added bit is taken. Segments are independent: the two samples around any time are in
//! one segment.

use crate::clip::{
    ALL_FOUR, AnimatedTrack, Channel, CompressedClip, ConstantTrack, RAW_BITS, Segment,
    SegmentTrack, component_count, dequantize, quantize, rotation_from_stored, sample_bits,
    segment_fractions, segment_layout, segment_range, stored_components,
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

/// Bit widths tried per component, smallest first (0: the segment range's minimum).
const BIT_RATES: [u8; 16] = [0, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, RAW_BITS];

/// How a joint's component is stored while compressing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Default,
    Constant,
    /// Index into `BIT_RATES` per stored component.
    Animated([u8; 4]),
}

/// A component's values over the samples (rotations whole, each in the hemisphere of the
/// one before; deltas for additive clips), and the range of the components stored.
struct TrackValues {
    values: Vec<Vec4>,
    /// Range of the stored components over the clip.
    min: Vec4,
    max: Vec4,
    /// [`AnimatedTrack::layout`] when quantized.
    layout: u8,
    /// The range in use (a segment's), in 255ths of the clip's.
    lo: [u8; 4],
    hi: [u8; 4],
}

impl TrackValues {
    /// The track as the clip stores it, with bit width `bits` in the current range.
    fn quantized(&self, channel: Channel, bits: [u8; 4]) -> (AnimatedTrack, SegmentTrack) {
        (
            AnimatedTrack {
                min: self.min,
                extent: self.max - self.min,
                joint: 0,
                channel,
                layout: self.layout,
            },
            SegmentTrack {
                bits,
                lo: self.lo,
                hi: self.hi,
            },
        )
    }
}

/// The components a track stores of `v` in `layout`.
fn stored(channel: Channel, v: Vec4, layout: u8) -> Vec4 {
    match channel {
        Channel::Rotation => stored_components(v, layout),
        Channel::Translation | Channel::Scale => v.truncate().extend(0.0),
    }
}

struct Compressor<'a> {
    skeleton: &'a Skeleton,
    threshold: f32,
    /// Per joint, the error to reach: the threshold, or a little above the error that
    /// remains with every track raw (the source's own curve between samples, which no
    /// bits fix).
    goal: Vec<f32>,
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
            Kind::Animated(rates) => {
                let bits = rates.map(|r| BIT_RATES[r as usize]);
                let v = track.values[s];
                let (t, st) = track.quantized(channel, bits);
                let (min, scale) = segment_range(&t, &st);
                let c = stored(channel, v, track.layout);
                let n = if channel == Channel::Rotation {
                    component_count(track.layout) as usize
                } else {
                    3
                };
                let mut q = Vec4::ZERO;
                for k in 0..n {
                    q[k] = dequantize(
                        quantize(c[k], min[k], scale[k], bits[k]),
                        min[k],
                        scale[k],
                        bits[k],
                    );
                }
                match channel {
                    Channel::Rotation => Vec4::from(rotation_from_stored(q, track.layout)),
                    _ => q,
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
        self.errors(j, a, replaced).0
    }

    /// The largest and the mean error of [`Self::error`]'s.
    fn errors(&self, j: usize, a: usize, replaced: &[Affine3A]) -> (f32, f32) {
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
        let (mut worst, mut sum) = (0.0f32, 0.0f32);
        for (c, local) in replaced.iter().enumerate().take(self.checkpoints) {
            let mut m = start.map_or(Affine3A::IDENTITY, |p| self.object[p][c]) * *local;
            for &k in &path {
                m *= self.decoded[k][c];
            }
            let source = self.object_source[c * joints + j];
            for &v in &vv {
                let e = (source.transform_point3a(v) - m.transform_point3a(v)).length();
                worst = worst.max(e);
                sum += e;
            }
        }
        (worst, sum / (4 * self.checkpoints.max(1)) as f32)
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
            let (error, mean) = self.errors(j, j, &self.decoded[j]);
            if error <= self.goal[j] {
                break;
            }
            // Candidates: one step up of one of j's own tracks; if none of them helps,
            // of an ancestor's.
            let mut best: Option<(f32, f32, usize, usize, Vec<Affine3A>)> = None;
            // The step that lowers the largest error most, for when no step scores
            // (lowering the largest error raised the mean).
            let mut closest: Option<(f32, usize, usize, Vec<Affine3A>)> = None;
            let mut ancestors = Vec::new();
            let mut k = Some(j);
            while let Some(a) = k {
                ancestors.push(a);
                k = self.skeleton.parent(a);
            }
            for (round, joints) in [&ancestors[..1], &ancestors[1..]].into_iter().enumerate() {
                for &a in joints {
                    for channel in 0..3 {
                        let Kind::Animated(rates) = self.kinds[a][channel] else {
                            continue;
                        };
                        let components = if channel == Channel::Rotation as usize {
                            component_count(self.tracks[a][channel].layout) as usize
                        } else {
                            3
                        };
                        for k in 0..components {
                            let rate = rates[k] as usize;
                            if rate + 1 >= BIT_RATES.len() {
                                continue;
                            }
                            let mut kinds = self.kinds[a];
                            let mut stepped = rates;
                            stepped[k] += 1;
                            kinds[channel] = Kind::Animated(stepped);
                            let decoded = self.decode_joint(a, kinds);
                            let (e, m) = self.errors(j, a, &decoded);
                            let bits = f32::from(BIT_RATES[rate + 1] - BIT_RATES[rate]);
                            // The largest error decides; the mean breaks ties where one
                            // step of one component cannot move the largest (another one
                            // sets it).
                            let score = ((error - e) + (mean - m)) / bits;
                            if e < error && closest.as_ref().is_none_or(|c| e < c.0) {
                                closest = Some((e, a, channel * 4 + k, decoded.clone()));
                            }
                            if best
                                .as_ref()
                                .is_none_or(|b| score > b.0 || (score == b.0 && e < b.1))
                            {
                                best = Some((score, e, a, channel * 4 + k, decoded));
                            }
                        }
                    }
                }
                if round == 0 && best.as_ref().is_some_and(|b| b.0 > 0.0) {
                    break;
                }
            }
            let step = match best.filter(|b| b.0 > 0.0) {
                Some((_, _, a, channel, decoded)) => Some((a, channel, decoded)),
                None => closest.map(|(_, a, channel, decoded)| (a, channel, decoded)),
            };
            let Some((a, channel, decoded)) = step else {
                // No single step lowers the largest error (it takes several components
                // at once): one step up for every component of the nearest joint on the
                // way that still has one. Every track on the way raw: what remains is the
                // source's own interpolation between samples; the stats report it.
                let raisable = |a: usize| {
                    (0..3).any(|c| {
                        let components = if c == Channel::Rotation as usize {
                            component_count(self.tracks[a][c].layout) as usize
                        } else {
                            3
                        };
                        matches!(self.kinds[a][c], Kind::Animated(r)
                            if r[..components].iter().any(|&r| (r as usize) + 1 < BIT_RATES.len()))
                    })
                };
                let Some(&a) = ancestors.iter().find(|&&a| raisable(a)) else {
                    break;
                };
                for c in 0..3 {
                    let components = if c == Channel::Rotation as usize {
                        component_count(self.tracks[a][c].layout) as usize
                    } else {
                        3
                    };
                    if let Kind::Animated(rates) = &mut self.kinds[a][c] {
                        for r in &mut rates[..components] {
                            *r = (*r + 1).min((BIT_RATES.len() - 1) as u8);
                        }
                    }
                }
                self.decoded[a] = self.decode_joint(a, self.kinds[a]);
                if a != j {
                    self.update_objects(a, j);
                }
                continue;
            };
            let (channel, k) = (channel / 4, channel % 4);
            if let Kind::Animated(mut rates) = self.kinds[a][channel] {
                rates[k] += 1;
                self.kinds[a][channel] = Kind::Animated(rates);
            }
            self.decoded[a] = decoded;
            if a != j {
                self.update_objects(a, j);
            }
        }
        self.update_objects(j, j + 1);
    }
}

impl<'a> Compressor<'a> {
    /// The samples `first .. first + count` alone, each animated track over its range in
    /// them (as 255ths of its range over the clip), every animated track at the lowest
    /// width.
    fn segment(&self, first: usize, count: usize) -> Compressor<'a> {
        let joints = self.skeleton.joint_count();
        let tracks: Vec<[TrackValues; 3]> = self
            .tracks
            .iter()
            .zip(&self.kinds)
            .map(|(tracks, kinds)| {
                core::array::from_fn(|c| {
                    let track = &tracks[c];
                    let channel = Channel::ALL[c];
                    if !matches!(kinds[c], Kind::Animated(_)) {
                        return TrackValues {
                            values: vec![track.values[0]],
                            min: track.min,
                            max: track.max,
                            layout: track.layout,
                            lo: [0; 4],
                            hi: [255; 4],
                        };
                    }
                    let values = track.values[first..first + count].to_vec();
                    let (mut lo, mut hi) = (Vec4::splat(f32::MAX), Vec4::splat(f32::MIN));
                    for v in &values {
                        let c = stored(channel, *v, track.layout);
                        lo = lo.min(c);
                        hi = hi.max(c);
                    }
                    let (lo, hi) = segment_fractions(track.min, track.max - track.min, lo, hi);
                    TrackValues {
                        values,
                        min: track.min,
                        max: track.max,
                        layout: track.layout,
                        lo,
                        hi,
                    }
                })
            })
            .collect();
        let checkpoints = 2 * count - 1;
        // Starting widths: per component, the fewest bits whose rounding alone moves the
        // joint's virtual vertices by at most the threshold (0 where the component stays
        // within that); the search raises them from there.
        let initial: Vec<[[u8; 4]; 3]> = (0..joints)
            .map(|j| {
                core::array::from_fn(|c| {
                    let track: &TrackValues = &tracks_ref(&tracks, j)[c];
                    let channel = Channel::ALL[c];
                    let shell = self.shell[j].max(1e-6);
                    let allowed = match channel {
                        Channel::Rotation => self.threshold / (2.0 * shell),
                        Channel::Translation => self.threshold / 3f32.sqrt(),
                        Channel::Scale => self.threshold / shell,
                    };
                    let (t, st) = track.quantized(channel, [0; 4]);
                    let unit = t.extent * (1.0 / 255.0);
                    core::array::from_fn(|k| {
                        let extent = unit[k] * (f32::from(st.hi[k]) - f32::from(st.lo[k]));
                        if extent <= allowed {
                            return 0;
                        }
                        let bits = (extent / (2.0 * allowed) + 1.0).log2().ceil() as u8;
                        BIT_RATES
                            .iter()
                            .position(|&b| b >= bits)
                            .unwrap_or(BIT_RATES.len() - 1) as u8
                    })
                })
            })
            .collect();
        Compressor {
            skeleton: self.skeleton,
            threshold: self.threshold,
            goal: self.goal.clone(),
            samples: count,
            checkpoints,
            shell: self.shell.clone(),
            base: self.base.clone(),
            tracks,
            kinds: initial
                .into_iter()
                .zip(&self.kinds)
                .map(|(rates, kinds)| {
                    core::array::from_fn(|c| match kinds[c] {
                        Kind::Animated(_) => Kind::Animated(rates[c]),
                        other => other,
                    })
                })
                .collect::<Vec<[Kind; 3]>>(),
            object_source: self.object_source
                [2 * first * joints..(2 * first + checkpoints) * joints]
                .to_vec(),
            decoded: vec![Vec::new(); joints],
            object: vec![vec![Affine3A::IDENTITY; checkpoints]; joints],
        }
    }

    /// Chooses every joint's bit widths (see the module documentation).
    fn optimize_all(&mut self) {
        let joints = self.skeleton.joint_count();
        // The floor: every animated component raw.
        let start = self.kinds.clone();
        for kinds in &mut self.kinds {
            for k in kinds.iter_mut() {
                if let Kind::Animated(_) = k {
                    *k = Kind::Animated([(BIT_RATES.len() - 1) as u8; 4]);
                }
            }
        }
        for j in 0..joints {
            self.decoded[j] = self.decode_joint(j, self.kinds[j]);
        }
        self.update_objects(0, joints);
        self.goal = (0..joints)
            .map(|j| {
                let floor = self.error(j, j, &self.decoded[j]);
                self.threshold.max(floor * 1.25)
            })
            .collect();
        self.kinds = start;
        for j in 0..joints {
            self.decoded[j] = self.decode_joint(j, self.kinds[j]);
        }
        for j in 0..joints {
            self.optimize(j);
        }
        // Raising an ancestor's bits for one joint can move the error of joints done
        // before it (other branches) a little either way: check every joint again until
        // all hold.
        for _ in 0..8 {
            self.update_objects(0, joints);
            let over: Vec<usize> = (0..joints)
                .filter(|&j| self.error(j, j, &self.decoded[j]) > self.goal[j])
                .collect();
            if over.is_empty() {
                break;
            }
            for j in over {
                self.update_objects(0, joints);
                self.optimize(j);
            }
        }
    }
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
    // A thousandth of margin: the runtime decoder places a time between samples with its
    // own rounding, which moves the result by float noise.
    let threshold = settings.error_threshold.max(1e-9) * 0.999;

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

    // Sample rate: the source's key rate, or the smallest multiple of it on whose grid
    // every key lies (keys off the grid get their corners cut), doubled while
    // interpolation alone misses.
    let max_rate = settings.max_sample_rate.max(1.0);
    let mut rate = settings
        .sample_rate
        .or_else(|| {
            let base = 1.0 / raw.min_key_interval()?;
            Some(aligned_rate(raw, base, max_rate).unwrap_or(base))
        })
        .unwrap_or(30.0)
        .clamp(1.0, max_rate);
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
                        Kind::Animated([(BIT_RATES.len() - 1) as u8; 4])
                    } else {
                        k
                    }
                })
            })
            .collect();
        let mut worst = 0.0f32;
        for (j, &kinds) in raw_kinds.iter().enumerate() {
            compressor.decoded[j] = compressor.decode_joint(j, kinds);
            compressor.update_objects(j, j + 1);
            let e = compressor.error(j, j, &compressor.decoded[j]);
            worst = worst.max(e);
        }
        if worst <= threshold * 0.5 {
            break;
        }
        rate *= 2.0;
    }

    let segments: Vec<Compressor<'_>> = segment_layout(compressor.samples as u32)
        .map(|(first, count)| {
            let mut segment = compressor.segment(first as usize, count as usize);
            segment.optimize_all();
            segment
        })
        .collect();

    let clip = encode(
        raw,
        &compressor,
        &segments,
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
    for kinds in &compressor.kinds {
        for k in kinds {
            match k {
                Kind::Default => stats.default_tracks += 1,
                Kind::Constant => stats.constant_tracks += 1,
                Kind::Animated(_) => stats.animated_tracks += 1,
            }
        }
    }
    let (mut bits, mut tracks) = (0usize, 0usize);
    for segment in &segments {
        for (j, kinds) in segment.kinds.iter().enumerate() {
            for (c, k) in kinds.iter().enumerate() {
                if let Kind::Animated(r) = k {
                    let n = if c == 0 {
                        component_count(segment.tracks[j][c].layout) as usize
                    } else {
                        3
                    };
                    bits += r[..n]
                        .iter()
                        .map(|&r| usize::from(BIT_RATES[r as usize]))
                        .sum::<usize>();
                    tracks += n;
                }
            }
        }
    }
    stats.mean_bits = bits as f32 / tracks.max(1) as f32;
    let (error, worst) = measure(&clip, skeleton, &compressor);
    stats.max_error = error;
    stats.worst_joint = worst;
    (clip, stats)
}

/// The smallest multiple of `base` (up to `max`) whose sample grid holds every key time
/// and the duration (within a hundredth of a sample).
fn aligned_rate(raw: &RawClip, base: f32, max: f32) -> Option<f32> {
    let mut times: Vec<f32> = raw
        .joints
        .iter()
        .flat_map(|j| {
            let t = j.translation.iter().flat_map(|t| t.times.iter());
            let r = j.rotation.iter().flat_map(|t| t.times.iter());
            let s = j.scale.iter().flat_map(|t| t.times.iter());
            t.chain(r).chain(s).copied()
        })
        .collect();
    times.push(raw.duration);
    times.sort_by(f32::total_cmp);
    times.dedup();
    (1..=16)
        .map(|m| base * m as f32)
        .take_while(|&r| r <= max * 1.0001)
        .find(|&r| {
            times.iter().all(|&t| {
                (f64::from(t) * f64::from(r))
                    .fract()
                    .min(1.0 - (f64::from(t) * f64::from(r)).fract())
                    < 0.01
            })
        })
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
                            Vec4::from(r.normalize())
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
            let mut values = values(c);
            let mut layout = 3;
            if c == Channel::Rotation {
                // Each in the hemisphere of the one before (no sign flips in the ranges),
                // then the component whose magnitude stays largest is dropped (made
                // positive), unless none stays above 0.25: then all four are stored.
                for s in 1..values.len() {
                    if values[s].dot(values[s - 1]) < 0.0 {
                        values[s] = -values[s];
                    }
                }
                let smallest =
                    |i: usize| values.iter().map(|v| v[i].abs()).fold(f32::MAX, f32::min);
                let (best, magnitude) = (0..4)
                    .map(|i| (i, smallest(i)))
                    .fold((3, f32::MIN), |a, b| if b.1 > a.1 { b } else { a });
                if magnitude >= 0.25 {
                    layout = best as u8;
                    if values[0][best] < 0.0 {
                        values.iter_mut().for_each(|v| *v = -*v);
                    }
                } else {
                    layout = ALL_FOUR;
                }
            }
            let (mut min, mut max) = (Vec4::splat(f32::MAX), Vec4::splat(f32::MIN));
            for v in &values {
                let c = stored(c, *v, layout);
                min = min.min(c);
                max = max.max(c);
            }
            TrackValues {
                values,
                min,
                max,
                layout,
                lo: [0; 4],
                hi: [255; 4],
            }
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
                Kind::Animated([0; 4])
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
        goal: vec![threshold; joints],
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

/// A joint's tracks (a helper that keeps the borrow of `tracks` short).
fn tracks_ref(tracks: &[[TrackValues; 3]], j: usize) -> &[TrackValues; 3] {
    &tracks[j]
}

/// Writes the clip: constants, animated tracks, then per segment its tracks' widths and
/// ranges, and its frames.
fn encode(
    raw: &RawClip,
    c: &Compressor<'_>,
    segments: &[Compressor<'_>],
    joints: usize,
    duration: f32,
    additive: bool,
) -> CompressedClip {
    let mut constants = Vec::new();
    let mut animated = Vec::new();
    let mut animated_at = Vec::new();
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
                Kind::Animated(_) => {
                    animated.push(AnimatedTrack {
                        joint: j as u16,
                        ..track.quantized(channel, [0; 4]).0
                    });
                    animated_at.push((j, channel as usize));
                }
            }
        }
    }
    let mut data = Vec::new();
    let mut table = Vec::new();
    let mut segment_tracks = Vec::new();
    for (segment, (first, count)) in segments.iter().zip(segment_layout(c.samples as u32)) {
        let mut offset = 0u32;
        let tracks: Vec<SegmentTrack> = animated_at
            .iter()
            .zip(&animated)
            .map(|(&(j, ch), t)| {
                let Kind::Animated(rates) = segment.kinds[j][ch] else {
                    unreachable!("animated in the clip, animated in every segment")
                };
                let bits = rates.map(|r| BIT_RATES[r as usize]);
                let st = segment.tracks[j][ch].quantized(t.channel, bits).1;
                offset += sample_bits(st.bits, t.layout);
                st
            })
            .collect();
        let frame_bits = offset;
        let start = data.len();
        data.resize(
            start + (u64::from(count) * u64::from(frame_bits)).div_ceil(8) as usize,
            0,
        );
        let mut put = |at: u64, value: u32, bits: u32| {
            for b in 0..bits {
                if value >> b & 1 != 0 {
                    let bit = at + u64::from(b);
                    data[start + (bit >> 3) as usize] |= 1 << (bit & 7);
                }
            }
        };
        for local in 0..count as usize {
            let frame = local as u64 * u64::from(frame_bits);
            let mut at = frame;
            for ((&(j, ch), t), st) in animated_at.iter().zip(&animated).zip(&tracks) {
                let v = stored(t.channel, segment.tracks[j][ch].values[local], t.layout);
                let (min, scale) = segment_range(t, st);
                let next = at + u64::from(sample_bits(st.bits, t.layout));
                for k in 0..component_count(t.layout) as usize {
                    let bits = st.bits[k];
                    put(at, quantize(v[k], min[k], scale[k], bits), u32::from(bits));
                    at += u64::from(bits);
                }
                at = next;
            }
        }
        table.push(Segment {
            first,
            count,
            frame_bits,
            data: start as u32,
        });
        segment_tracks.extend(tracks);
    }
    data.extend_from_slice(&[0; 8]);
    CompressedClip {
        name: raw.name.clone(),
        duration,
        sample_count: c.samples as u32,
        joint_count: joints as u16,
        additive,
        constants,
        animated,
        segments: table,
        segment_tracks,
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
