//! CLAUDE.md §5: laying out and drawing text every frame must not allocate once the layout
//! buffers and the glyph atlas are warm.

#![allow(clippy::unwrap_used)]

use engine_text::{Align, Fonts, GlyphAtlas, GlyphKey, TextLayout, TextStyle};
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

#[test]
fn steady_state_text_does_not_touch_the_heap() {
    let mut fonts = Fonts::new();
    let go = fonts.load(engine_text::fonts::GO_REGULAR, 0).unwrap();
    let mono = fonts.load(engine_text::fonts::GO_MONO, 0).unwrap();
    let arabic = fonts.load(engine_text::fonts::NOTO_SANS_ARABIC, 0).unwrap();
    let hebrew = fonts.load(engine_text::fonts::NOTO_SANS_HEBREW, 0).unwrap();
    let mut layout = TextLayout::new();
    let mut atlas = GlyphAtlas::new(1024, 1024);
    let texts = [
        "Frame 16.6 ms · GPU 4.2 ms · 144 fps",
        "Инспектор: Позиция, Поворот, Масштаб",
        "The quick brown fox jumps over the lazy dog.\nSecond line, wrapped when narrow.",
        // Right to left, with numbers and Latin inside (UAX #9 levels and reordering).
        "لديك ٣ أرواح و 250 نقطة ذهب (Gold). مرحبا بك في اللعبة!",
        "שלום world 42, עולם (ok)",
    ];
    let mut frame = |n: usize| {
        for (i, text) in texts.iter().enumerate() {
            let style = TextStyle {
                size: [13.0, 16.0][n % 2],
                max_width: Some([160.0, 300.0][(n + i) % 2]),
                align: [Align::Start, Align::Center, Align::End][n % 3],
                ..TextStyle::default()
            };
            let stack = match i {
                0 => [mono, go],
                3 => [arabic, go],
                4 => [hebrew, go],
                _ => [go, mono],
            };
            layout.layout(&mut fonts, &stack, text, &style);
            for g in layout.glyphs() {
                let (key, _) = GlyphKey::at(g.font, g.glyph, style.size, g.x);
                std::hint::black_box(atlas.get(&fonts, key));
            }
        }
        let _ = atlas.take_dirty();
    };
    for n in 0..60 {
        frame(n);
    }
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for n in 60..600 {
        frame(n);
    }
    let allocated = ALLOCATIONS.load(Ordering::Relaxed) - before;
    println!("540 frames of layout + atlas: {allocated} allocations");
    assert_eq!(allocated, 0);
}
