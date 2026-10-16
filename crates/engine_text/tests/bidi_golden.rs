//! A golden image of text in both directions (ADR 0102): Arabic with numbers and Latin
//! inside, wrapped and aligned right; Hebrew inside Latin; Hebrew with Latin; Latin forced
//! right to left. Laid out and rasterized on the CPU (the GPU draws the same coverage:
//! `engine_render2d`'s text test), compared with `tests/data/bidi.png`;
//! `ENGINE_UPDATE_GOLDEN=1` writes it anew.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use engine_text::{
    Align, FontId, Fonts, GlyphAtlas, GlyphKey, TextDirection, TextLayout, TextStyle,
};
use std::path::PathBuf;

const W: u32 = 560;
const H: u32 = 330;
const SIZE: f32 = 22.0;

/// The layout's glyphs composited white over `image` at `origin`.
fn draw(
    image: &mut [u8],
    layout: &TextLayout,
    fonts: &Fonts,
    atlas: &mut GlyphAtlas,
    origin: (f32, f32),
) {
    let (aw, _) = atlas.size();
    for g in layout.glyphs() {
        let (key, x) = GlyphKey::at(g.font, g.glyph, SIZE, origin.0 + g.x);
        let Some(a) = atlas.get(fonts, key) else {
            continue;
        };
        let left = x + i32::from(a.left);
        let top = (origin.1 + g.y).round() as i32 - i32::from(a.top);
        for row in 0..i32::from(a.height) {
            for col in 0..i32::from(a.width) {
                let (px, py) = (left + col, top + row);
                if px < 0 || py < 0 || px >= W as i32 || py >= H as i32 {
                    continue;
                }
                let cov = u32::from(
                    atlas.pixels()[((u32::from(a.y) + row as u32) * aw
                        + u32::from(a.x)
                        + col as u32) as usize],
                );
                let d = &mut image[(py as u32 * W + px as u32) as usize];
                *d = (cov + u32::from(*d) * (255 - cov) / 255).min(255) as u8;
            }
        }
    }
}

fn render() -> Vec<u8> {
    let mut fonts = Fonts::new();
    let latin = fonts.load(engine_text::fonts::NOTO_SANS, 0).unwrap();
    let arabic = fonts.load(engine_text::fonts::NOTO_SANS_ARABIC, 0).unwrap();
    let hebrew = fonts.load(engine_text::fonts::NOTO_SANS_HEBREW, 0).unwrap();
    let mut atlas = GlyphAtlas::new(1024, 1024);
    let mut layout = TextLayout::new();
    let mut image = vec![0u8; (W * H) as usize];
    let width = W as f32 - 40.0;
    let paragraphs: [(&str, [FontId; 2], TextDirection, f32); 4] = [
        // An Arabic paragraph: right to left, right aligned, its numbers left to right.
        (
            "مرحبا بك في اللعبة! لديك ٣ أرواح و 250 نقطة ذهب (Gold) في المستوى 12.",
            [arabic, latin],
            TextDirection::Auto,
            12.0,
        ),
        // Hebrew inside a Latin paragraph: read right to left in place.
        (
            "Level 3: שלב שלוש, 42 points",
            [latin, hebrew],
            TextDirection::Auto,
            118.0,
        ),
        // A Hebrew paragraph with Latin and numbers inside, a bracket mirrored.
        (
            "שלום, world! 2026 (בדיקה)",
            [hebrew, latin],
            TextDirection::Auto,
            172.0,
        ),
        // Latin in a right-to-left paragraph: aligned right, its punctuation at the left.
        (
            "Hello, world!",
            [latin, hebrew],
            TextDirection::RightToLeft,
            226.0,
        ),
    ];
    for (text, stack, direction, y) in paragraphs {
        let style = TextStyle {
            size: SIZE,
            max_width: Some(width),
            align: Align::Start,
            direction,
            ..TextStyle::default()
        };
        layout.layout(&mut fonts, &stack, text, &style);
        draw(&mut image, &layout, &fonts, &mut atlas, (20.0, y));
    }
    // The layout box's edges, a pixel wide, for the eye.
    for y in 0..H {
        for x in [19, W - 20] {
            image[(y * W + x) as usize] = 64;
        }
    }
    image
}

#[test]
fn text_in_both_directions_matches_its_golden_image() {
    let image = render();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/bidi.png");
    let update = std::env::var("ENGINE_UPDATE_GOLDEN").is_ok_and(|v| v == "1");
    if update || !path.exists() {
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&image)
            .unwrap();
        println!("wrote {}", path.display());
        if !update {
            panic!(
                "no golden image yet: wrote {}; look at it, then commit",
                path.display()
            );
        }
        return;
    }
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut golden = vec![0u8; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut golden).unwrap();
    assert_eq!((info.width, info.height), (W, H));
    golden.truncate(info.buffer_size());
    // The CPU's rasterization is deterministic; a few pixels may differ by rounding on
    // another CPU's code path.
    let differing = image
        .iter()
        .zip(&golden)
        .filter(|(a, b)| a.abs_diff(**b) > 8)
        .count();
    println!("{differing} of {} pixels differ", image.len());
    assert!(
        differing * 1000 <= image.len(),
        "{differing} pixels differ from {}: ENGINE_UPDATE_GOLDEN=1 writes it anew (look at it first)",
        path.display()
    );
}
