//! What travels: the replicated components with their field codecs, and the event types.
//!
//! A component is replicated by one line, `NetSchema::replicate::<T>()`; its fields and
//! their precision come from reflection (`#[derive(Reflect)]`, ADR 0022):
//!
//! - `#[reflect(net_quantum = 0.001)]` on an `f32`: sent as an integer number of
//!   quanta (here millimetres), so small changes cost a few bits;
//! - `#[reflect(net_skip)]` (or `skip`): not replicated (stays as the receiver has it);
//! - a `Quat` field: "smallest three" (the largest component's index and the other three
//!   at 15 bits each, Fiedler's "Snapshot Compression"); `quat_bits` overrides;
//! - integers, enums and booleans: exact; `f32` without a quantum: exact (32 bits).
//!
//! Server and client build the schema with the same function: its hash is checked when a
//! client joins, so builds that disagree cannot talk.

use core::any::TypeId;
use engine_ecs::{Component, ComponentId, World};
use engine_reflect::{Primitive, Reflect, TypeInfo, TypeKind};

/// Most replicated component types per schema (one bit each in a 64-bit mask).
pub const MAX_COMPONENTS: usize = 64;

/// How one word of a component's network state is coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WordKind {
    /// An integer: deltas are zigzag exponential-Golomb coded.
    Int,
    /// 32 raw bits (unquantized floats, halves of 64-bit values).
    Raw,
    /// One bit.
    Bit,
}

/// How a leaf field becomes words, and back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Codec {
    /// `f32` in quanta of `q` (one `Int` word); interpolated linearly.
    F32Quantized { offset: u32, q: f64, inv: f64 },
    /// `f32` bits (one `Raw` word); interpolated linearly.
    F32 { offset: u32 },
    /// An integer of `bytes` bytes, sign-extended or not (one `Int` word).
    Int {
        offset: u32,
        bytes: u8,
        signed: bool,
    },
    /// A `bool` (one `Bit` word).
    Bool { offset: u32 },
    /// Eight bytes (`u64`, `i64`, `f64`): two `Raw` words.
    Wide { offset: u32 },
    /// A unit quaternion, smallest three: index (`Int`) and three components (`Int`) in
    /// `bits`-bit precision; interpolated by normalized lerp.
    Quat { offset: u32, bits: u8 },
}

impl Codec {
    pub(crate) fn words(self) -> usize {
        match self {
            Codec::Wide { .. } => 2,
            Codec::Quat { .. } => 4,
            _ => 1,
        }
    }
}

/// A replicated component type.
#[derive(Debug)]
pub(crate) struct NetComponent {
    pub(crate) name: &'static str,
    pub(crate) type_id: TypeId,
    pub(crate) size: usize,
    pub(crate) codecs: Vec<Codec>,
    /// One per word.
    pub(crate) kinds: Vec<WordKind>,
    pub(crate) words: usize,
    pub(crate) register: fn(&mut World) -> ComponentId,
}

/// A one-shot event type (server to clients or clients to server), plain data.
#[derive(Debug)]
pub(crate) struct NetEventType {
    pub(crate) name: &'static str,
    pub(crate) type_id: TypeId,
    pub(crate) size: usize,
}

/// Per-type overrides for components the game cannot annotate (engine types such as
/// `Transform`): quanta by field path.
#[derive(Clone, Debug, Default)]
pub struct FieldHints {
    quanta: Vec<(&'static str, f64)>,
    skip: Vec<&'static str>,
    quat_bits: Option<u8>,
}

impl FieldHints {
    /// No overrides.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends the `f32` fields at or under `path` (`"translation"`, `"translation.y"`) in
    /// quanta of `q`.
    pub fn quantum(mut self, path: &'static str, q: f64) -> Self {
        self.quanta.push((path, q));
        self
    }

    /// Does not replicate the fields at or under `path`.
    pub fn skip(mut self, path: &'static str) -> Self {
        self.skip.push(path);
        self
    }

    /// Bits per quaternion component (default 15; 9 to 16).
    pub fn quat_bits(mut self, bits: u8) -> Self {
        self.quat_bits = Some(bits.clamp(9, 16));
        self
    }

    fn quantum_for(&self, path: &str) -> Option<f64> {
        self.quanta
            .iter()
            .rev()
            .find(|(p, _)| under(path, p))
            .map(|&(_, q)| q)
    }

    fn skipped(&self, path: &str) -> bool {
        self.skip.iter().any(|p| under(path, p))
    }
}

fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || (path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.as_bytes()[prefix.len()] == b'.')
}

/// The replicated components and event types of a game, in a fixed order. Build it with
/// the same code on server and client.
///
/// ```
/// use engine_ecs::{bytemuck, reflect_component};
/// use engine_net::NetSchema;
/// use engine_reflect::Reflect;
///
/// #[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable, Reflect)]
/// #[repr(C)]
/// struct Door {
///     #[reflect(net_quantum = 0.01)]
///     open: f32,
///     locked: u32,
/// }
/// reflect_component!(pod Door);
///
/// let schema = NetSchema::new(0x0f1e_c0de).replicate::<Door>();
/// assert_eq!(schema.component_count(), 1);
/// ```
#[derive(Debug)]
pub struct NetSchema {
    pub(crate) protocol_id: u64,
    pub(crate) components: Vec<NetComponent>,
    pub(crate) events: Vec<NetEventType>,
}

impl NetSchema {
    /// An empty schema for the game's protocol id (the transport refuses peers with
    /// another; change it when the game's network messages change).
    pub fn new(protocol_id: u64) -> Self {
        Self {
            protocol_id,
            components: Vec::new(),
            events: Vec::new(),
        }
    }

    /// The protocol id.
    pub fn protocol_id(&self) -> u64 {
        self.protocol_id
    }

    /// Replicates `T` with the precision its reflection attributes give.
    ///
    /// # Panics
    ///
    /// If `T` is registered twice, if more than [`MAX_COMPONENTS`] are registered, or if
    /// `T` has no replicable field.
    #[must_use]
    pub fn replicate<T>(self) -> Self
    where
        T: Component + Reflect + bytemuck::Pod,
    {
        self.replicate_with::<T>(FieldHints::default())
    }

    /// Replicates `T` with overrides for fields it cannot annotate itself.
    ///
    /// # Panics
    ///
    /// As [`Self::replicate`].
    #[must_use]
    pub fn replicate_with<T>(mut self, hints: FieldHints) -> Self
    where
        T: Component + Reflect + bytemuck::Pod,
    {
        assert!(
            self.components.len() < MAX_COMPONENTS,
            "at most {MAX_COMPONENTS} replicated components"
        );
        assert!(
            !self
                .components
                .iter()
                .any(|c| c.type_id == TypeId::of::<T>()),
            "{} is replicated twice",
            core::any::type_name::<T>()
        );
        let mut codecs = Vec::new();
        let mut path = String::new();
        collect(
            <T as Reflect>::TYPE_INFO,
            0,
            &hints,
            &mut path,
            &mut codecs,
            false,
        );
        assert!(
            !codecs.is_empty(),
            "{} has no replicable fields",
            core::any::type_name::<T>()
        );
        let mut kinds = Vec::new();
        for c in &codecs {
            match *c {
                Codec::F32Quantized { .. } | Codec::Int { .. } => kinds.push(WordKind::Int),
                Codec::F32 { .. } => kinds.push(WordKind::Raw),
                Codec::Bool { .. } => kinds.push(WordKind::Bit),
                Codec::Wide { .. } => kinds.extend([WordKind::Raw, WordKind::Raw]),
                Codec::Quat { .. } => kinds.extend([WordKind::Int; 4]),
            }
        }
        self.components.push(NetComponent {
            name: <T as Reflect>::TYPE_INFO.name,
            type_id: TypeId::of::<T>(),
            size: core::mem::size_of::<T>(),
            words: kinds.len(),
            codecs,
            kinds,
            register: |w| w.register::<T>(),
        });
        self
    }

    /// Adds a one-shot event type (plain data). Events are for things that happen once
    /// (a door opened, a hit for effects), never for state: state is replicated.
    ///
    /// # Panics
    ///
    /// If `E` is registered twice.
    #[must_use]
    pub fn event<E: bytemuck::Pod + Send + Sync + 'static>(mut self) -> Self {
        assert!(
            !self.events.iter().any(|e| e.type_id == TypeId::of::<E>()),
            "{} is registered twice",
            core::any::type_name::<E>()
        );
        self.events.push(NetEventType {
            name: core::any::type_name::<E>(),
            type_id: TypeId::of::<E>(),
            size: core::mem::size_of::<E>(),
        });
        self
    }

    /// Number of replicated component types.
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Words of network state of every component together (the largest entity state).
    pub fn max_words(&self) -> usize {
        self.components.iter().map(|c| c.words).sum()
    }

    /// A hash of everything that shapes the wire format (names, layouts, codecs), checked
    /// when a client joins.
    pub fn hash(&self) -> u64 {
        let mut h = Fnv(0xcbf2_9ce4_8422_2325);
        h.u64(self.protocol_id);
        h.u64(crate::WIRE_VERSION);
        for c in &self.components {
            h.bytes(c.name.as_bytes());
            h.u64(c.size as u64);
            for codec in &c.codecs {
                match *codec {
                    Codec::F32Quantized { offset, q, .. } => {
                        h.u64(1);
                        h.u64(u64::from(offset));
                        h.u64(q.to_bits());
                    }
                    Codec::F32 { offset } => {
                        h.u64(2);
                        h.u64(u64::from(offset));
                    }
                    Codec::Int {
                        offset,
                        bytes,
                        signed,
                    } => {
                        h.u64(3);
                        h.u64(u64::from(offset));
                        h.u64(u64::from(bytes) | (u64::from(signed) << 8));
                    }
                    Codec::Bool { offset } => {
                        h.u64(4);
                        h.u64(u64::from(offset));
                    }
                    Codec::Wide { offset } => {
                        h.u64(5);
                        h.u64(u64::from(offset));
                    }
                    Codec::Quat { offset, bits } => {
                        h.u64(6);
                        h.u64(u64::from(offset));
                        h.u64(u64::from(bits));
                    }
                }
            }
        }
        for e in &self.events {
            h.bytes(e.name.as_bytes());
            h.u64(e.size as u64);
        }
        h.0
    }

    /// Endpoint settings with the channels replication uses: 0 reliable-ordered
    /// (control, reliable events), 1 unreliable with delivery notification (snapshots),
    /// 2 unreliable (inputs, unreliable events).
    pub fn endpoint_config(&self) -> engine_net_transport::EndpointConfig {
        use engine_net_transport::ChannelConfig;
        let mut c = engine_net_transport::EndpointConfig::new(self.protocol_id);
        c.channels = vec![
            ChannelConfig::reliable(),
            ChannelConfig::unreliable().with_notify(),
            ChannelConfig::unreliable(),
        ];
        c
    }

    pub(crate) fn component_index(&self, t: TypeId) -> Option<usize> {
        self.components.iter().position(|c| c.type_id == t)
    }

    pub(crate) fn event_index(&self, t: TypeId) -> Option<usize> {
        self.events.iter().position(|e| e.type_id == t)
    }
}

struct Fnv(u64);

impl Fnv {
    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= u64::from(x);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.u64(b.len() as u64);
    }

    fn u64(&mut self, v: u64) {
        for x in v.to_le_bytes() {
            self.0 ^= u64::from(x);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

const DEFAULT_QUAT_BITS: u8 = 15;

fn collect(
    ty: &'static TypeInfo,
    offset: usize,
    hints: &FieldHints,
    path: &mut String,
    out: &mut Vec<Codec>,
    skipped: bool,
) {
    if skipped || hints.skipped(path) {
        return;
    }
    let off = offset as u32;
    match ty.kind {
        TypeKind::Struct(fields) => {
            if ty.name == "Quat" && ty.size == 16 {
                out.push(Codec::Quat {
                    offset: off,
                    bits: hints.quat_bits.unwrap_or(DEFAULT_QUAT_BITS),
                });
                return;
            }
            for f in fields {
                let len = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(f.name);
                let skip = f.attrs.net_skip || f.attrs.skip;
                // A field's quantum applies to the floats under it.
                if let (Some(q), false) = (f.attrs.net_quantum, skip) {
                    collect_quantized(f.ty, offset + f.offset, q, hints, path, out);
                } else {
                    collect(f.ty, offset + f.offset, hints, path, out, skip);
                }
                path.truncate(len);
            }
        }
        TypeKind::Array { element, len } => {
            for i in 0..len {
                let old = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(&i.to_string());
                collect(element, offset + i * element.size, hints, path, out, false);
                path.truncate(old);
            }
        }
        TypeKind::Primitive(p) => {
            if let Some(q) = hints.quantum_for(path).filter(|_| p == Primitive::F32) {
                out.push(quantized(off, q));
            } else {
                out.push(primitive(p, off));
            }
        }
        TypeKind::Enum { repr, .. } => out.push(primitive(repr, off)),
        TypeKind::Opaque => {}
    }
}

fn collect_quantized(
    ty: &'static TypeInfo,
    offset: usize,
    q: f64,
    hints: &FieldHints,
    path: &mut String,
    out: &mut Vec<Codec>,
) {
    match ty.kind {
        TypeKind::Primitive(Primitive::F32) => out.push(quantized(offset as u32, q)),
        TypeKind::Struct(fields) if ty.name != "Quat" => {
            for f in fields {
                if f.attrs.net_skip || f.attrs.skip {
                    continue;
                }
                let len = path.len();
                path.push('.');
                path.push_str(f.name);
                collect_quantized(f.ty, offset + f.offset, q, hints, path, out);
                path.truncate(len);
            }
        }
        TypeKind::Array { element, len } => {
            for i in 0..len {
                collect_quantized(element, offset + i * element.size, q, hints, path, out);
            }
        }
        _ => collect(ty, offset, hints, path, out, false),
    }
}

fn quantized(offset: u32, q: f64) -> Codec {
    let q = if q > 0.0 && q.is_finite() { q } else { 1.0 };
    Codec::F32Quantized {
        offset,
        q,
        inv: 1.0 / q,
    }
}

fn primitive(p: Primitive, offset: u32) -> Codec {
    match p {
        Primitive::Bool => Codec::Bool { offset },
        Primitive::F32 => Codec::F32 { offset },
        Primitive::I8 => Codec::Int {
            offset,
            bytes: 1,
            signed: true,
        },
        Primitive::I16 => Codec::Int {
            offset,
            bytes: 2,
            signed: true,
        },
        Primitive::I32 => Codec::Int {
            offset,
            bytes: 4,
            signed: true,
        },
        Primitive::U8 => Codec::Int {
            offset,
            bytes: 1,
            signed: false,
        },
        Primitive::U16 => Codec::Int {
            offset,
            bytes: 2,
            signed: false,
        },
        Primitive::U32 => Codec::Int {
            offset,
            bytes: 4,
            signed: false,
        },
        Primitive::I64 | Primitive::U64 | Primitive::F64 => Codec::Wide { offset },
    }
}

// ---------------------------------------------------------------------------------------
// Quantization: component bytes ↔ words. Deterministic (same binary on both sides).
// ---------------------------------------------------------------------------------------

#[inline]
fn rd<const N: usize>(bytes: &[u8], offset: u32) -> [u8; N] {
    let o = offset as usize;
    let mut a = [0u8; N];
    a.copy_from_slice(&bytes[o..o + N]);
    a
}

#[inline]
fn wr(bytes: &mut [u8], offset: u32, v: &[u8]) {
    let o = offset as usize;
    bytes[o..o + v.len()].copy_from_slice(v);
}

/// Smallest-three precision: components lie in ±1/√2.
const QUAT_RANGE: f64 = core::f64::consts::FRAC_1_SQRT_2;

#[inline]
pub(crate) fn quantize_f32(v: f32, inv: f64) -> u32 {
    let x = (f64::from(v) * inv).round();
    // Saturate instead of wrapping (and NaN to 0).
    let x = if x.is_nan() {
        0.0
    } else {
        x.clamp(f64::from(i32::MIN), f64::from(i32::MAX))
    };
    (x as i32) as u32
}

#[inline]
pub(crate) fn dequantize_f32(w: u32, q: f64) -> f32 {
    (f64::from(w as i32) * q) as f32
}

impl NetComponent {
    /// Quantizes the component at `bytes` into `out` (`self.words` long).
    pub(crate) fn quantize(&self, bytes: &[u8], out: &mut [u32]) {
        let mut w = 0;
        for c in &self.codecs {
            match *c {
                Codec::F32Quantized { offset, inv, .. } => {
                    out[w] = quantize_f32(f32::from_le_bytes(rd(bytes, offset)), inv);
                }
                Codec::F32 { offset } => out[w] = u32::from_le_bytes(rd(bytes, offset)),
                Codec::Int {
                    offset,
                    bytes: n,
                    signed,
                } => {
                    out[w] = match (n, signed) {
                        (1, true) => i32::from(i8::from_le_bytes(rd(bytes, offset))) as u32,
                        (1, false) => u32::from(u8::from_le_bytes(rd(bytes, offset))),
                        (2, true) => i32::from(i16::from_le_bytes(rd(bytes, offset))) as u32,
                        (2, false) => u32::from(u16::from_le_bytes(rd(bytes, offset))),
                        _ => u32::from_le_bytes(rd(bytes, offset)),
                    };
                }
                Codec::Bool { offset } => out[w] = u32::from(bytes[offset as usize] != 0),
                Codec::Wide { offset } => {
                    let v = u64::from_le_bytes(rd(bytes, offset));
                    out[w] = v as u32;
                    out[w + 1] = (v >> 32) as u32;
                }
                Codec::Quat { offset, bits } => {
                    let q = [
                        f32::from_le_bytes(rd(bytes, offset)),
                        f32::from_le_bytes(rd(bytes, offset + 4)),
                        f32::from_le_bytes(rd(bytes, offset + 8)),
                        f32::from_le_bytes(rd(bytes, offset + 12)),
                    ];
                    quantize_quat(q, bits, &mut out[w..w + 4]);
                }
            }
            w += c.words();
        }
    }

    /// Writes the values of `words` into the component at `bytes` (fields that are not
    /// replicated keep their bytes).
    pub(crate) fn dequantize(&self, words: &[u32], bytes: &mut [u8]) {
        let mut w = 0;
        for c in &self.codecs {
            match *c {
                Codec::F32Quantized { offset, q, .. } => {
                    wr(bytes, offset, &dequantize_f32(words[w], q).to_le_bytes());
                }
                Codec::F32 { offset } => wr(bytes, offset, &words[w].to_le_bytes()),
                Codec::Int {
                    offset, bytes: n, ..
                } => wr(bytes, offset, &words[w].to_le_bytes()[..n as usize]),
                Codec::Bool { offset } => bytes[offset as usize] = u8::from(words[w] != 0),
                Codec::Wide { offset } => {
                    let v = u64::from(words[w]) | (u64::from(words[w + 1]) << 32);
                    wr(bytes, offset, &v.to_le_bytes());
                }
                Codec::Quat { offset, bits } => {
                    let q = dequantize_quat(&words[w..w + 4], bits);
                    for (i, v) in q.iter().enumerate() {
                        wr(bytes, offset + 4 * i as u32, &v.to_le_bytes());
                    }
                }
            }
            w += c.words();
        }
    }

    /// Writes the interpolation between `a` and `b` at `t` into the component at
    /// `bytes`: floats linearly, quaternions by normalized lerp on the shorter arc,
    /// everything else from `a` (`b` once `t >= 1`). `t > 1` extrapolates the floats.
    pub(crate) fn interpolate(&self, a: &[u32], b: &[u32], t: f32, bytes: &mut [u8]) {
        if t <= 0.0 {
            return self.dequantize(a, bytes);
        }
        if t == 1.0 {
            return self.dequantize(b, bytes);
        }
        let mut w = 0;
        for c in &self.codecs {
            match *c {
                Codec::F32Quantized { offset, q, .. } => {
                    let (x, y) = (dequantize_f32(a[w], q), dequantize_f32(b[w], q));
                    wr(bytes, offset, &(x + (y - x) * t).to_le_bytes());
                }
                Codec::F32 { offset } => {
                    let (x, y) = (f32::from_bits(a[w]), f32::from_bits(b[w]));
                    wr(bytes, offset, &(x + (y - x) * t).to_le_bytes());
                }
                Codec::Quat { offset, bits } => {
                    let x = dequantize_quat(&a[w..w + 4], bits);
                    let mut y = dequantize_quat(&b[w..w + 4], bits);
                    let dot: f32 = x.iter().zip(&y).map(|(p, q)| p * q).sum();
                    if dot < 0.0 {
                        y = y.map(|v| -v);
                    }
                    let mut r = [0f32; 4];
                    for i in 0..4 {
                        r[i] = x[i] + (y[i] - x[i]) * t;
                    }
                    let len = r.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-12);
                    for (i, v) in r.iter().enumerate() {
                        wr(bytes, offset + 4 * i as u32, &(v / len).to_le_bytes());
                    }
                }
                Codec::Int { .. } | Codec::Bool { .. } | Codec::Wide { .. } => {
                    // Discrete values switch at the newer state's tick.
                    let n = c.words();
                    let mut tmp = [0u32; 2];
                    let src = if t >= 1.0 { b } else { a };
                    tmp[..n].copy_from_slice(&src[w..w + n]);
                    self.dequantize_one(c, &tmp[..n], bytes);
                }
            }
            w += c.words();
        }
    }

    fn dequantize_one(&self, c: &Codec, words: &[u32], bytes: &mut [u8]) {
        match *c {
            Codec::Int {
                offset, bytes: n, ..
            } => wr(bytes, offset, &words[0].to_le_bytes()[..n as usize]),
            Codec::Bool { offset } => bytes[offset as usize] = u8::from(words[0] != 0),
            Codec::Wide { offset } => {
                let v = u64::from(words[0]) | (u64::from(words[1]) << 32);
                wr(bytes, offset, &v.to_le_bytes());
            }
            _ => {}
        }
    }
}

fn quantize_quat(q: [f32; 4], bits: u8, out: &mut [u32]) {
    let len = q
        .iter()
        .map(|v| f64::from(*v) * f64::from(*v))
        .sum::<f64>()
        .sqrt();
    let n: [f64; 4] = if len > 1e-12 {
        q.map(|v| f64::from(v) / len)
    } else {
        [0.0, 0.0, 0.0, 1.0]
    };
    let mut largest = 0;
    for i in 1..4 {
        if n[i].abs() > n[largest].abs() {
            largest = i;
        }
    }
    let sign = if n[largest] < 0.0 { -1.0 } else { 1.0 };
    let scale = f64::from((1i32 << (bits - 1)) - 1) / QUAT_RANGE;
    out[0] = largest as u32;
    let mut k = 1;
    for (i, v) in n.iter().enumerate() {
        if i != largest {
            let x = (v * sign * scale).round().clamp(
                -f64::from((1i32 << (bits - 1)) - 1),
                f64::from((1i32 << (bits - 1)) - 1),
            );
            out[k] = (x as i32) as u32;
            k += 1;
        }
    }
}

fn dequantize_quat(w: &[u32], bits: u8) -> [f32; 4] {
    let inv = QUAT_RANGE / f64::from((1i32 << (bits - 1)) - 1);
    let largest = (w[0] & 3) as usize;
    let mut out = [0f64; 4];
    let mut sum = 0.0;
    let mut k = 1;
    for (i, o) in out.iter_mut().enumerate() {
        if i != largest {
            *o = f64::from(w[k] as i32) * inv;
            sum += *o * *o;
            k += 1;
        }
    }
    out[largest] = (1.0 - sum).max(0.0).sqrt();
    out.map(|v| v as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::math::{Quat, Vec3};
    use engine_ecs::{Transform, bytemuck, reflect_component};

    #[derive(
        Clone, Copy, Default, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable, Reflect,
    )]
    #[repr(C)]
    struct Sample {
        rotation: Quat,
        #[reflect(net_quantum = 0.001)]
        position: Vec3,
        health: i16,
        flags: u8,
        alive: u8,
        exact: f32,
        #[reflect(net_skip)]
        local: f32,
        big: u64,
    }
    reflect_component!(pod Sample);

    #[test]
    fn fields_follow_the_reflection_hints() {
        let s = NetSchema::new(1).replicate::<Sample>();
        let c = &s.components[0];
        // quat (4), position (3), health, flags, alive, exact, big (2).
        assert_eq!(c.words, 3 + 4 + 1 + 1 + 1 + 1 + 2);
        assert!(matches!(c.codecs[0], Codec::Quat { bits: 15, .. }));
        assert!(matches!(c.codecs[1], Codec::F32Quantized { .. }));
        assert!(!c.codecs.iter().any(|k| matches!(k, Codec::F32 { offset } if *offset as usize == core::mem::offset_of!(Sample, local))));
    }

    #[test]
    fn quantization_round_trips_within_the_quantum() {
        let s = NetSchema::new(1).replicate::<Sample>();
        let c = &s.components[0];
        let v = Sample {
            position: Vec3::new(12.345_67, -0.000_4, 1000.0),
            rotation: Quat::from_euler(engine_core::math::EulerRot::YXZ, 1.0, -0.5, 2.0),
            health: -7,
            flags: 200,
            alive: 1,
            exact: 1.234_567_8,
            local: 9.0,
            big: u64::MAX - 5,
        };
        let mut words = vec![0u32; c.words];
        c.quantize(bytemuck::bytes_of(&v), &mut words);
        let mut out = Sample::default();
        c.dequantize(&words, bytemuck::bytes_of_mut(&mut out));
        assert!((out.position - v.position).abs().max_element() <= 0.0005 + 1e-6);
        assert!(out.rotation.dot(v.rotation).abs() > 1.0 - 1e-6);
        assert_eq!(
            (out.health, out.flags, out.alive, out.exact, out.big),
            (-7, 200, 1, v.exact, v.big)
        );
        assert_eq!(out.local, 0.0, "not replicated");
        // Quantizing the dequantized value gives the same words (snapping is stable).
        let mut again = vec![0u32; c.words];
        c.quantize(bytemuck::bytes_of(&out), &mut again);
        assert_eq!(words, again);
    }

    #[test]
    fn interpolation_blends_floats_and_rotations() {
        let s = NetSchema::new(1).replicate::<Sample>();
        let c = &s.components[0];
        let a = Sample {
            rotation: Quat::IDENTITY,
            health: 1,
            ..Sample::default()
        };
        let b = Sample {
            position: Vec3::new(2.0, 0.0, 0.0),
            rotation: Quat::from_rotation_y(1.0),
            health: 2,
            ..Sample::default()
        };
        let (mut wa, mut wb) = (vec![0u32; c.words], vec![0u32; c.words]);
        c.quantize(bytemuck::bytes_of(&a), &mut wa);
        c.quantize(bytemuck::bytes_of(&b), &mut wb);
        let mut out = Sample::default();
        c.interpolate(&wa, &wb, 0.25, bytemuck::bytes_of_mut(&mut out));
        assert!((out.position.x - 0.5).abs() < 1e-3);
        // Normalized lerp: within 0.004 rad of the slerp at 1 rad.
        assert!(out.rotation.angle_between(Quat::from_rotation_y(0.25)) < 5e-3);
        assert_eq!(out.health, 1, "integers switch at the newer tick");
    }

    #[test]
    fn engine_types_take_hints_and_the_hash_sees_them() {
        let a = NetSchema::new(1).replicate_with::<Transform>(
            FieldHints::new()
                .quantum("translation", 0.001)
                .skip("scale"),
        );
        let b = NetSchema::new(1).replicate::<Transform>();
        let c = &a.components[0];
        // rotation 4 + translation 3; padding is `skip`, scale skipped.
        assert_eq!(c.words, 7);
        assert_ne!(a.hash(), b.hash());
        assert_eq!(
            a.hash(),
            NetSchema::new(1)
                .replicate_with::<Transform>(
                    FieldHints::new()
                        .quantum("translation", 0.001)
                        .skip("scale")
                )
                .hash()
        );
    }
}
