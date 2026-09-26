//! Stable sort of draw keys: least-significant-digit radix sort over 11-bit digits.
//!
//! Keys already in order (typical for UI and for scenes without y-sorting) are detected
//! first and not moved. Otherwise one read pass computes the histograms of every digit.
//! Digits that are the same for every key are skipped (P. Terdiman, "Radix Sort
//! Revisited", 2000), so y-sorting within one layer (32 varying bits) costs three
//! scatter passes. 11-bit digits keep the histograms (5 × 2048 counters) in L1/L2 while
//! needing a pass fewer than bytes would. Stability keeps submission order for equal keys,
//! which makes the result deterministic.

/// A key, the index of its item, and the item's batch state (in what would be padding).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) key: u64,
    pub(crate) index: u32,
    pub(crate) state: u16,
}

const DIGIT_BITS: u32 = 11;
const BUCKETS: usize = 1 << DIGIT_BITS;
/// Digits covering the 48 key bits that can be non-zero (layer and depth).
const DIGITS: usize = 48_usize.div_ceil(DIGIT_BITS as usize);

/// Sorts `entries` by key, stably, and returns whether they already were in order.
/// `scratch` is resized to match (reused across calls, so steady-state sorting does not
/// allocate).
pub(crate) fn sort(entries: &mut Vec<Entry>, scratch: &mut Vec<Entry>) -> bool {
    let n = entries.len();
    if n < 2 {
        return true;
    }
    // In-order input (UI, a single layer without y-sorting) costs one comparison per key.
    if entries.windows(2).all(|w| w[0].key <= w[1].key) {
        return true;
    }
    let mut histograms = [[0u32; BUCKETS]; DIGITS];
    for entry in entries.iter() {
        for (digit, histogram) in histograms.iter_mut().enumerate() {
            histogram[digit_of(entry.key, digit)] += 1;
        }
    }
    scratch.clear();
    scratch.resize(n, Entry::default());
    let (mut from, mut to) = (entries, scratch);
    let mut swapped = false;
    for (digit, histogram) in histograms.iter_mut().enumerate() {
        // Every key has the same digit here: this pass would not move anything.
        if histogram.iter().any(|&count| count as usize == n) {
            continue;
        }
        // Counts → starting offsets, in place.
        let mut sum = 0;
        for slot in histogram.iter_mut() {
            let count = *slot;
            *slot = sum;
            sum += count;
        }
        for entry in from.iter() {
            let bucket = &mut histogram[digit_of(entry.key, digit)];
            to[*bucket as usize] = *entry;
            *bucket += 1;
        }
        core::mem::swap(&mut from, &mut to);
        swapped = !swapped;
    }
    // After an odd number of passes the result is in the scratch buffer.
    if swapped {
        to.copy_from_slice(from);
    }
    false
}

#[inline(always)]
fn digit_of(key: u64, digit: usize) -> usize {
    ((key >> (digit as u32 * DIGIT_BITS)) as usize) & (BUCKETS - 1)
}

/// Maps a float to an unsigned integer with the same order (−∞ < … < −0 < +0 < … < +∞;
/// NaNs sort after +∞ or before −∞ by sign).
pub(crate) fn ordered_f32(value: f32) -> u32 {
    let bits = value.to_bits();
    if bits & 0x8000_0000 != 0 {
        !bits
    } else {
        bits | 0x8000_0000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state >> 16
    }

    fn check(keys: &[u64]) {
        let mut entries: Vec<Entry> = keys
            .iter()
            .enumerate()
            .map(|(i, &key)| Entry {
                key,
                index: i as u32,
                state: (i % 3) as u16,
            })
            .collect();
        let mut expected = entries.clone();
        expected.sort_by_key(|e| e.key); // stable
        let mut scratch = Vec::new();
        let was_sorted = sort(&mut entries, &mut scratch);
        assert_eq!(entries, expected);
        assert_eq!(was_sorted, keys.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn matches_a_stable_comparison_sort() {
        let mut state = 7;
        for n in [0, 1, 2, 3, 17, 256, 1000, 5000] {
            // Random 48-bit keys, few distinct keys (many ties), one varying byte,
            // already sorted, reversed.
            let random: Vec<u64> = (0..n).map(|_| lcg(&mut state) & 0xFFFF_FFFF_FFFF).collect();
            let ties: Vec<u64> = (0..n).map(|_| lcg(&mut state) % 5).collect();
            let one_byte: Vec<u64> = (0..n).map(|_| (lcg(&mut state) & 0xFF) << 24).collect();
            // Layers only, and layers with depths (the top digits).
            let layers: Vec<u64> = (0..n).map(|_| (lcg(&mut state) % 7) << 40).collect();
            let mixed: Vec<u64> = (0..n)
                .map(|_| ((lcg(&mut state) % 3) << 32) | (lcg(&mut state) & 0xFFFF_FFFF))
                .collect();
            let ascending: Vec<u64> = (0..n as u64).collect();
            let descending: Vec<u64> = (0..n as u64).rev().collect();
            for keys in [random, ties, one_byte, layers, mixed, ascending, descending] {
                check(&keys);
            }
        }
    }

    #[test]
    fn float_order_is_preserved() {
        let values = [
            f32::NEG_INFINITY,
            -1e30,
            -1.5,
            -f32::MIN_POSITIVE,
            -0.0,
            0.0,
            f32::MIN_POSITIVE,
            0.25,
            1.0,
            1e30,
            f32::INFINITY,
        ];
        for pair in values.windows(2) {
            assert!(
                ordered_f32(pair[0]) <= ordered_f32(pair[1]),
                "{} {}",
                pair[0],
                pair[1]
            );
        }
        assert!(ordered_f32(-0.0) < ordered_f32(0.0));
    }
}
