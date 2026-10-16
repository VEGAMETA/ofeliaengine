//! CLAUDE.md §5: zero heap allocations per frame. A HUD formats its counters every frame:
//! plurals, grouped numbers, fractions, terms and fallbacks into a string that has grown,
//! without one allocation. One test per file, so nothing else allocates concurrently.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_loc::{Args, Localizer, compile, loc};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const EN: &str = "\
items = { $count ->
    [one] { $count } item
   *[other] { $count } items
}
time = { NUMBER($seconds, minimumFractionDigits: 1, maximumFractionDigits: 1) } s
-brand = Ofelia
title = { -brand }: { $level }
only-en = Ammo: { $ammo }
";

const RU: &str = "\
items = { $count ->
    [one] { $count } предмет
    [few] { $count } предмета
   *[many] { $count } предметов
}
time = { NUMBER($seconds, minimumFractionDigits: 1, maximumFractionDigits: 1) } с
-brand = { $case ->
   *[nominative] Офелия
    [genitive] Офелии
}
title = Уровень { $level } { -brand(case: \"genitive\") }
";

#[test]
fn formatting_a_hud_every_frame_allocates_nothing() {
    let (en, _) = compile("en", &[("hud.ftl", EN)]);
    let (ru, _) = compile("ru", &[("hud.ftl", RU)]);
    let mut loc = Localizer::new(vec![en, ru], "en").unwrap();
    loc.set_locale(&["ru-RU"]);
    let mut text = String::with_capacity(256);
    let frame = |n: i64, text: &mut String| {
        text.clear();
        loc.format(loc!("items"), &Args::new().with("count", n % 30), text);
        loc.format(
            loc!("time"),
            &Args::new().with("seconds", n as f64 * 0.016),
            text,
        );
        loc.format(loc!("title"), &Args::new().with("level", n % 7), text);
        // Not in Russian: the English fallback, a grouped number.
        loc.format(loc!("only-en"), &Args::new().with("ammo", 12_345 + n), text);
    };
    // Warm up: formatters made, the string grown.
    for n in 0..100 {
        frame(n, &mut text);
    }
    println!("{text}");
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for n in 0..10_000 {
        frame(n, &mut text);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert_eq!(
        allocations, 0,
        "{allocations} allocations over 10,000 frames"
    );
}
