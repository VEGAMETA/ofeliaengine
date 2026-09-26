//! Fonts: loaded once from bytes, shared by layout and rasterization.

use core::fmt;
use harfrust::{Direction, FontRef, Script, ShapePlan, ShaperData};
use skrifa::MetadataProvider;
use skrifa::instance::{LocationRef, Size};
use std::sync::Arc;

/// A font in a [`Fonts`] collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FontId(pub(crate) u16);

/// Why a font could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FontError {
    /// Not a TrueType / OpenType font, or no face at that index.
    Invalid,
    /// More than 65,535 fonts.
    TooMany,
}

impl fmt::Display for FontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid => f.write_str("not a TrueType/OpenType font (or no face at that index)"),
            Self::TooMany => f.write_str("too many fonts"),
        }
    }
}

impl std::error::Error for FontError {}

/// Vertical metrics in font units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerticalMetrics {
    /// Above the baseline (positive).
    pub ascent: f32,
    /// Below the baseline (positive).
    pub descent: f32,
    /// Extra space between lines.
    pub line_gap: f32,
}

/// One font face.
pub struct Font {
    data: Arc<[u8]>,
    index: u32,
    shaper: ShaperData,
    /// Shape plans by (script, direction), built on first use.
    plans: Vec<(Script, Direction, ShapePlan)>,
    units_per_em: f32,
    metrics: VerticalMetrics,
    /// Glyph ids of the Basic Multilingual Plane (0 = missing), built once at load: 128 KiB
    /// that turn every lookup outside ASCII into an array read instead of a charmap search.
    bmp: Box<[u16]>,
}

impl fmt::Debug for Font {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Font")
            .field("bytes", &self.data.len())
            .field("index", &self.index)
            .field("units_per_em", &self.units_per_em)
            .finish()
    }
}

impl Font {
    /// Loads face `index` of a TrueType / OpenType font or collection.
    pub fn from_bytes(data: impl Into<Arc<[u8]>>, index: u32) -> Result<Self, FontError> {
        let data: Arc<[u8]> = data.into();
        let font = FontRef::from_index(&data, index).map_err(|_| FontError::Invalid)?;
        let shaper = ShaperData::new(&font);
        let m = font.metrics(Size::unscaled(), LocationRef::default());
        let mut bmp = vec![0u16; 0x1_0000].into_boxed_slice();
        for (c, g) in font.charmap().mappings() {
            if let (Some(slot), Ok(g)) = (bmp.get_mut(c as usize), u16::try_from(g.to_u32())) {
                *slot = g;
            }
        }
        Ok(Self {
            shaper,
            plans: Vec::new(),
            units_per_em: f32::from(m.units_per_em.max(1)),
            metrics: VerticalMetrics {
                ascent: m.ascent,
                descent: -m.descent,
                line_gap: m.leading,
            },
            bmp,
            index,
            data,
        })
    }

    /// The font's table data, parsed on demand (cheap: a table directory lookup).
    pub(crate) fn font_ref(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(&self.data, self.index).ok()
    }

    /// Font units per em.
    pub fn units_per_em(&self) -> f32 {
        self.units_per_em
    }

    /// Ascent, descent and line gap in font units.
    pub fn vertical_metrics(&self) -> VerticalMetrics {
        self.metrics
    }

    /// The glyph for `c`, if the font has one.
    #[inline]
    pub fn glyph(&self, c: char) -> Option<u16> {
        let g = if (c as u32) < 0x1_0000 {
            self.bmp[c as usize]
        } else {
            self.font_ref()?
                .charmap()
                .map(c)
                .map_or(0, |g| u16::try_from(g.to_u32()).unwrap_or(0))
        };
        (g != 0).then_some(g)
    }

    /// Shapes `text` with the plan for `script` and `direction`, appending to `out` (glyph
    /// id, cluster byte offset within `text`, advance and offsets in font units).
    pub(crate) fn shape(
        &mut self,
        buffer: &mut Option<harfrust::UnicodeBuffer>,
        text: &str,
        script: Script,
        direction: Direction,
        mut out: impl FnMut(u16, u32, i32, i32, i32),
    ) {
        let Ok(font) = FontRef::from_index(&self.data, self.index) else {
            return;
        };
        let plan_index = match self
            .plans
            .iter()
            .position(|(s, d, _)| *s == script && *d == direction)
        {
            Some(i) => i,
            None => {
                let shaper = self.shaper.shaper(&font).build();
                let plan = ShapePlan::new(&shaper, direction, Some(script), None, &[]);
                self.plans.push((script, direction, plan));
                self.plans.len() - 1
            }
        };
        let shaper = self.shaper.shaper(&font).build();
        let mut unicode = buffer.take().unwrap_or_default();
        unicode.push_str(text);
        unicode.set_direction(direction);
        unicode.set_script(script);
        let plan = &self.plans[plan_index].2;
        let glyphs = shaper.shape(unicode, harfrust::ShapeOptions::new().plan(Some(plan)));
        for (info, pos) in glyphs.glyph_infos().iter().zip(glyphs.glyph_positions()) {
            out(
                info.glyph_id as u16,
                info.cluster,
                pos.x_advance,
                pos.x_offset,
                pos.y_offset,
            );
        }
        *buffer = Some(glyphs.clear());
    }
}

/// The fonts an application uses. Layouts refer to fonts by [`FontId`].
#[derive(Debug, Default)]
pub struct Fonts {
    fonts: Vec<Font>,
}

impl Fonts {
    /// An empty collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a font.
    pub fn add(&mut self, font: Font) -> Result<FontId, FontError> {
        let id = u16::try_from(self.fonts.len()).map_err(|_| FontError::TooMany)?;
        self.fonts.push(font);
        Ok(FontId(id))
    }

    /// Loads and adds face `index` of a font file's bytes.
    pub fn load(&mut self, data: impl Into<Arc<[u8]>>, index: u32) -> Result<FontId, FontError> {
        self.add(Font::from_bytes(data, index)?)
    }

    /// The font with `id`.
    pub fn get(&self, id: FontId) -> Option<&Font> {
        self.fonts.get(usize::from(id.0))
    }

    pub(crate) fn get_mut(&mut self, id: FontId) -> Option<&mut Font> {
        self.fonts.get_mut(usize::from(id.0))
    }
}
