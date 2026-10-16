//! Text layout: bidirectional levels (UAX #9), itemization by font, script and level,
//! shaping, UAX #14 line breaking, wrapping with hanging whitespace, lines in visual order,
//! alignment.

use crate::bidi::{Bidi, visual_order};
use crate::font::{FontId, Fonts};
use core::ops::Range;
use harfrust::{Direction, Script, UnicodeBuffer};
use unicode_linebreak::BreakOpportunity;
use unicode_script::UnicodeScript;

/// Horizontal alignment of lines within the layout width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    /// The paragraph's start: left for left-to-right text, right for right-to-left.
    #[default]
    Start,
    /// Centered.
    Center,
    /// The paragraph's end: right for left-to-right text, left for right-to-left.
    End,
}

/// A paragraph's direction (UAX #9's paragraph level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextDirection {
    /// The direction of its first strong character (Arabic or Hebrew: right to left; Latin,
    /// Cyrillic, CJK: left to right), left to right without one.
    #[default]
    Auto,
    /// Left to right (a left-to-right UI's labels).
    LeftToRight,
    /// Right to left (a right-to-left UI's labels, whatever their first letter).
    RightToLeft,
}

/// How to lay out a piece of text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    /// Font size in pixels (the em size).
    pub size: f32,
    /// Distance between baselines in pixels; `None` uses the primary font's natural line
    /// height (ascent + descent + line gap).
    pub line_height: Option<f32>,
    /// Wrap width in pixels; `None` breaks only at newlines.
    pub max_width: Option<f32>,
    /// Alignment of lines within `max_width` (or within the widest line without it).
    pub align: Align,
    /// The paragraphs' direction.
    pub direction: TextDirection,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            size: 14.0,
            line_height: None,
            max_width: None,
            align: Align::Start,
            direction: TextDirection::Auto,
        }
    }
}

/// A glyph placed by the layout. Coordinates are pixels from the layout's top-left
/// corner; `y` is the baseline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionedGlyph {
    /// Font of the glyph.
    pub font: FontId,
    /// Glyph id in that font.
    pub glyph: u16,
    /// Pen position x.
    pub x: f32,
    /// Baseline y.
    pub y: f32,
    /// Byte offset of the glyph's cluster in the text.
    pub cluster: u32,
}

/// A laid-out line.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// Glyph range in [`TextLayout::glyphs`].
    pub glyphs: Range<usize>,
    /// Byte range of the text on this line (including hanging whitespace).
    pub text: Range<usize>,
    /// Width of the visible content (trailing whitespace excluded).
    pub width: f32,
    /// Baseline y.
    pub baseline: f32,
    /// Its paragraph runs right to left ([`Align::Start`] is then the right).
    pub rtl: bool,
}

#[derive(Clone, Copy)]
struct Shaped {
    font: FontId,
    glyph: u16,
    cluster: u32,
    /// Pixels.
    advance: f32,
    x_offset: f32,
    y_offset: f32,
    whitespace: bool,
}

struct Run {
    text: Range<usize>,
    font: FontId,
    script: Script,
    direction: Direction,
}

fn to_hb_script(s: unicode_script::Script) -> Script {
    let name = s.short_name().as_bytes();
    let tag = match name {
        [a, b, c, d] => harfrust::Tag::new(&[*a, *b, *c, *d]),
        _ => return harfrust::script::LATIN,
    };
    Script::from_iso15924_tag(tag).unwrap_or(harfrust::script::LATIN)
}

/// Script of `c`, with table-free answers for ASCII, Latin-1 and Cyrillic (most UI text).
#[inline]
fn script_of(c: char) -> unicode_script::Script {
    use unicode_script::Script as S;
    let u = c as u32;
    if u < 0x80 {
        if c.is_ascii_alphabetic() {
            S::Latin
        } else {
            S::Common
        }
    } else if (0xC0..0x250).contains(&u) && u != 0xD7 && u != 0xF7 {
        S::Latin
    } else if (0x400..0x530).contains(&u) {
        S::Cyrillic
    } else {
        c.script()
    }
}

/// A reusable text layout: call [`TextLayout::layout`] to lay out text, then read
/// [`TextLayout::glyphs`] and [`TextLayout::lines`]. Buffers are kept between calls, so
/// laying out text of similar size again does not allocate.
#[derive(Default)]
pub struct TextLayout {
    glyphs: Vec<PositionedGlyph>,
    lines: Vec<Line>,
    width: f32,
    height: f32,
    shaped: Vec<Shaped>,
    runs: Vec<Run>,
    /// Glyph index where a line may start after each break opportunity, with whether the
    /// break is mandatory.
    breaks: Vec<(usize, bool)>,
    /// (first glyph, end glyph, visible width) per line, during wrapping.
    line_ends: Vec<(usize, usize, f32)>,
    buffer: Option<UnicodeBuffer>,
    /// The paragraph's bidirectional levels.
    bidi: Bidi,
    /// A line's glyph levels and their visual order.
    levels: Vec<u8>,
    order: Vec<u32>,
}

impl core::fmt::Debug for TextLayout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TextLayout")
            .field("glyphs", &self.glyphs.len())
            .field("lines", &self.lines.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl TextLayout {
    /// An empty layout.
    pub fn new() -> Self {
        Self::default()
    }

    /// Positioned glyphs, line by line, each line's in visual order (left to right).
    pub fn glyphs(&self) -> &[PositionedGlyph] {
        &self.glyphs
    }

    /// Lines, top to bottom.
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// Width of the widest line (or `max_width` when aligning within it).
    pub fn width(&self) -> f32 {
        self.width
    }

    /// Total height of all lines.
    pub fn height(&self) -> f32 {
        self.height
    }

    /// Lays out `text` with the fonts in `stack` (first is primary; the others are
    /// fallbacks tried in order for characters the primary lacks). `\n` (and `\r\n`)
    /// starts a new line.
    pub fn layout(&mut self, fonts: &mut Fonts, stack: &[FontId], text: &str, style: &TextStyle) {
        self.glyphs.clear();
        self.lines.clear();
        self.width = 0.0;
        self.height = 0.0;
        let Some(primary_font) = stack.first().and_then(|&id| fonts.get(id)) else {
            return;
        };
        let scale = style.size / primary_font.units_per_em();
        let vm = primary_font.vertical_metrics();
        let ascent = vm.ascent * scale;
        let line_height = style
            .line_height
            .unwrap_or((vm.ascent + vm.descent + vm.line_gap) * scale);

        let base = match style.direction {
            TextDirection::Auto => None,
            TextDirection::LeftToRight => Some(0),
            TextDirection::RightToLeft => Some(1),
        };
        let mut baseline = ascent;
        let mut offset = 0;
        for paragraph in text.split('\n') {
            let body = paragraph.strip_suffix('\r').unwrap_or(paragraph);
            self.runs.clear();
            self.shaped.clear();
            self.breaks.clear();
            self.bidi.resolve(body, base);
            self.itemize(fonts, stack, body, offset);
            self.shape(fonts, text, style.size);
            self.find_breaks(body, offset);
            self.wrap(style.max_width.unwrap_or(f32::INFINITY));
            self.place(offset..offset + body.len(), &mut baseline, line_height);
            offset += paragraph.len() + 1;
        }

        let layout_width = style.max_width.unwrap_or(self.width);
        for line in &self.lines {
            // Start and end are the paragraph's: right and left for right-to-left text.
            let align = match (style.align, line.rtl) {
                (Align::Start, true) => Align::End,
                (Align::End, true) => Align::Start,
                (align, _) => align,
            };
            let shift = match align {
                Align::Start => 0.0,
                Align::Center => ((layout_width - line.width) * 0.5).max(0.0),
                Align::End => (layout_width - line.width).max(0.0),
            };
            if shift != 0.0 {
                for g in &mut self.glyphs[line.glyphs.clone()] {
                    g.x += shift;
                }
            }
        }
        if style.max_width.is_some() {
            self.width = layout_width;
        }
        self.height = self.lines.len() as f32 * line_height;
    }

    /// Splits a paragraph (at byte `offset` in the text) into runs of one font, script and
    /// bidirectional level (its parity the run's direction). Whitespace and characters of
    /// the Common and Inherited scripts join the current run when its font has them and
    /// their level is its; one that starts a run takes Latin at a left-to-right level, the
    /// previous run's script at a right-to-left one.
    fn itemize(&mut self, fonts: &Fonts, stack: &[FontId], text: &str, offset: usize) {
        let mut start = 0;
        let mut current: Option<(FontId, unicode_script::Script, u8)> = None;
        let mut hint = 0;
        for (i, c) in text.char_indices() {
            let level = self.bidi.level_at(i as u32, &mut hint);
            let char_script = script_of(c);
            let neutral = c.is_whitespace()
                || matches!(
                    char_script,
                    unicode_script::Script::Common | unicode_script::Script::Inherited
                );
            let has = |id: FontId| fonts.get(id).is_some_and(|f| f.glyph(c).is_some());
            // Neutral characters stay in the current run when its font has them; combining
            // marks always do (they belong to the cluster before them). Otherwise the first
            // font in the stack that has the character, and if none does, the current font
            // (shaping synthesizes missing space variants) or the primary.
            let font = match current {
                Some((f, _, _)) if char_script == unicode_script::Script::Inherited => f,
                Some((f, _, _)) if neutral && has(f) => f,
                _ => stack
                    .iter()
                    .copied()
                    .find(|&id| has(id))
                    .unwrap_or_else(|| {
                        current.map_or(stack[0], |(f, _, _)| if neutral { f } else { stack[0] })
                    }),
            };
            let script = match current {
                _ if !neutral => char_script,
                Some((_, s, l)) if l == level || level % 2 == 1 => s,
                _ => unicode_script::Script::Latin,
            };
            match current {
                Some((f, s, l)) if f == font && s == script && l == level => {}
                Some((f, s, l)) => {
                    self.push_run(offset + start..offset + i, f, s, l);
                    start = i;
                    current = Some((font, script, level));
                }
                None => {
                    start = i;
                    current = Some((font, script, level));
                }
            }
        }
        if let Some((font, script, level)) = current {
            self.push_run(offset + start..offset + text.len(), font, script, level);
        }
    }

    fn push_run(
        &mut self,
        text: Range<usize>,
        font: FontId,
        script: unicode_script::Script,
        level: u8,
    ) {
        if text.is_empty() {
            return;
        }
        self.runs.push(Run {
            text,
            font,
            script: to_hb_script(script),
            direction: if level % 2 == 1 {
                Direction::RightToLeft
            } else {
                Direction::LeftToRight
            },
        });
    }

    fn shape(&mut self, fonts: &mut Fonts, text: &str, size: f32) {
        let bytes = text.as_bytes();
        for run in &self.runs {
            let Some(font) = fonts.get_mut(run.font) else {
                continue;
            };
            let scale = size / font.units_per_em();
            let first = self.shaped.len();
            let shaped = &mut self.shaped;
            let id = run.font;
            let base = run.text.start as u32;
            font.shape(
                &mut self.buffer,
                &text[run.text.clone()],
                run.script,
                run.direction,
                |glyph, cluster, advance, x_offset, y_offset| {
                    let cluster = base + cluster;
                    shaped.push(Shaped {
                        font: id,
                        glyph,
                        cluster,
                        advance: advance as f32 * scale,
                        x_offset: x_offset as f32 * scale,
                        y_offset: y_offset as f32 * scale,
                        whitespace: matches!(bytes.get(cluster as usize), Some(b' ' | b'\t')),
                    });
                },
            );
            // Right-to-left runs come out in visual order; keep logical order for line
            // breaking, and reorder visually when placing.
            if run.direction == Direction::RightToLeft {
                self.shaped[first..].reverse();
            }
        }
    }

    /// Maps UAX #14 break opportunities in a paragraph to glyph indices where a line may
    /// start.
    fn find_breaks(&mut self, text: &str, offset: usize) {
        let mut g = 0;
        for (at, kind) in unicode_linebreak::linebreaks(text) {
            if kind == BreakOpportunity::Mandatory {
                // Only the end of the paragraph: newlines were split off already.
                continue;
            }
            while g < self.shaped.len() && (self.shaped[g].cluster as usize) < offset + at {
                g += 1;
            }
            self.breaks.push((g, false));
        }
    }

    /// Greedy wrapping of the paragraph's glyphs into `line_ends`. Whitespace at the end of
    /// a line hangs past the edge and does not count toward its width.
    fn wrap(&mut self, max: f32) {
        self.line_ends.clear();
        let n = self.shaped.len();
        let mut line_start = 0usize;
        let mut next_break = 0usize;
        let mut candidate: Option<(usize, f32)> = None;
        let mut x = 0.0f32;
        let mut visible = 0.0f32;
        let mut i = 0usize;
        loop {
            while next_break < self.breaks.len() && self.breaks[next_break].0 <= i {
                if self.breaks[next_break].0 == i && i > line_start {
                    candidate = Some((i, visible));
                }
                next_break += 1;
            }
            if i >= n {
                self.line_ends.push((line_start, n, visible));
                return;
            }
            let g = self.shaped[i];
            let after = x + g.advance;
            if !g.whitespace && after > max && i > line_start {
                // Wrap at the last opportunity, or force a break inside a word longer
                // than the line.
                let (end, width) = match candidate {
                    Some((end, width)) if end > line_start => (end, width),
                    _ => (i, visible),
                };
                self.line_ends.push((line_start, end, width));
                line_start = end;
                candidate = None;
                x = 0.0;
                visible = 0.0;
                for s in &self.shaped[end..i] {
                    x += s.advance;
                    if !s.whitespace {
                        visible = x;
                    }
                }
                continue;
            }
            x = after;
            if !g.whitespace {
                visible = x;
            }
            i += 1;
        }
    }

    /// Places the paragraph's lines (`line_ends`) at successive baselines, each in visual
    /// order: its levels reset at its end (L1), its glyphs reordered by level (L2).
    fn place(&mut self, paragraph: Range<usize>, baseline: &mut f32, line_height: f32) {
        let para = self.bidi.level();
        let rtl = para % 2 == 1;
        for k in 0..self.line_ends.len() {
            let (start, end, width) = self.line_ends[k];
            let first = self.glyphs.len();
            let text_start = self
                .shaped
                .get(start)
                .map_or(paragraph.end, |s| s.cluster as usize);
            let text_end = self
                .shaped
                .get(end)
                .map_or(paragraph.end, |s| s.cluster as usize);
            let mut pen = 0.0;
            if self.bidi.mixed() {
                let local = |b: usize| (b - paragraph.start) as u32;
                let line_end = text_end.max(text_start).min(paragraph.end);
                self.bidi
                    .reset_line(local(text_start.min(paragraph.end))..local(line_end), para);
                self.levels.clear();
                let mut hint = 0;
                let mut total = 0.0;
                for s in &self.shaped[start..end] {
                    let at = (s.cluster as usize).saturating_sub(paragraph.start) as u32;
                    self.levels.push(self.bidi.level_at(at, &mut hint));
                    total += s.advance;
                }
                visual_order(&self.levels, &mut self.order);
                // A right-to-left line's trailing whitespace is now at its left: it hangs
                // past the start of the line.
                if rtl {
                    pen = -(total - width);
                }
                for &o in &self.order {
                    let s = self.shaped[start + o as usize];
                    self.glyphs.push(PositionedGlyph {
                        font: s.font,
                        glyph: s.glyph,
                        x: pen + s.x_offset,
                        y: *baseline - s.y_offset,
                        cluster: s.cluster,
                    });
                    pen += s.advance;
                }
            } else {
                for s in &self.shaped[start..end] {
                    self.glyphs.push(PositionedGlyph {
                        font: s.font,
                        glyph: s.glyph,
                        x: pen + s.x_offset,
                        y: *baseline - s.y_offset,
                        cluster: s.cluster,
                    });
                    pen += s.advance;
                }
            }
            self.lines.push(Line {
                glyphs: first..self.glyphs.len(),
                text: text_start.min(paragraph.end)..text_end.max(text_start),
                width,
                baseline: *baseline,
                rtl,
            });
            self.width = self.width.max(width);
            *baseline += line_height;
        }
    }
}
