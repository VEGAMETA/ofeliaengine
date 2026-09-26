//! Stable hashing: content keys for caches (shader cache, derived data cache) and fast
//! simulation state hashes for desync detection.
//!
//! Cache keys must never change between builds, toolchains or machines: they name files in
//! caches shared across the team. `std::hash::DefaultHasher` makes no such promise, so
//! [`StableHasher`] is 128-bit FNV-1a (Fowler–Noll–Vo, public domain), which is simple,
//! deterministic and more than wide enough for cache keys. [`StateHasher`] trades that
//! width for speed on megabytes of simulation state. Neither is a cryptographic hash.

/// FNV-1a 128-bit offset basis.
const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
/// FNV-1a 128-bit prime (2^88 + 2^8 + 0x3b).
const PRIME: u128 = 0x0000000001000000000000000000013b;

/// Incremental 128-bit FNV-1a hasher.
///
/// ```
/// use engine_core::hash::StableHasher;
///
/// let mut h = StableHasher::new();
/// h.write(b"shader.slang");
/// h.write_u64(42);
/// let key = h.finish();
/// assert_eq!(key, {
///     let mut h = StableHasher::new();
///     h.write(b"shader.slang");
///     h.write_u64(42);
///     h.finish()
/// });
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StableHasher {
    state: u128,
}

impl Default for StableHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl StableHasher {
    /// A hasher in its initial state.
    pub const fn new() -> Self {
        Self { state: OFFSET }
    }

    /// Feeds bytes.
    #[inline]
    pub fn write(&mut self, bytes: &[u8]) {
        let mut state = self.state;
        for &b in bytes {
            state ^= u128::from(b);
            state = state.wrapping_mul(PRIME);
        }
        self.state = state;
    }

    /// Feeds a length-prefixed byte string, so that `("ab", "c")` and `("a", "bc")` hash
    /// differently.
    #[inline]
    pub fn write_str(&mut self, s: &str) {
        self.write_u64(s.len() as u64);
        self.write(s.as_bytes());
    }

    /// Feeds an integer (little-endian bytes).
    #[inline]
    pub fn write_u64(&mut self, value: u64) {
        self.write(&value.to_le_bytes());
    }

    /// The hash so far.
    pub const fn finish(&self) -> u128 {
        self.state
    }
}

/// Hash of `bytes`.
pub fn hash128(bytes: &[u8]) -> u128 {
    let mut h = StableHasher::new();
    h.write(bytes);
    h.finish()
}

/// Lower-case hexadecimal form of a 128-bit hash (32 characters), for file names.
pub fn to_hex(hash: u128) -> String {
    format!("{hash:032x}")
}

const STATE_K: u64 = 0x9e37_79b9_7f4a_7c15;
const STATE_SEEDS: [u64; 4] = [
    0x243f_6a88_85a3_08d3,
    0x1319_8a2e_0370_7344,
    0xa409_3822_299f_31d0,
    0x082e_fa98_ec4e_6c89,
];

/// Fast 64-bit hash of simulation state, for desync detection and determinism tests.
///
/// Four independent lanes each take one little-endian word per 32-byte block. Every step
/// (xor with the word, multiply by an odd constant, rotate) is a bijection of the lane, so
/// a difference in a single word always changes the result. It is not a cryptographic hash
/// and not a cache key (use [`StableHasher`] for those). It is stable across builds for
/// the same byte stream; comparisons are between runs, never against stored values.
///
/// ```
/// use engine_core::hash::StateHasher;
///
/// let mut a = StateHasher::new();
/// a.write(&[1, 2, 3]);
/// a.write(&[4]);
/// let mut b = StateHasher::new();
/// b.write(&[1, 2, 3, 4]);
/// assert_eq!(a.finish(), b.finish());
/// ```
#[derive(Debug, Clone)]
pub struct StateHasher {
    lanes: [u64; 4],
    block: [u8; 32],
    block_len: usize,
    total: u64,
}

impl Default for StateHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl StateHasher {
    /// A hasher in its initial state.
    pub const fn new() -> Self {
        Self {
            lanes: STATE_SEEDS,
            block: [0; 32],
            block_len: 0,
            total: 0,
        }
    }

    #[inline(always)]
    fn round(lanes: &mut [u64; 4], block: &[u8]) {
        for (i, lane) in lanes.iter_mut().enumerate() {
            let mut word = [0u8; 8];
            word.copy_from_slice(&block[i * 8..i * 8 + 8]);
            *lane = (*lane ^ u64::from_le_bytes(word))
                .wrapping_mul(STATE_K)
                .rotate_left(31);
        }
    }

    /// Feeds bytes. Splitting the same stream differently gives the same hash.
    #[inline]
    pub fn write(&mut self, mut bytes: &[u8]) {
        self.total = self.total.wrapping_add(bytes.len() as u64);
        if self.block_len > 0 {
            let take = (32 - self.block_len).min(bytes.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&bytes[..take]);
            self.block_len += take;
            bytes = &bytes[take..];
            if self.block_len < 32 {
                return;
            }
            let block = self.block;
            Self::round(&mut self.lanes, &block);
            self.block_len = 0;
        }
        let mut blocks = bytes.chunks_exact(32);
        for block in &mut blocks {
            Self::round(&mut self.lanes, block);
        }
        let rest = blocks.remainder();
        self.block[..rest.len()].copy_from_slice(rest);
        self.block_len = rest.len();
    }

    /// Feeds an integer (little-endian bytes).
    #[inline]
    pub fn write_u64(&mut self, value: u64) {
        self.write(&value.to_le_bytes());
    }

    /// The hash so far.
    pub fn finish(&self) -> u64 {
        let mut lanes = self.lanes;
        if self.block_len > 0 {
            let mut block = [0u8; 32];
            block[..self.block_len].copy_from_slice(&self.block[..self.block_len]);
            Self::round(&mut lanes, &block);
        }
        let mut h = self.total;
        for lane in lanes {
            h = (h ^ lane).wrapping_mul(STATE_K).rotate_left(27);
        }
        // Final avalanche (MurmurHash3's fmix64).
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        h ^ (h >> 33)
    }
}

/// [`StateHasher`] hash of `bytes`.
pub fn state_hash64(bytes: &[u8]) -> u64 {
    let mut h = StateHasher::new();
    h.write(bytes);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // Reference values for FNV-1a 128 (published test vectors).
        assert_eq!(hash128(b""), 0x6c62272e07bb014262b821756295c58d);
        assert_eq!(hash128(b"a"), 0xd228cb696f1a8caf78912b704e4a8964);
        assert_eq!(hash128(b"foobar"), 0x343e1662793c64bf6f0d3597ba446f18);
    }

    #[test]
    fn length_prefix_separates_fields() {
        let mut a = StableHasher::new();
        a.write_str("ab");
        a.write_str("c");
        let mut b = StableHasher::new();
        b.write_str("a");
        b.write_str("bc");
        assert_ne!(a.finish(), b.finish());
    }

    #[test]
    fn state_hash_is_split_independent_and_sensitive() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let whole = state_hash64(&data);
        for split in [0, 1, 7, 31, 32, 33, 500, 999, 1000] {
            let mut h = StateHasher::new();
            h.write(&data[..split]);
            h.write(&data[split..]);
            assert_eq!(h.finish(), whole, "split at {split}");
        }
        for i in [0, 1, 31, 32, 500, 998, 999] {
            let mut changed = data.clone();
            changed[i] ^= 1;
            assert_ne!(state_hash64(&changed), whole, "byte {i}");
        }
        // Trailing zeros are not free: the length is part of the hash.
        let mut padded = data.clone();
        padded.push(0);
        assert_ne!(state_hash64(&padded), whole);
        assert_ne!(state_hash64(&[]), state_hash64(&[0]));
    }

    #[test]
    fn hex_is_32_chars() {
        assert_eq!(to_hex(1), "00000000000000000000000000000001");
        assert_eq!(to_hex(hash128(b"")).len(), 32);
    }
}
