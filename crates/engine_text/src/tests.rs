#![allow(clippy::unwrap_used, clippy::float_cmp)]

use crate::*;

fn setup() -> (Fonts, FontId) {
    let mut fonts = Fonts::new();
    let go = fonts.load(fonts::GO_REGULAR, 0).unwrap();
    (fonts, go)
}

fn style(max_width: Option<f32>) -> TextStyle {
    TextStyle {
        size: 16.0,
        max_width,
        ..TextStyle::default()
    }
}

#[test]
fn one_line_advances_left_to_right() {
    let (mut fonts, go) = setup();
    let mut l = TextLayout::new();
    l.layout(&mut fonts, &[go], "Hello", &style(None));
    assert_eq!(l.glyphs().len(), 5);
    assert_eq!(l.lines().len(), 1);
    let xs: Vec<f32> = l.glyphs().iter().map(|g| g.x).collect();
    assert!(xs.windows(2).all(|w| w[1] > w[0]), "{xs:?}");
    assert!(l.width() > 30.0 && l.width() < 60.0, "{}", l.width());
    let clusters: Vec<u32> = l.glyphs().iter().map(|g| g.cluster).collect();
    assert_eq!(clusters, [0, 1, 2, 3, 4]);
    // Every glyph exists in the font (no .notdef).
    assert!(l.glyphs().iter().all(|g| g.glyph != 0));
}

#[test]
fn cyrillic_and_latin_runs_shape_every_character() {
    let (mut fonts, go) = setup();
    let mut l = TextLayout::new();
    let text = "Scene Сцена 42";
    l.layout(&mut fonts, &[go], text, &style(None));
    assert_eq!(l.glyphs().len(), text.chars().count());
    assert!(l.glyphs().iter().all(|g| g.glyph != 0));
    // Clusters point at the start of each character.
    let starts: Vec<u32> = text.char_indices().map(|(i, _)| i as u32).collect();
    let clusters: Vec<u32> = l.glyphs().iter().map(|g| g.cluster).collect();
    assert_eq!(clusters, starts);
}

#[test]
fn wrapping_hangs_trailing_spaces_and_respects_the_width() {
    let (mut fonts, go) = setup();
    let mut l = TextLayout::new();
    let text = "The quick brown fox jumps over the lazy dog again and again.";
    let max = 120.0;
    l.layout(&mut fonts, &[go], text, &style(Some(max)));
    assert!(l.lines().len() >= 3, "{:?}", l.lines());
    for line in l.lines() {
        assert!(line.width <= max + 0.01, "{line:?}");
        // Lines never start with the space that ended the previous one.
        let first = &text[line.text.clone()];
        assert!(!first.starts_with(' '), "{first:?}");
    }
    // All glyphs are placed exactly once.
    assert_eq!(l.glyphs().len(), text.chars().count());
    // Baselines step down by the same amount.
    let b: Vec<f32> = l.lines().iter().map(|line| line.baseline).collect();
    let step = b[1] - b[0];
    assert!(step > 0.0 && b.windows(2).all(|w| ((w[1] - w[0]) - step).abs() < 1e-3));
    assert_eq!(l.width(), max);
}

#[test]
fn a_word_longer_than_the_line_is_broken_inside() {
    let (mut fonts, go) = setup();
    let mut l = TextLayout::new();
    l.layout(
        &mut fonts,
        &[go],
        "Supercalifragilistic",
        &style(Some(40.0)),
    );
    assert!(l.lines().len() > 1);
    for line in l.lines() {
        assert!(!line.glyphs.is_empty());
        assert!(line.width <= 40.0 + 12.0);
    }
}

#[test]
fn newlines_make_lines_including_empty_ones() {
    let (mut fonts, go) = setup();
    let mut l = TextLayout::new();
    l.layout(&mut fonts, &[go], "one\n\nthree\r\n", &style(None));
    let lines = l.lines();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[1].glyphs.len(), 0);
    assert_eq!(lines[3].glyphs.len(), 0);
    assert_eq!(l.glyphs().len(), "onethree".len());
    assert_eq!(&"one\n\nthree\r\n"[lines[2].text.clone()], "three");
}

#[test]
fn alignment_shifts_lines() {
    let (mut fonts, go) = setup();
    let mut l = TextLayout::new();
    let mut s = style(Some(200.0));
    l.layout(&mut fonts, &[go], "Hi", &s);
    let left = l.glyphs()[0].x;
    s.align = Align::Center;
    l.layout(&mut fonts, &[go], "Hi", &s);
    let center = l.glyphs()[0].x;
    s.align = Align::End;
    l.layout(&mut fonts, &[go], "Hi", &s);
    let right = l.glyphs()[0].x;
    let w = l.lines()[0].width;
    assert!((center - left - (200.0 - w) / 2.0).abs() < 1e-3);
    assert!((right - left - (200.0 - w)).abs() < 1e-3);
}

#[test]
fn missing_characters_fall_back_or_use_notdef() {
    let (mut fonts, go) = setup();
    let mono = fonts.load(fonts::GO_MONO, 0).unwrap();
    let mut l = TextLayout::new();
    // Neither Go font has CJK: the primary's .notdef is used, nothing panics.
    l.layout(&mut fonts, &[go, mono], "a中b", &style(None));
    assert_eq!(l.glyphs().len(), 3);
    assert_eq!(l.glyphs()[1].glyph, 0);
    assert_eq!(l.glyphs()[0].font, go);
    // A character the primary lacks but a fallback has goes to the fallback.
    let stack = [go, mono];
    let missing_in_regular = ('\u{2500}'..'\u{2600}').find(|&c| {
        fonts.get(go).unwrap().glyph(c).is_none() && fonts.get(mono).unwrap().glyph(c).is_some()
    });
    if let Some(c) = missing_in_regular {
        let text = format!("x{c}");
        l.layout(&mut fonts, &stack, &text, &style(None));
        assert_eq!(l.glyphs()[1].font, mono, "{c:?}");
    }
}

#[test]
fn layout_is_deterministic_and_reusable() {
    let (mut fonts, go) = setup();
    let mut a = TextLayout::new();
    let mut b = TextLayout::new();
    let text = "Deterministic layout, детерминированная раскладка.";
    a.layout(&mut fonts, &[go], text, &style(Some(150.0)));
    b.layout(&mut fonts, &[go], "something else first", &style(None));
    b.layout(&mut fonts, &[go], text, &style(Some(150.0)));
    assert_eq!(a.glyphs(), b.glyphs());
    assert_eq!(a.lines(), b.lines());
}

#[test]
fn rasterizer_covers_a_rectangle_exactly() {
    let mut r = Rasterizer::new();
    r.reset(10, 8);
    // Rectangle from x 2.5 to 7.5, y 2 to 6, clockwise in y-down coordinates.
    let pts = [(2.5, 2.0), (7.5, 2.0), (7.5, 6.0), (2.5, 6.0)];
    for i in 0..4 {
        r.line(pts[i], pts[(i + 1) % 4]);
    }
    let mut cov = Vec::new();
    r.coverage(&mut cov);
    let at = |x: usize, y: usize| cov[y * 10 + x];
    for y in 2..6 {
        assert_eq!(at(2, y), 128, "half pixel on the left edge");
        for x in 3..7 {
            assert_eq!(at(x, y), 255);
        }
        assert_eq!(at(7, y), 128, "half pixel on the right edge");
        assert_eq!(at(1, y), 0);
        assert_eq!(at(8, y), 0);
    }
    assert!((0..10).all(|x| at(x, 0) == 0 && at(x, 7) == 0));
}

#[test]
fn glyphs_rasterize_with_plausible_ink() {
    let (fonts, go) = setup();
    let font = fonts.get(go).unwrap();
    let mut r = Rasterizer::new();
    let mut bmp = GlyphBitmap::default();
    let h = font.glyph('H').unwrap();
    rasterize(font, h, 32.0, 0.0, &mut r, &mut bmp).unwrap();
    // A 32 px 'H' is roughly cap height tall and a bit narrower.
    assert!((18..=26).contains(&bmp.height), "{}", bmp.height);
    assert!((14..=26).contains(&bmp.width), "{}", bmp.width);
    assert!(
        bmp.top > 0 && bmp.top as u32 == bmp.height,
        "sits on the baseline"
    );
    let ink: u32 = bmp.coverage.iter().map(|&c| u32::from(c)).sum();
    let area = ink as f32 / 255.0;
    assert!(area > 60.0 && area < 250.0, "{area}");
    // The space glyph has no outline.
    let space = font.glyph(' ').unwrap();
    assert!(rasterize(font, space, 32.0, 0.0, &mut r, &mut bmp).is_none());
    // A subpixel shift keeps the ink (area) and moves it.
    let mut shifted = GlyphBitmap::default();
    rasterize(font, h, 32.0, 0.5, &mut r, &mut shifted).unwrap();
    let ink2: u32 = shifted.coverage.iter().map(|&c| u32::from(c)).sum();
    assert!((ink2 as f32 / ink as f32 - 1.0).abs() < 0.02);
    assert_ne!(bmp.coverage, shifted.coverage);
}

#[test]
fn atlas_caches_packs_and_reports_dirty_rects() {
    let (fonts, go) = setup();
    let mut atlas = GlyphAtlas::new(128, 128);
    let font = fonts.get(go).unwrap();
    let a = font.glyph('A').unwrap();
    let (k1, _) = GlyphKey::at(go, a, 20.0, 10.3);
    let g1 = atlas.get(&fonts, k1).unwrap();
    assert_eq!(atlas.get(&fonts, k1), Some(g1));
    let dirty = atlas.take_dirty().unwrap();
    assert!(dirty.x <= u32::from(g1.x) && dirty.width >= u32::from(g1.width));
    assert!(atlas.take_dirty().is_none());
    // Another subpixel position is another entry, placed elsewhere.
    let (k2, _) = GlyphKey::at(go, a, 20.0, 10.6);
    assert_ne!(k1, k2);
    let g2 = atlas.get(&fonts, k2).unwrap();
    assert_ne!((g1.x, g1.y), (g2.x, g2.y));
    // Pixels were copied in.
    let px = atlas.pixels();
    let (w, _) = atlas.size();
    let ink: u32 = (0..u32::from(g1.height))
        .flat_map(|y| (0..u32::from(g1.width)).map(move |x| (x, y)))
        .map(|(x, y)| u32::from(px[((u32::from(g1.y) + y) * w + u32::from(g1.x) + x) as usize]))
        .sum();
    assert!(ink > 0);
    // Filling it up clears and starts over, with a new generation.
    let generation = atlas.generation();
    for size in 20..200 {
        let (k, _) = GlyphKey::at(go, a, size as f32, 0.0);
        let _ = atlas.get(&fonts, k);
    }
    assert!(atlas.generation() > generation);
}

#[test]
fn subpixel_keys_round_to_quarters() {
    let f = FontId(0);
    assert_eq!(GlyphKey::at(f, 1, 10.0, 3.0).1, 3);
    assert_eq!(GlyphKey::at(f, 1, 10.0, 3.26).0.subpixel, 1);
    let (k, x) = GlyphKey::at(f, 1, 10.0, 3.9);
    assert_eq!((k.subpixel, x), (0, 4));
    assert_eq!(GlyphKey::at(f, 1, 10.0, -0.25).1, -1);
}

#[test]
fn bundled_fonts_cover_their_scripts_and_icons() {
    let noto = Font::from_bytes(crate::fonts::NOTO_SANS, 0).unwrap();
    for c in ['A', 'z', 'Ж', 'я', 'Ω', 'ő', 'ệ', '€', '…'] {
        assert!(noto.glyph(c).is_some(), "Noto Sans lacks {c}");
    }
    // Noto Sans has no arrows or math: JetBrains Mono fills in for them in the UI stack.
    assert!(noto.glyph('→').is_none());
    let mono = Font::from_bytes(crate::fonts::JETBRAINS_MONO, 0).unwrap();
    for c in ['{', '0', 'Ж', '≠', '→', '✓', '⌘', '⇧', '⏎'] {
        assert!(mono.glyph(c).is_some(), "JetBrains Mono lacks {c}");
    }
    let icons = Font::from_bytes(crate::fonts::NERD_SYMBOLS, 0).unwrap();
    // Material Design play / pause / stop / folder, Codicons gear, Font Awesome cube.
    for c in [
        '\u{F040A}',
        '\u{F03E4}',
        '\u{F04DB}',
        '\u{F024B}',
        '\u{EB51}',
        '\u{F1B2}',
    ] {
        assert!(icons.glyph(c).is_some(), "no icon U+{:X}", c as u32);
    }
    // The UI stack shapes text, symbols and icons from one string.
    let mut fonts = Fonts::new();
    let text = fonts.load(crate::fonts::NOTO_SANS, 0).unwrap();
    let mono = fonts.load(crate::fonts::JETBRAINS_MONO, 0).unwrap();
    let symbols = fonts.load(crate::fonts::NERD_SYMBOLS, 0).unwrap();
    let mut layout = TextLayout::new();
    let stack = [text, mono, symbols];
    layout.layout(&mut fonts, &stack, "\u{F040A} Играть →", &style(None));
    let used: Vec<FontId> = layout.glyphs().iter().map(|g| g.font).collect();
    assert_eq!(used.first(), Some(&symbols), "{used:?}");
    assert_eq!(used.last(), Some(&mono), "{used:?}");
    assert!(
        used[1..used.len() - 1].iter().all(|&f| f == text),
        "{used:?}"
    );
}

/// Fonts for right-to-left text: Arabic and Hebrew, Go for Latin and digits.
fn rtl_setup() -> (Fonts, [FontId; 2], [FontId; 2]) {
    let mut fonts = Fonts::new();
    let arabic = fonts.load(fonts::NOTO_SANS_ARABIC, 0).unwrap();
    let hebrew = fonts.load(fonts::NOTO_SANS_HEBREW, 0).unwrap();
    let go = fonts.load(fonts::GO_REGULAR, 0).unwrap();
    (fonts, [arabic, go], [hebrew, go])
}

/// The clusters of the first line's glyphs, left to right.
fn clusters(l: &TextLayout) -> Vec<u32> {
    l.glyphs()[l.lines()[0].glyphs.clone()]
        .iter()
        .map(|g| g.cluster)
        .collect()
}

#[test]
fn numbers_in_arabic_text_read_left_to_right() {
    let (mut fonts, arabic, _) = rtl_setup();
    let mut l = TextLayout::new();
    let text = "عدد 123 عنصر";
    l.layout(&mut fonts, &arabic, text, &style(None));
    let line = &l.lines()[0];
    assert!(line.rtl, "an Arabic paragraph runs right to left");
    let c = clusters(&l);
    let digits: Vec<usize> = (0..c.len())
        .filter(|&k| text.as_bytes()[c[k] as usize].is_ascii_digit())
        .collect();
    let digit_clusters: Vec<u32> = digits.iter().map(|&k| c[k]).collect();
    // The number keeps its order: 1, 2, 3 from left to right.
    assert_eq!(digit_clusters.len(), 3, "{c:?}");
    assert!(digit_clusters.windows(2).all(|w| w[0] < w[1]), "{c:?}");
    // The last word is at the left, the first at the right.
    let second_word = text.rfind(' ').unwrap() as u32;
    assert!(c[0] > second_word, "{c:?}");
    assert!(*c.last().unwrap() < 8, "{c:?}");
    // The number between the words on screen too (marks may sit left of their pen, so
    // glyphs are not ordered by x one by one).
    let x = |k: usize| l.glyphs()[k].x;
    let (first_digit, last_digit) = (digits[0], digits[2]);
    assert!(x(0) < x(first_digit) && x(last_digit) < x(c.len() - 1));
    assert!(x(first_digit) < x(digits[1]) && x(digits[1]) < x(last_digit));
}

#[test]
fn hebrew_inside_latin_reads_right_to_left_in_place() {
    let (mut fonts, _, hebrew) = rtl_setup();
    let mut l = TextLayout::new();
    let text = "Hello שלום world";
    l.layout(&mut fonts, &hebrew, text, &style(None));
    assert!(!l.lines()[0].rtl);
    let c = clusters(&l);
    let start = text.find('ש').unwrap() as u32;
    let end = text.rfind(' ').unwrap() as u32;
    let inner: Vec<u32> = c
        .iter()
        .copied()
        .filter(|&x| x >= start && x < end)
        .collect();
    assert_eq!(inner.len(), 4, "{c:?}");
    // The Hebrew word's letters run right to left, between the two Latin words.
    assert!(inner.windows(2).all(|w| w[0] > w[1]), "{c:?}");
    assert_eq!(&c[..5], &[0, 1, 2, 3, 4]);
    assert_eq!(*c.last().unwrap(), text.len() as u32 - 1);
}

#[test]
fn a_paragraph_s_direction_can_be_forced() {
    let (mut fonts, go) = setup();
    let mut l = TextLayout::new();
    let rtl = TextStyle {
        direction: TextDirection::RightToLeft,
        ..style(None)
    };
    l.layout(&mut fonts, &[go], "Hello!", &rtl);
    assert!(l.lines()[0].rtl);
    // The exclamation mark goes to the paragraph's end: the left.
    assert_eq!(clusters(&l), [5, 0, 1, 2, 3, 4]);
    // Left to right, an Arabic first letter does not turn the paragraph.
    let (mut fonts, arabic, _) = rtl_setup();
    let ltr = TextStyle {
        direction: TextDirection::LeftToRight,
        ..style(None)
    };
    l.layout(&mut fonts, &arabic, "عدد: 3", &ltr);
    assert!(!l.lines()[0].rtl);
}

#[test]
fn right_to_left_lines_align_right_and_hang_their_spaces_left() {
    let (mut fonts, arabic, _) = rtl_setup();
    let mut l = TextLayout::new();
    let text = "مرحبا بك في اللعبة الجديدة، لديك ثلاثة أرواح ومئتان وخمسون نقطة ذهب";
    let max = 220.0;
    l.layout(&mut fonts, &arabic, text, &style(Some(max)));
    assert!(l.lines().len() >= 2, "{:?}", l.lines());
    for line in l.lines() {
        assert!(line.rtl);
        // The visible content ends at the right edge; its trailing space (the line's
        // logical end) hangs past its left.
        let visible = l.glyphs()[line.glyphs.clone()]
            .iter()
            .filter(|g| text.as_bytes()[g.cluster as usize] != b' ')
            .map(|g| g.x)
            .fold(f32::INFINITY, f32::min);
        assert!(
            (visible - (max - line.width)).abs() < 1.0,
            "{visible} against {}: {line:?}",
            max - line.width
        );
        assert!(line.width <= max + 0.01);
    }
    // Align::End puts them on the left.
    let end = TextStyle {
        align: Align::End,
        ..style(Some(max))
    };
    l.layout(&mut fonts, &arabic, text, &end);
    for line in l.lines() {
        let visible = l.glyphs()[line.glyphs.clone()]
            .iter()
            .filter(|g| text.as_bytes()[g.cluster as usize] != b' ')
            .map(|g| g.x)
            .fold(f32::INFINITY, f32::min);
        assert!(visible.abs() < 1.0, "{visible}: {line:?}");
    }
}

#[test]
fn brackets_mirror_in_right_to_left_text() {
    let (mut fonts, _, hebrew) = rtl_setup();
    let mut l = TextLayout::new();
    // "(א)" in a right-to-left paragraph: its opening bracket is at the right, drawn as
    // a closing one, so that it opens towards the letter.
    let rtl = TextStyle {
        direction: TextDirection::RightToLeft,
        ..style(None)
    };
    l.layout(&mut fonts, &hebrew, "(א)", &rtl);
    let rtl_glyphs: Vec<(u32, u16)> = l.glyphs().iter().map(|g| (g.cluster, g.glyph)).collect();
    l.layout(&mut fonts, &hebrew, "(a)", &style(None));
    let ltr_glyphs: Vec<(u32, u16)> = l.glyphs().iter().map(|g| (g.cluster, g.glyph)).collect();
    // Visual order: the closing bracket's cluster at the left, the opening's at the right.
    assert_eq!(rtl_glyphs[0].0, 3, "{rtl_glyphs:?}");
    assert_eq!(rtl_glyphs[2].0, 0, "{rtl_glyphs:?}");
    // The opening bracket at the right is drawn as ")" (left to right's last glyph).
    assert_eq!(
        rtl_glyphs[2].1, ltr_glyphs[2].1,
        "{rtl_glyphs:?} {ltr_glyphs:?}"
    );
    assert_eq!(
        rtl_glyphs[0].1, ltr_glyphs[0].1,
        "{rtl_glyphs:?} {ltr_glyphs:?}"
    );
}
