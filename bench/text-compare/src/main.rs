//! Text layout comparison for the text ADR (outside the workspace, like
//! `bench/ecs-compare`): Parley 0.11 and cosmic-text 0.19 against shaping alone with
//! HarfRust 0.13 (the lower bound for anything built on it). All three get the same font
//! from bytes (Go Regular, BSD-3-Clause, `third_party/fonts/go`) and no system fonts.
//!
//! Scenarios:
//! - editor labels: 1,000 short labels shaped and laid out without wrapping (what a UI
//!   does when text changes or on first display);
//! - a 10 KB paragraph (English and Russian) built and wrapped at 600 px;
//! - re-wrapping that paragraph at three widths (window resize) without re-shaping.
//!
//! ```text
//! cargo run --release --manifest-path bench/text-compare/Cargo.toml
//! ```

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

const FONT: &[u8] = include_bytes!("../../../third_party/fonts/go/Go-Regular.ttf");
const SIZE: f32 = 14.0;

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

mod parley_case {
    use parley::fontique::Blob;
    use parley::{FontContext, FontFamily, Layout, LayoutContext, StyleProperty};
    use std::sync::Arc;

    pub struct Ctx {
        font: FontContext,
        layout: LayoutContext<()>,
        family: String,
    }

    impl Ctx {
        pub fn new() -> Self {
            let mut font = FontContext::new();
            let families = font
                .collection
                .register_fonts(Blob::new(Arc::new(super::FONT.to_vec())), None);
            let family = font
                .collection
                .family_name(families[0].0)
                .unwrap()
                .to_owned();
            Self {
                font,
                layout: LayoutContext::new(),
                family,
            }
        }

        pub fn build(&mut self, text: &str, width: Option<f32>) -> Layout<()> {
            let mut builder = self.layout.ranged_builder(&mut self.font, text, 1.0, true);
            builder.push_default(StyleProperty::FontSize(super::SIZE));
            builder.push_default(StyleProperty::FontFamily(FontFamily::named(&self.family)));
            let mut layout: Layout<()> = builder.build(text);
            layout.break_all_lines(width);
            layout
        }
    }

    pub fn glyphs(layout: &Layout<()>) -> usize {
        layout
            .lines()
            .flat_map(|l| l.items())
            .map(|item| match item {
                parley::PositionedLayoutItem::GlyphRun(run) => run.glyphs().count(),
                parley::PositionedLayoutItem::InlineBox(_) => 0,
            })
            .sum()
    }
}

mod cosmic_case {
    use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping};
    use std::sync::Arc;

    pub fn system() -> FontSystem {
        FontSystem::new_with_fonts([cosmic_text::fontdb::Source::Binary(Arc::new(
            super::FONT.to_vec(),
        ))])
    }

    pub fn build(fs: &mut FontSystem, text: &str, width: Option<f32>) -> Buffer {
        let mut buffer = Buffer::new_empty(Metrics::new(super::SIZE, super::SIZE * 1.25));
        buffer.set_size(width, None);
        buffer.set_text(
            text,
            &Attrs::new().family(Family::Name("Go")),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(fs, false);
        buffer
    }

    pub fn glyphs(buffer: &Buffer) -> usize {
        buffer.layout_runs().map(|r| r.glyphs.len()).sum()
    }
}

mod harfrust_case {
    use harfrust::{FontRef, ShapeOptions, ShaperData, UnicodeBuffer};

    pub struct Ctx {
        data: ShaperData,
        font: FontRef<'static>,
        buffer: Option<UnicodeBuffer>,
    }

    impl Ctx {
        pub fn new() -> Self {
            let font = FontRef::from_index(super::FONT, 0).unwrap();
            Self {
                data: ShaperData::new(&font),
                font,
                buffer: Some(UnicodeBuffer::new()),
            }
        }

        /// Shapes `text` and returns the advance sum (no line breaking).
        pub fn shape(&mut self, text: &str) -> (usize, i32) {
            let shaper = self.data.shaper(&self.font).build();
            let mut buffer = self.buffer.take().unwrap();
            buffer.push_str(text);
            buffer.guess_segment_properties();
            let glyphs = shaper.shape(buffer, ShapeOptions::new());
            let n = glyphs.len();
            let advance = glyphs.glyph_positions().iter().map(|p| p.x_advance).sum();
            self.buffer = Some(glyphs.clear());
            (n, advance)
        }
    }
}

/// What a fast path for simple text could cost: script runs (Latin/Cyrillic, shared
/// punctuation attached to the current run), HarfRust with cached shape plans, and greedy
/// wrapping at spaces. No bidi, no font fallback: the lower bound of a real fast path.
mod fast_case {
    use harfrust::{
        Direction, FontRef, ShapeOptions, ShapePlan, ShaperData, UnicodeBuffer, script,
    };

    pub struct Ctx {
        data: ShaperData,
        font: FontRef<'static>,
        latin: ShapePlan,
        cyrillic: ShapePlan,
        buffer: Option<UnicodeBuffer>,
        /// (byte offset of the cluster, advance) per glyph.
        glyphs: Vec<(u32, i32)>,
        breaks: Vec<u32>,
        scale: f32,
    }

    fn is_cyrillic(c: char) -> bool {
        ('\u{0400}'..='\u{052f}').contains(&c)
    }

    impl Ctx {
        pub fn new() -> Self {
            let font = FontRef::from_index(super::FONT, 0).unwrap();
            let data = ShaperData::new(&font);
            let shaper = data.shaper(&font).build();
            let latin = ShapePlan::new(
                &shaper,
                Direction::LeftToRight,
                Some(script::LATIN),
                None,
                &[],
            );
            let cyrillic = ShapePlan::new(
                &shaper,
                Direction::LeftToRight,
                Some(script::CYRILLIC),
                None,
                &[],
            );
            let upem = shaper.units_per_em() as f32;
            Self {
                data,
                font,
                latin,
                cyrillic,
                buffer: Some(UnicodeBuffer::new()),
                glyphs: Vec::new(),
                breaks: Vec::new(),
                scale: super::SIZE / upem,
            }
        }

        fn shape_run(&mut self, text: &str, offset: usize, cyrillic: bool) {
            let shaper = self.data.shaper(&self.font).build();
            let mut buffer = self.buffer.take().unwrap();
            buffer.push_str(text);
            buffer.set_direction(Direction::LeftToRight);
            buffer.set_script(if cyrillic {
                script::CYRILLIC
            } else {
                script::LATIN
            });
            let plan = if cyrillic {
                &self.cyrillic
            } else {
                &self.latin
            };
            let out = shaper.shape(buffer, ShapeOptions::new().plan(Some(plan)));
            for (info, pos) in out.glyph_infos().iter().zip(out.glyph_positions()) {
                self.glyphs
                    .push((offset as u32 + info.cluster, pos.x_advance));
            }
            self.buffer = Some(out.clear());
        }

        /// Returns (glyphs, lines).
        pub fn layout(&mut self, text: &str, width: Option<f32>) -> (usize, usize) {
            self.glyphs.clear();
            let mut start = 0;
            let mut current = None;
            for (i, c) in text.char_indices() {
                let class = if is_cyrillic(c) {
                    Some(true)
                } else if c.is_ascii_alphabetic() {
                    Some(false)
                } else {
                    None
                };
                if let (Some(now), Some(run)) = (class, current)
                    && now != run
                {
                    self.shape_run(&text[start..i], start, run);
                    start = i;
                }
                if class.is_some() {
                    current = class;
                }
            }
            self.shape_run(&text[start..], start, current.unwrap_or(false));
            let Some(width) = width else {
                return (self.glyphs.len(), 1);
            };
            // Greedy wrap at UAX #14 break opportunities.
            let limit = (width / self.scale) as i32;
            self.breaks.clear();
            self.breaks
                .extend(unicode_linebreak::linebreaks(text).map(|(at, _)| at as u32));
            let (mut lines, mut x, mut since_break) = (1, 0i32, 0i32);
            let mut next = 0;
            for &(cluster, advance) in &self.glyphs {
                while next < self.breaks.len() && self.breaks[next] <= cluster {
                    if self.breaks[next] == cluster {
                        since_break = 0;
                    }
                    next += 1;
                }
                x += advance;
                since_break += advance;
                if x > limit {
                    lines += 1;
                    x = since_break;
                }
            }
            (self.glyphs.len(), lines)
        }
    }
}

fn main() {
    let labels = labels();
    let text = paragraph();
    let label_bytes: usize = labels.iter().map(String::len).sum();
    println!(
        "### Text layout (µs, median; lower is better)\n\nFont: Go Regular {SIZE} px, from bytes. \
         Labels: 1,000 ({label_bytes} bytes). Paragraph: {} bytes, English + Russian.\n",
        text.len()
    );
    println!(
        "| scenario | Parley 0.11.1 | cosmic-text 0.19.0 | HarfRust 0.13.3 shaping only | fast path sketch (HarfRust, cached plans, UAX #14 greedy wrap) | engine_text |"
    );
    println!("|---|---:|---:|---:|---:|---:|");
    let mut fc = fast_case::Ctx::new();
    let mut efonts = engine_text::Fonts::new();
    let ego = efonts.load(FONT, 0).unwrap();
    let mut elayout = engine_text::TextLayout::new();
    let estyle = |w: Option<f32>| engine_text::TextStyle {
        size: SIZE,
        max_width: w,
        ..engine_text::TextStyle::default()
    };

    let mut pc = parley_case::Ctx::new();
    let mut cs = cosmic_case::system();
    let mut hc = harfrust_case::Ctx::new();

    // Glyph counts must agree, or the comparison is not like for like.
    let pg: usize = labels
        .iter()
        .map(|l| parley_case::glyphs(&pc.build(l, None)))
        .sum();
    let cg: usize = labels
        .iter()
        .map(|l| cosmic_case::glyphs(&cosmic_case::build(&mut cs, l, None)))
        .sum();
    let hg: usize = labels.iter().map(|l| hc.shape(l).0).sum();
    let fg: usize = labels.iter().map(|l| fc.layout(l, None).0).sum();
    let eg: usize = labels
        .iter()
        .map(|l| {
            elayout.layout(&mut efonts, &[ego], l, &estyle(None));
            elayout.glyphs().len()
        })
        .sum();

    let runs = 21;
    let p = median(runs, || {
        for l in &labels {
            black_box(pc.build(l, None));
        }
    });
    let c = median(runs, || {
        for l in &labels {
            black_box(cosmic_case::build(&mut cs, l, None));
        }
    });
    let h = median(runs, || {
        for l in &labels {
            black_box(hc.shape(l));
        }
    });
    let f = median(runs, || {
        for l in &labels {
            black_box(fc.layout(l, None));
        }
    });
    let e = median(runs, || {
        for l in &labels {
            elayout.layout(&mut efonts, &[ego], l, &estyle(None));
            black_box(elayout.glyphs().len());
        }
    });
    println!("| 1,000 labels, shape + lay out | {p:.0} | {c:.0} | {h:.0} | {f:.0} | {e:.0} |");
    println!(
        "| per label | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |",
        p / 1000.0,
        c / 1000.0,
        h / 1000.0,
        f / 1000.0,
        e / 1000.0
    );
    println!("| glyphs (check) | {pg} | {cg} | {hg} | {fg} | {eg} |");

    let p = median(runs, || {
        black_box(pc.build(&text, Some(600.0)));
    });
    let c = median(runs, || {
        black_box(cosmic_case::build(&mut cs, &text, Some(600.0)));
    });
    let h = median(runs, || {
        black_box(hc.shape(&text));
    });
    let f = median(runs, || {
        black_box(fc.layout(&text, Some(600.0)));
    });
    let e = median(runs, || {
        elayout.layout(&mut efonts, &[ego], &text, &estyle(Some(600.0)));
        black_box(elayout.lines().len());
    });
    let lines_parley = pc.build(&text, Some(600.0)).len();
    let lines_fast = fc.layout(&text, Some(600.0)).1;
    elayout.layout(&mut efonts, &[ego], &text, &estyle(Some(600.0)));
    let lines_engine = elayout.lines().len();
    println!(
        "| 10 KB paragraph, shape + wrap at 600 px | {p:.0} | {c:.0} | {h:.0} (no wrap) | {f:.0} | {e:.0} |"
    );
    println!("| lines (check) | {lines_parley} | | | {lines_fast} | {lines_engine} |");

    let mut layout = pc.build(&text, Some(600.0));
    let p = median(runs, || {
        for w in [400.0, 800.0, 600.0] {
            layout.break_all_lines(Some(w));
            black_box(layout.height());
        }
    });
    let mut buffer = cosmic_case::build(&mut cs, &text, Some(600.0));
    let c = median(runs, || {
        for w in [400.0, 800.0, 600.0] {
            buffer.set_size(Some(w), None);
            buffer.shape_until_scroll(&mut cs, false);
            black_box(buffer.layout_runs().count());
        }
    });
    println!("| re-wrap at 3 widths (no re-shaping) | {p:.0} | {c:.0} | — | — | — |");
    println!();
    let _ = Arc::new(());
}
