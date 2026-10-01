//! Memory aliasing of transient resources: resources whose lifetimes (pass ranges) do
//! not overlap may share memory. Offsets are assigned greedily, largest first, at the
//! lowest aligned offset that is free for the whole lifetime (the classic interval
//! "register allocation" heuristic Frostbite and Unreal describe).

/// One transient resource to place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) size: u64,
    pub(crate) align: u64,
    /// First and last pass that use it (inclusive).
    pub(crate) first: u32,
    pub(crate) last: u32,
    /// Output: offset in the heap.
    pub(crate) offset: u64,
}

impl Item {
    fn lifetime_overlaps(&self, other: &Self) -> bool {
        if std::env::var("ENGINE_DEBUG_NO_ALIAS").is_ok() {
            return true;
        }
        self.first <= other.last && other.first <= self.last
    }

    pub(crate) fn memory_overlaps(&self, other: &Self) -> bool {
        self.offset < other.offset + other.size && other.offset < self.offset + self.size
    }
}

/// Reusable scratch space for [`place`] (capacities survive across frames).
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    order: Vec<u32>,
    /// `(offset, end)` of placed items whose lifetimes overlap the current one.
    conflicts: Vec<(u64, u64)>,
}

/// Assigns offsets to `items` and returns the heap size needed. O(n² log n).
pub(crate) fn place(items: &mut [Item], scratch: &mut Scratch) -> u64 {
    let Scratch { order, conflicts } = scratch;
    order.clear();
    order.extend(0..items.len() as u32);
    // Largest first; ties by start. `sort_unstable` does not allocate.
    order.sort_unstable_by(|&a, &b| {
        let (a, b) = (&items[a as usize], &items[b as usize]);
        b.size.cmp(&a.size).then(a.first.cmp(&b.first))
    });
    let mut heap = 0;
    for placed_count in 0..order.len() {
        let index = order[placed_count] as usize;
        let item = items[index];
        conflicts.clear();
        conflicts.extend(order[..placed_count].iter().filter_map(|&other| {
            let other = &items[other as usize];
            other
                .lifetime_overlaps(&item)
                .then_some((other.offset, other.offset + other.size))
        }));
        conflicts.sort_unstable();
        // Sweep the conflicts in address order for the first gap that fits.
        let align = item.align.max(1);
        let mut offset = 0u64;
        for &(start, end) in conflicts.iter() {
            if offset + item.size <= start {
                break;
            }
            offset = offset.max(end.next_multiple_of(align));
        }
        items[index].offset = offset;
        heap = heap.max(offset + item.size);
    }
    heap
}

/// Resources whose memory `items[index]` reuses and that it must wait for, into `out`
/// (item indices). Only the most recent occupants that together cover its memory are
/// kept: an older occupant of a covered range was itself waited for by a newer one, and
/// barriers chain.
pub(crate) fn predecessors(
    items: &[Item],
    index: usize,
    out: &mut Vec<u32>,
    scratch: &mut Scratch,
) {
    let Scratch { order, conflicts } = scratch;
    let item = items[index];
    let (start, end) = (item.offset, item.offset + item.size);
    order.clear();
    order.extend((0..items.len() as u32).filter(|&j| {
        let other = &items[j as usize];
        other.last < item.first && item.memory_overlaps(other)
    }));
    // Most recent first.
    order.sort_unstable_by(|&a, &b| items[b as usize].last.cmp(&items[a as usize].last));
    // `conflicts` holds the covered part of [start, end) as sorted, merged intervals.
    conflicts.clear();
    for &j in order.iter() {
        let other = &items[j as usize];
        let overlap = (start.max(other.offset), end.min(other.offset + other.size));
        let covered = conflicts
            .iter()
            .any(|&(s, e)| s <= overlap.0 && overlap.1 <= e);
        if covered {
            continue;
        }
        out.push(j);
        conflicts.push(overlap);
        conflicts.sort_unstable();
        // Merge touching or overlapping intervals.
        let mut merged = 0;
        for k in 1..conflicts.len() {
            if conflicts[k].0 <= conflicts[merged].1 {
                conflicts[merged].1 = conflicts[merged].1.max(conflicts[k].1);
            } else {
                merged += 1;
                conflicts[merged] = conflicts[k];
            }
        }
        conflicts.truncate(merged + 1);
        if conflicts.len() == 1 && conflicts[0] == (start, end) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(size: u64, first: u32, last: u32) -> Item {
        Item {
            size,
            align: 256,
            first,
            last,
            offset: 0,
        }
    }

    #[test]
    fn disjoint_lifetimes_share_memory() {
        let mut items = [item(1024, 0, 1), item(1024, 2, 3), item(512, 4, 4)];
        let heap = place(&mut items, &mut Scratch::default());
        assert_eq!(heap, 1024);
        assert!(items.iter().all(|i| i.offset == 0));
    }

    #[test]
    fn overlapping_lifetimes_do_not() {
        let mut items = [item(1024, 0, 2), item(1024, 1, 3), item(2048, 2, 2)];
        let heap = place(&mut items, &mut Scratch::default());
        assert_eq!(heap, 4096);
        for (i, a) in items.iter().enumerate() {
            for b in &items[i + 1..] {
                assert!(!(a.lifetime_overlaps(b) && a.memory_overlaps(b)));
            }
        }
    }

    #[test]
    fn alignment_is_respected() {
        let mut items = [
            item(300, 0, 5),
            Item {
                align: 65536,
                ..item(1000, 0, 5)
            },
        ];
        place(&mut items, &mut Scratch::default());
        assert_eq!(items[1].offset % 65536, 0);
        assert!(!items[0].memory_overlaps(&items[1]));
    }

    #[test]
    fn random_graphs_never_alias_live_resources_and_stay_near_the_lower_bound() {
        let mut seed = 0x1234_5678_9ABC_DEF1u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut scratch = Scratch::default();
        let mut worst = 0.0f64;
        for _ in 0..300 {
            let passes = 1 + (next() % 40) as u32;
            let count = 1 + (next() % 60) as usize;
            let mut items: Vec<Item> = (0..count)
                .map(|_| {
                    let first = (next() % u64::from(passes)) as u32;
                    let last = first + (next() % u64::from(passes - first)) as u32;
                    Item {
                        size: 256 * (1 + next() % 4096),
                        align: 256 << (next() % 9),
                        first,
                        last,
                        offset: 0,
                    }
                })
                .collect();
            let heap = place(&mut items, &mut scratch);
            for (i, a) in items.iter().enumerate() {
                assert_eq!(a.offset % a.align, 0);
                assert!(a.offset + a.size <= heap);
                for b in &items[i + 1..] {
                    assert!(
                        !(a.lifetime_overlaps(b) && a.memory_overlaps(b)),
                        "{a:?} and {b:?} alias while both alive"
                    );
                }
            }
            // Lower bound: the most bytes alive at any one pass.
            let peak = (0..passes)
                .map(|p| {
                    items
                        .iter()
                        .filter(|i| i.first <= p && p <= i.last)
                        .map(|i| i.size)
                        .sum::<u64>()
                })
                .max()
                .unwrap_or(0);
            let total: u64 = items.iter().map(|i| i.size).sum();
            assert!(heap <= total + items.iter().map(|i| i.align).sum::<u64>());
            worst = worst.max(heap as f64 / peak as f64);
        }
        // Greedy interval packing is not optimal, but it should stay close.
        assert!(worst < 2.0, "worst heap / peak live = {worst}");
    }

    #[test]
    fn predecessors_keep_only_the_latest_covering_occupants() {
        // Three generations in the same memory, then one resource spanning two halves
        // whose last occupants differ.
        let mut items = [
            Item {
                offset: 0,
                ..item(1024, 0, 0)
            },
            Item {
                offset: 0,
                ..item(1024, 1, 1)
            },
            Item {
                offset: 0,
                ..item(512, 2, 2)
            },
            Item {
                offset: 0,
                ..item(1024, 3, 3)
            },
        ];
        let mut out = Vec::new();
        let mut scratch = Scratch::default();
        predecessors(&items, 1, &mut out, &mut scratch);
        assert_eq!(out, [0]);
        out.clear();
        // Item 3 covers [0, 1024): [0, 512) was last used by 2, [512, 1024) by 1.
        predecessors(&items, 3, &mut out, &mut scratch);
        assert_eq!(out, [2, 1]);
        out.clear();
        items[2].size = 1024;
        predecessors(&items, 3, &mut out, &mut scratch);
        assert_eq!(out, [2]);
    }
}
