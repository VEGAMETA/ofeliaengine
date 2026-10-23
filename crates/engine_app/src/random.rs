//! The world's random numbers for scripts (ADR 0114 §4: `random.number()`,
//! `random.integer(a, b)`, `random.vector_in_sphere()`): a generator of plain data on an
//! entity of its own. A component, not a resource, so a snapshot (prediction's rollback,
//! ADR 0074) and a save carry it like any other: the same seed gives the same numbers on
//! every machine, and a re-simulated tick draws what the first run drew.
//!
//! SplitMix64 (Steele, Lea and Flood, "Fast splittable pseudorandom number generators",
//! 2014): one 64-bit word of state, every value of it visited once per period of 2^64, and
//! output that passes BigCrush. Scripts' `math.random` stays outside the simulation.

use engine_core::math::Vec3;
use engine_ecs::bytemuck;
use engine_reflect::Reflect;

/// The seed a world's generator starts from when nothing set one.
pub const DEFAULT_SEED: u64 = 0x6f66_656c_6961_0001;

/// A world's generator ([`crate::scripts`] makes one on its first draw). Runtime data, as
/// a `MeshRenderer` is: out of the inspector and the add menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Reflect)]
#[repr(C)]
#[reflect(hidden)]
pub struct Random {
    /// The generator's state: what it draws next follows from it alone.
    pub state: u64,
}
// SAFETY: `repr(C)` of one `u64`: no padding, and every bit pattern is a valid value.
unsafe impl bytemuck::Zeroable for Random {}
// SAFETY: as above; `Copy` and `'static`.
unsafe impl bytemuck::Pod for Random {}
engine_ecs::reflect_component!(pod Random);

impl Default for Random {
    fn default() -> Self {
        Self::new(DEFAULT_SEED)
    }
}

impl Random {
    /// A generator seeded with `seed`.
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in [0, 1), of 53 random bits.
    pub fn number(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// A whole number from `lo` to `hi`, both included (`lo` ≤ `hi`): Lemire's
    /// multiplication, unbiased enough for any span a game draws from.
    pub fn integer(&mut self, lo: i64, hi: i64) -> i64 {
        let span = hi.wrapping_sub(lo) as u64 as u128 + 1;
        let at = (u128::from(self.next_u64()) * span) >> 64;
        lo.wrapping_add(at as i64)
    }

    /// A point inside the sphere of radius 1, every place as likely: drawn in the cube
    /// around it until one falls inside (about two in three do).
    pub fn in_sphere(&mut self) -> Vec3 {
        loop {
            let v = Vec3::new(
                (self.number() * 2.0 - 1.0) as f32,
                (self.number() * 2.0 - 1.0) as f32,
                (self.number() * 2.0 - 1.0) as f32,
            );
            if v.length_squared() <= 1.0 {
                return v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_gives_its_numbers_and_they_stay_in_their_ranges() {
        let mut a = Random::new(7);
        let mut b = Random::new(7);
        let first: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let again: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        assert_eq!(first, again, "the same seed, the same numbers");
        assert_ne!(Random::new(8).next_u64(), first[0]);
        let mut r = Random::default();
        let mut seen = [0u32; 6];
        for _ in 0..60_000 {
            let n = r.number();
            assert!((0.0..1.0).contains(&n));
            let i = r.integer(1, 6);
            assert!((1..=6).contains(&i));
            seen[(i - 1) as usize] += 1;
            assert!(r.in_sphere().length_squared() <= 1.0);
        }
        // Each face of a die about a sixth of the time.
        for count in seen {
            assert!((9_000..11_000).contains(&count), "{seen:?}");
        }
        assert_eq!(r.integer(5, 5), 5);
        assert_eq!(r.integer(-3, -3), -3);
    }
}
