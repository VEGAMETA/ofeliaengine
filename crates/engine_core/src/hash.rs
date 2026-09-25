//! Stable content hashing for caches (shader cache, derived data cache).
//!
//! The results must never change between builds, toolchains or machines: they name files in
//! caches shared across the team. `std::hash::DefaultHasher` makes no such promise, so this
//! is 128-bit FNV-1a (Fowler–Noll–Vo, public domain), which is simple, deterministic and more
//! than wide enough for cache keys. It is not a cryptographic hash.

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
    fn hex_is_32_chars() {
        assert_eq!(to_hex(1), "00000000000000000000000000000001");
        assert_eq!(to_hex(hash128(b"")).len(), 32);
    }
}
