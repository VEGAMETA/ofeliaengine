//! CPU cost of text: layout of editor labels and of a paragraph (the same inputs as
//! `bench/text-compare`, where Parley and cosmic-text are measured), glyph rasterization,
//! and atlas lookups. Prints Markdown.
//!
//! ```text
//! cargo bench -p engine_text --bench text
//! ```

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use engine_text::{Fonts, GlyphAtlas, GlyphBitmap, GlyphKey, Rasterizer, TextLayout, TextStyle};
use std::hint::black_box;
use std::time::Instant;

fn median(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut samples: Vec<f64> = (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn labels() -> Vec<String> {
    let words = [
        "Transform",
        "Position",
        "Rotation",
        "Scale",
        "Visible",
        "Layer",
        "Material",
        "Метка",
        "Сцена",
        "Объект",
        "Скорость",
        "Гравитация",
        "Physics",
        "Audio",
        "Bus",
        "Volume",
        "Pitch",
        "Priority",
        "Inspector",
        "Hierarchy",
        "Console",
        "Profiler",
    ];
    (0..1000)
        .map(|i| {
            let n = 1 + i % 4;
            (0..n)
                .map(|k| words[(i * 7 + k * 13) % words.len()])
                .collect::<Vec<_>>()
                .join(" ")
                + &format!(" {i}")
        })
        .collect()
}

fn paragraph() -> String {
    let en = "The quick brown fox jumps over the lazy dog while the editor keeps every frame \
              under a millisecond, because layout work is cached and only redone when the \
              text or its width changes. ";
    let ru = "Съешь же ещё этих мягких французских булок, да выпей чаю, пока редактор держит \
              каждый кадр быстрым и перестраивает текст только при изменении ширины. ";
    let mut s = String::new();
    while s.len() < 10_000 {
        s.push_str(en);
        s.push_str(ru);
    }
    s
}

fn main() {
    let mut fonts = Fonts::new();
    let go = fonts.load(engine_text::fonts::GO_REGULAR, 0).unwrap();
    let labels = labels();
    let text = paragraph();
    let mut layout = TextLayout::new();
    let style = TextStyle {
        size: 14.0,
        ..TextStyle::default()
    };
    let wrapped = TextStyle {
        max_width: Some(600.0),
        ..style
    };

    println!("### engine_text CPU cost (µs, median)\n");
    println!("Go Regular 14 px. Inputs as in `bench/text-compare` (Parley / cosmic-text).\n");
    println!("| case | µs | per item |");
    println!("|---|---:|---:|");
    let t = median(31, || {
        for l in &labels {
            layout.layout(&mut fonts, &[go], l, &style);
            black_box(layout.glyphs().len());
        }
    });
    println!(
        "| 1,000 labels, shape + lay out | {t:.0} | {:.2} µs per label |",
        t / 1000.0
    );
    let t = median(31, || {
        layout.layout(&mut fonts, &[go], &text, &wrapped);
        black_box(layout.lines().len());
    });
    println!(
        "| 10 KB paragraph, shape + wrap at 600 px ({} lines) | {t:.0} | {:.1} ns per byte |",
        layout.lines().len(),
        t * 1000.0 / text.len() as f64
    );

    // Rasterize every glyph of the paragraph once (cold), at 14 px.
    layout.layout(&mut fonts, &[go], &text, &wrapped);
    let mut glyphs: Vec<u16> = layout.glyphs().iter().map(|g| g.glyph).collect();
    glyphs.sort_unstable();
    glyphs.dedup();
    let font = fonts.get(go).unwrap();
    let mut raster = Rasterizer::new();
    let mut bitmap = GlyphBitmap::default();
    let t = median(31, || {
        for &g in &glyphs {
            black_box(engine_text::rasterize(
                font,
                g,
                14.0,
                0.25,
                &mut raster,
                &mut bitmap,
            ));
        }
    });
    println!(
        "| rasterize {} distinct glyphs at 14 px | {t:.0} | {:.2} µs per glyph |",
        glyphs.len(),
        t / glyphs.len() as f64
    );
    let t = median(31, || {
        for &g in &glyphs {
            black_box(engine_text::rasterize(
                font,
                g,
                48.0,
                0.25,
                &mut raster,
                &mut bitmap,
            ));
        }
    });
    println!(
        "| rasterize {} distinct glyphs at 48 px | {t:.0} | {:.2} µs per glyph |",
        glyphs.len(),
        t / glyphs.len() as f64
    );

    // Atlas lookups for a laid-out paragraph once everything is cached.
    let mut atlas = GlyphAtlas::new(1024, 1024);
    let lookup = |atlas: &mut GlyphAtlas, layout: &TextLayout, fonts: &Fonts| {
        let mut n = 0u32;
        for g in layout.glyphs() {
            let (key, _) = GlyphKey::at(g.font, g.glyph, 14.0, g.x);
            n += u32::from(atlas.get(fonts, key).is_some());
        }
        n
    };
    lookup(&mut atlas, &layout, &fonts);
    let n = layout.glyphs().len();
    let t = median(31, || {
        black_box(lookup(&mut atlas, &layout, &fonts));
    });
    println!(
        "| atlas lookups for the paragraph ({n} glyphs, all cached) | {t:.0} | {:.1} ns per glyph |",
        t * 1000.0 / n as f64
    );
    println!();
}
