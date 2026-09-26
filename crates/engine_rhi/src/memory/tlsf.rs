//! Two-level segregated fit (TLSF) sub-allocator for ranges of GPU memory.
//!
//! Masmano, Ripoll, Crespo, Real: "TLSF: a New Dynamic Memory Allocator for Real-Time
//! Systems" (ECRTS 2004); the variant AMD's VMA 3 uses by default. Free blocks are kept
//! in size-class lists indexed by two levels (power of two, then 32 linear steps inside
//! it); two bitmaps find the first non-empty list that is large enough with two bit scans,
//! so allocation and free are O(1). Adjacent free blocks are always merged.
//!
//! The allocator manages offsets only; it never touches the memory. Nodes live in a
//! vector and refer to each other by index; freed nodes are recycled, so once the node
//! vector has grown to the working set, allocating and freeing touch no heap
//! (CLAUDE.md §5).

/// Allocation granularity: sizes are rounded up to it and every offset is a multiple of
/// it. 256 bytes covers every buffer alignment Vulkan drivers report
/// (`minStorageBufferOffsetAlignment` ≤ 256) and `nonCoherentAtomSize`.
pub(crate) const GRANULE: u64 = 256;
const GRANULE_LOG2: u32 = GRANULE.trailing_zeros();
/// Second level: 2^5 = 32 lists per power of two, as recommended by the paper.
const SL_LOG2: u32 = 5;
const SL_COUNT: usize = 1 << SL_LOG2;
const NONE: u32 = u32::MAX;
/// Largest manageable range: sizes in granules must fit the first-level bitmap.
pub(crate) const MAX_CAPACITY: u64 = 1 << 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Free,
    Used,
    /// Merged into a neighbour; on the spare list.
    Dead,
}

#[derive(Debug, Clone, Copy)]
struct Node {
    offset: u64,
    size: u64,
    prev_phys: u32,
    next_phys: u32,
    prev_free: u32,
    next_free: u32,
    state: State,
}

/// A range handed out by [`Tlsf::allocate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TlsfAllocation {
    /// Offset from the start of the managed range, aligned as requested.
    pub(crate) offset: u64,
    /// Size reserved (the request rounded up to [`GRANULE`]).
    pub(crate) size: u64,
    node: u32,
}

/// TLSF over `[0, capacity)`.
#[derive(Debug)]
pub(crate) struct Tlsf {
    capacity: u64,
    used: u64,
    allocations: u32,
    nodes: Vec<Node>,
    spare: Vec<u32>,
    fl_count: usize,
    fl_bitmap: u32,
    sl_bitmap: Vec<u32>,
    heads: Vec<u32>,
}

/// List of a block of `units` granules: the one whose blocks it belongs to.
fn mapping(units: u64) -> (usize, usize) {
    if units < SL_COUNT as u64 {
        (0, units as usize)
    } else {
        let msb = 63 - units.leading_zeros();
        let fl = (msb - SL_LOG2 + 1) as usize;
        let sl = ((units >> (msb - SL_LOG2)) as usize) ^ SL_COUNT;
        (fl, sl)
    }
}

/// First list whose every block holds at least `units` granules.
fn mapping_search(units: u64) -> (usize, usize) {
    if units < SL_COUNT as u64 {
        // First-level 0 lists are exact: every block in list `u` has `u` granules.
        mapping(units)
    } else {
        let msb = 63 - units.leading_zeros();
        mapping(units + (1 << (msb - SL_LOG2)) - 1)
    }
}

impl Tlsf {
    /// Manages `[0, capacity)`. `capacity` is rounded down to [`GRANULE`] and must be
    /// below [`MAX_CAPACITY`].
    pub(crate) fn new(capacity: u64) -> Self {
        let capacity = capacity.min(MAX_CAPACITY - GRANULE) & !(GRANULE - 1);
        let units = (capacity >> GRANULE_LOG2).max(1);
        let fl_count = mapping(units).0 + 1;
        let mut tlsf = Self {
            capacity,
            used: 0,
            allocations: 0,
            nodes: Vec::with_capacity(64),
            spare: Vec::with_capacity(64),
            fl_count,
            fl_bitmap: 0,
            sl_bitmap: vec![0; fl_count],
            heads: vec![NONE; fl_count * SL_COUNT],
        };
        if capacity > 0 {
            let root = tlsf.new_node(Node {
                offset: 0,
                size: capacity,
                prev_phys: NONE,
                next_phys: NONE,
                prev_free: NONE,
                next_free: NONE,
                state: State::Free,
            });
            tlsf.insert_free(root);
        }
        tlsf
    }

    /// Size of the managed range.
    pub(crate) fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Bytes currently allocated (rounded sizes, excluding alignment padding).
    pub(crate) fn used(&self) -> u64 {
        self.used
    }

    /// Number of live allocations.
    pub(crate) fn allocation_count(&self) -> u32 {
        self.allocations
    }

    /// Whether nothing is allocated.
    pub(crate) fn is_empty(&self) -> bool {
        self.allocations == 0
    }

    /// Reserves `size` bytes aligned to `align` (a power of two; values below
    /// [`GRANULE`] are raised to it). `None` when no free block fits.
    pub(crate) fn allocate(&mut self, size: u64, align: u64) -> Option<TlsfAllocation> {
        self.allocate_with(size, align, true)
    }

    /// [`Self::allocate`] restricted to the O(1) path: only size classes whose every
    /// block fits are considered. May fail where [`Self::allocate`] succeeds (a block in a
    /// smaller class could still fit), so callers with several ranges try this on all of
    /// them first.
    pub(crate) fn allocate_fast(&mut self, size: u64, align: u64) -> Option<TlsfAllocation> {
        self.allocate_with(size, align, false)
    }

    fn allocate_with(&mut self, size: u64, align: u64, thorough: bool) -> Option<TlsfAllocation> {
        debug_assert!(
            align.is_power_of_two(),
            "alignment {align} is not a power of two"
        );
        let align = align.max(GRANULE);
        let size = size.max(1).checked_next_multiple_of(GRANULE)?;
        if size > self.capacity - self.used || align > self.capacity {
            return None;
        }
        let units = size >> GRANULE_LOG2;
        // Enough for any start offset: the worst case needs `align - GRANULE` of padding.
        let padded = units + ((align >> GRANULE_LOG2) - 1);
        let (fl, sl) = mapping_search(padded);
        let node = match self.first_list_from(fl, sl) {
            Some(list) => self.heads[list],
            None if thorough => self.search_partial_fits(units, padded, size, align)?,
            None => return None,
        };
        Some(self.take(node, size, align))
    }

    /// Returns an allocation. It must come from this allocator and not be freed twice;
    /// violations are ignored in release builds and panic in debug builds.
    pub(crate) fn free(&mut self, allocation: TlsfAllocation) {
        let index = allocation.node;
        let valid = self.nodes.get(index as usize).is_some_and(|n| {
            n.state == State::Used && n.offset == allocation.offset && n.size == allocation.size
        });
        debug_assert!(valid, "invalid or double free: {allocation:?}");
        if !valid {
            return;
        }
        self.used -= allocation.size;
        self.allocations -= 1;
        let mut index = index;
        self.nodes[index as usize].state = State::Free;
        let next = self.nodes[index as usize].next_phys;
        if next != NONE && self.nodes[next as usize].state == State::Free {
            self.remove_free(next);
            self.absorb(index, next);
        }
        let prev = self.nodes[index as usize].prev_phys;
        if prev != NONE && self.nodes[prev as usize].state == State::Free {
            self.remove_free(prev);
            self.absorb(prev, index);
            index = prev;
        }
        self.insert_free(index);
    }

    /// Index into `heads` of the first non-empty list at or after `(fl, sl)`.
    fn first_list_from(&self, fl: usize, sl: usize) -> Option<usize> {
        if fl >= self.fl_count {
            return None;
        }
        let sl_map = self.sl_bitmap[fl] & (u32::MAX << sl);
        if sl_map != 0 {
            return Some(fl * SL_COUNT + sl_map.trailing_zeros() as usize);
        }
        let fl_map = self.fl_bitmap & u32::MAX.checked_shl(fl as u32 + 1).unwrap_or(0);
        if fl_map == 0 {
            return None;
        }
        let fl = fl_map.trailing_zeros() as usize;
        Some(fl * SL_COUNT + self.sl_bitmap[fl].trailing_zeros() as usize)
    }

    /// Slow path when no list guarantees a fit: blocks in the lists between the exact
    /// size and the guaranteed one may still fit, depending on their size and alignment.
    fn search_partial_fits(&self, units: u64, padded: u64, size: u64, align: u64) -> Option<u32> {
        let (first_fl, first_sl) = mapping(units);
        let (last_fl, last_sl) = mapping_search(padded);
        let first = first_fl * SL_COUNT + first_sl;
        let last = (last_fl * SL_COUNT + last_sl).min(self.heads.len());
        for list in first..last {
            let mut node = self.heads[list];
            while node != NONE {
                let n = &self.nodes[node as usize];
                let pad = n.offset.next_multiple_of(align) - n.offset;
                if pad + size <= n.size {
                    return Some(node);
                }
                node = n.next_free;
            }
        }
        None
    }

    /// Carves `size` bytes at an `align`ed offset out of free block `index`.
    fn take(&mut self, index: u32, size: u64, align: u64) -> TlsfAllocation {
        self.remove_free(index);
        let mut index = index;
        let offset = self.nodes[index as usize].offset;
        let pad = offset.next_multiple_of(align) - offset;
        if pad > 0 {
            // The padding stays free. Its left neighbour is in use (free neighbours are
            // always merged), so it needs no merging.
            let rest = self.split(index, pad);
            self.insert_free(index);
            index = rest;
        }
        if self.nodes[index as usize].size > size {
            // Likewise the tail's right neighbour is in use.
            let tail = self.split(index, size);
            self.insert_free(tail);
        }
        let node = &mut self.nodes[index as usize];
        node.state = State::Used;
        self.used += size;
        self.allocations += 1;
        TlsfAllocation {
            offset: node.offset,
            size,
            node: index,
        }
    }

    /// Splits block `index` after `left_size` bytes; returns the new right block (state
    /// copied from the left, not on any free list).
    fn split(&mut self, index: u32, left_size: u64) -> u32 {
        let left = self.nodes[index as usize];
        let right = self.new_node(Node {
            offset: left.offset + left_size,
            size: left.size - left_size,
            prev_phys: index,
            next_phys: left.next_phys,
            prev_free: NONE,
            next_free: NONE,
            state: left.state,
        });
        if left.next_phys != NONE {
            self.nodes[left.next_phys as usize].prev_phys = right;
        }
        let left = &mut self.nodes[index as usize];
        left.size = left_size;
        left.next_phys = right;
        right
    }

    /// Merges block `right` into its physical predecessor `left` and recycles `right`.
    fn absorb(&mut self, left: u32, right: u32) {
        let r = self.nodes[right as usize];
        let l = &mut self.nodes[left as usize];
        l.size += r.size;
        l.next_phys = r.next_phys;
        if r.next_phys != NONE {
            self.nodes[r.next_phys as usize].prev_phys = left;
        }
        self.nodes[right as usize].state = State::Dead;
        self.spare.push(right);
    }

    fn new_node(&mut self, node: Node) -> u32 {
        match self.spare.pop() {
            Some(index) => {
                self.nodes[index as usize] = node;
                index
            }
            None => {
                self.nodes.push(node);
                (self.nodes.len() - 1) as u32
            }
        }
    }

    fn list_of(&self, index: u32) -> usize {
        let (fl, sl) = mapping(self.nodes[index as usize].size >> GRANULE_LOG2);
        fl * SL_COUNT + sl
    }

    fn insert_free(&mut self, index: u32) {
        let list = self.list_of(index);
        let head = self.heads[list];
        {
            let node = &mut self.nodes[index as usize];
            node.state = State::Free;
            node.prev_free = NONE;
            node.next_free = head;
        }
        if head != NONE {
            self.nodes[head as usize].prev_free = index;
        }
        self.heads[list] = index;
        let (fl, sl) = (list / SL_COUNT, list % SL_COUNT);
        self.sl_bitmap[fl] |= 1 << sl;
        self.fl_bitmap |= 1 << fl;
    }

    fn remove_free(&mut self, index: u32) {
        let list = self.list_of(index);
        let Node {
            prev_free,
            next_free,
            ..
        } = self.nodes[index as usize];
        if prev_free != NONE {
            self.nodes[prev_free as usize].next_free = next_free;
        } else {
            self.heads[list] = next_free;
        }
        if next_free != NONE {
            self.nodes[next_free as usize].prev_free = prev_free;
        }
        if self.heads[list] == NONE {
            let (fl, sl) = (list / SL_COUNT, list % SL_COUNT);
            self.sl_bitmap[fl] &= !(1 << sl);
            if self.sl_bitmap[fl] == 0 {
                self.fl_bitmap &= !(1 << fl);
            }
        }
    }

    /// Checks every invariant; for tests.
    #[cfg(test)]
    fn validate(&self) {
        // The physical chain starts at node 0 (offset 0) and tiles the whole range.
        let mut index = if self.capacity > 0 { 0 } else { NONE };
        let (mut end, mut used, mut count, mut prev, mut prev_free) = (0, 0, 0, NONE, false);
        while index != NONE {
            let n = self.nodes[index as usize];
            assert_eq!(n.offset, end, "gap or overlap at node {index}");
            assert_eq!(n.prev_phys, prev);
            assert!(n.size > 0 && n.size.is_multiple_of(GRANULE));
            match n.state {
                State::Free => {
                    assert!(!prev_free, "adjacent free blocks at {}", n.offset);
                    let list = self.list_of(index);
                    let mut cursor = self.heads[list];
                    while cursor != NONE && cursor != index {
                        cursor = self.nodes[cursor as usize].next_free;
                    }
                    assert_eq!(cursor, index, "free block not on its list");
                }
                State::Used => {
                    used += n.size;
                    count += 1;
                }
                State::Dead => panic!("dead node {index} in the physical chain"),
            }
            prev_free = n.state == State::Free;
            end += n.size;
            prev = index;
            index = n.next_phys;
        }
        assert_eq!(end, self.capacity);
        assert_eq!(used, self.used);
        assert_eq!(count, self.allocations);
        for (list, &head) in self.heads.iter().enumerate() {
            let (fl, sl) = (list / SL_COUNT, list % SL_COUNT);
            assert_eq!(head != NONE, self.sl_bitmap[fl] & (1 << sl) != 0);
            assert_eq!(self.sl_bitmap[fl] != 0, self.fl_bitmap & (1 << fl) != 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Small deterministic generator (xorshift64*), so failures reproduce.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn size_classes_are_monotonic_and_search_rounds_up() {
        let mut last = (0, 0);
        for units in 1..100_000u64 {
            let class = mapping(units);
            assert!(class >= last, "{units}");
            last = class;
            // Every block in the search list or later is at least `units` long: the
            // smallest block in list (fl, sl) must be >= units.
            let (fl, sl) = mapping_search(units);
            let smallest = if fl == 0 {
                sl as u64
            } else {
                let base = 1u64 << (fl as u32 + SL_LOG2 - 1);
                base + sl as u64 * (base >> SL_LOG2)
            };
            assert!(
                smallest >= units,
                "units {units}: list ({fl},{sl}) starts at {smallest}"
            );
        }
    }

    #[test]
    fn allocates_aligned_ranges_and_coalesces_on_free() {
        let mut tlsf = Tlsf::new(1 << 20);
        let a = tlsf.allocate(1000, 256).unwrap();
        let b = tlsf.allocate(5000, 4096).unwrap();
        let c = tlsf.allocate(100, 65536).unwrap();
        tlsf.validate();
        for x in [a, b, c] {
            assert_eq!(x.offset % 256, 0);
        }
        assert_eq!(b.offset % 4096, 0);
        assert_eq!(c.offset % 65536, 0);
        assert_eq!(a.size, 1024);
        assert_eq!(tlsf.used(), 1024 + 5120 + 256);
        tlsf.free(b);
        tlsf.validate();
        tlsf.free(a);
        tlsf.validate();
        tlsf.free(c);
        tlsf.validate();
        assert!(tlsf.is_empty());
        // Everything merged back: the full range is available again.
        let all = tlsf.allocate(1 << 20, 256).unwrap();
        assert_eq!((all.offset, all.size), (0, 1 << 20));
    }

    #[test]
    fn exhaustion_and_exact_fill() {
        let mut tlsf = Tlsf::new(64 * 1024);
        let blocks: Vec<_> = (0..256).map(|_| tlsf.allocate(256, 256).unwrap()).collect();
        assert!(tlsf.allocate(1, 1).is_none());
        tlsf.validate();
        // Free every other block: 128 holes of 256 bytes, none can hold 512.
        for b in blocks.iter().step_by(2) {
            tlsf.free(*b);
        }
        tlsf.validate();
        assert!(tlsf.allocate(512, 256).is_none());
        assert!(tlsf.allocate(256, 256).is_some());
    }

    #[test]
    fn partial_fit_search_finds_blocks_the_fast_path_skips() {
        // One free block of exactly 3 KiB left: a 3 KiB request with 1 KiB alignment
        // needs the slow path (the guaranteed list would require 3 KiB + 768 B).
        let mut tlsf = Tlsf::new(8 * 1024);
        let a = tlsf.allocate(3 * 1024, 1024).unwrap();
        let _b = tlsf.allocate(5 * 1024, 256).unwrap();
        tlsf.free(a);
        let again = tlsf.allocate(3 * 1024, 1024).unwrap();
        assert_eq!(again.offset, 0);
        tlsf.validate();
    }

    #[test]
    fn random_operations_match_a_model() {
        const CAPACITY: u64 = 64 << 20;
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut tlsf = Tlsf::new(CAPACITY);
        let mut live: Vec<TlsfAllocation> = Vec::new();
        let mut model: BTreeMap<u64, u64> = BTreeMap::new();
        let aligns = [256u64, 1024, 4096, 65536];
        let (mut failures, mut successes) = (0, 0);
        for step in 0..200_000u32 {
            let allocate = live.is_empty() || rng.below(100) < 55;
            if allocate {
                // Log-uniform sizes from 1 byte to 1 MiB.
                let size = 1 + (rng.next() >> (rng.below(44) + 20)) % (1 << 20);
                let align = aligns[rng.below(4) as usize];
                // As the pool allocator uses it: the O(1) path, then the thorough one.
                let fast = step % 2 == 0;
                let result = if fast {
                    tlsf.allocate_fast(size, align)
                        .or_else(|| tlsf.allocate(size, align))
                } else {
                    tlsf.allocate(size, align)
                };
                match result {
                    Some(a) => {
                        assert_eq!(a.offset % align, 0);
                        assert!(a.size >= size && a.offset + a.size <= CAPACITY);
                        if let Some((&o, &s)) = model.range(..=a.offset).next_back() {
                            assert!(o + s <= a.offset, "overlaps the block before");
                        }
                        if let Some((&o, _)) = model.range(a.offset..).next() {
                            assert!(a.offset + a.size <= o, "overlaps the block after");
                        }
                        model.insert(a.offset, a.size);
                        live.push(a);
                        successes += 1;
                    }
                    None => failures += 1,
                }
            } else {
                let a = live.swap_remove(rng.below(live.len() as u64) as usize);
                assert_eq!(model.remove(&a.offset), Some(a.size));
                tlsf.free(a);
            }
            if step % 997 == 0 {
                tlsf.validate();
            }
        }
        tlsf.validate();
        assert!(
            successes > 90_000,
            "{successes} successes, {failures} failures"
        );
        for a in live.drain(..) {
            tlsf.free(a);
        }
        tlsf.validate();
        assert!(tlsf.is_empty());
        assert_eq!(tlsf.allocate(CAPACITY, 256).map(|a| a.size), Some(CAPACITY));
    }

    #[test]
    fn huge_ranges_and_requests_do_not_overflow() {
        let mut tlsf = Tlsf::new(u64::MAX);
        assert_eq!(tlsf.capacity(), MAX_CAPACITY - GRANULE);
        assert!(tlsf.allocate(u64::MAX, 256).is_none());
        assert!(tlsf.allocate(1 << 39, 1 << 39).is_some());
        tlsf.validate();
        let mut empty = Tlsf::new(100);
        assert!(empty.allocate(1, 1).is_none());
    }
}
