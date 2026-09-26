//! Text: fonts, shaping, line breaking, layout, glyph rasterization and atlas (ADR 0025).
//!
//! - [`Fonts`] holds fonts loaded from bytes ([`Font`]).
//! - [`TextLayout`] shapes text with HarfRust, breaks lines by UAX #14 and wraps it
//!   (whitespace at line ends hangs), with fallback fonts and alignment. It keeps its
//!   buffers, so laying text out again does not allocate.
//! - [`GlyphAtlas`] renders glyphs with the built-in coverage rasterizer and packs them
//!   into an 8-bit texture, keyed by size and quarter-pixel x offset.
//!
//! ```
//! use engine_text::{Fonts, GlyphAtlas, GlyphKey, TextLayout, TextStyle};
//!
//! let mut fonts = Fonts::new();
//! let go = fonts.load(engine_text::fonts::GO_REGULAR, 0).unwrap();
//! let mut layout = TextLayout::new();
//! let style = TextStyle { size: 16.0, max_width: Some(120.0), ..TextStyle::default() };
//! layout.layout(&mut fonts, &[go], "Hello, world! Привет, мир!", &style);
//!
//! let mut atlas = GlyphAtlas::new(512, 512);
//! for g in layout.glyphs() {
//!     let (key, x) = GlyphKey::at(g.font, g.glyph, style.size, g.x);
//!     if let Some(a) = atlas.get(&fonts, key) {
//!         // Draw atlas rect (a.x, a.y, a.width, a.height) at (x + a.left, g.y - a.top).
//!         let _ = (x, a);
//!     }
//! }
//! assert!(layout.lines().len() >= 2);
//! ```

mod atlas;
mod font;
mod layout;
mod raster;

pub use atlas::{AtlasGlyph, GlyphAtlas, GlyphKey, Rect, SUBPIXEL_STEPS};
pub use font::{Font, FontError, FontId, Fonts, VerticalMetrics};
pub use layout::{Align, Line, PositionedGlyph, TextLayout, TextStyle};
pub use raster::{GlyphBitmap, Rasterizer, rasterize};

/// Fonts bundled with the engine (`third_party/fonts`; licenses and credits in
/// `THIRD_PARTY_NOTICES.md`, policy in ADR 0036). A font stack of a text font followed by
/// [`NERD_SYMBOLS`](fonts::NERD_SYMBOLS) draws text with the Nerd Fonts icons.
pub mod fonts {
    /// Noto Sans 2.015 Regular (The Noto Project Authors, OFL-1.1): the default UI font.
    /// Latin, Greek and Cyrillic, including Vietnamese and extended Latin.
    pub static NOTO_SANS: &[u8] =
        include_bytes!("../../../third_party/fonts/noto-sans/NotoSans-Regular.ttf");
    /// JetBrains Mono 2.305 Regular (The JetBrains Mono Project Authors, OFL-1.1): code,
    /// consoles, aligned numbers.
    pub static JETBRAINS_MONO: &[u8] =
        include_bytes!("../../../third_party/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf");
    /// Symbols Nerd Font (Nerd Fonts 3.5.1): the Nerd Fonts icons (Material Design,
    /// Codicons, Font Awesome, Octicons, Devicons…) at their usual code points, for use as
    /// a fallback after a text font. Icon sets under MIT, Apache-2.0, CC BY 4.0, OFL-1.1
    /// and the Unlicense.
    pub static NERD_SYMBOLS: &[u8] = include_bytes!(
        "../../../third_party/fonts/nerd-fonts-symbols/SymbolsNerdFont-Regular.ttf"
    );
    /// Go Regular (Bigelow & Holmes, BSD-3-Clause): Latin, Cyrillic, Greek. Compact; used
    /// by tests and benchmarks.
    pub static GO_REGULAR: &[u8] = include_bytes!("../../../third_party/fonts/go/Go-Regular.ttf");
    /// Go Bold.
    pub static GO_BOLD: &[u8] = include_bytes!("../../../third_party/fonts/go/Go-Bold.ttf");
    /// Go Mono: monospaced.
    pub static GO_MONO: &[u8] = include_bytes!("../../../third_party/fonts/go/Go-Mono.ttf");
}

#[cfg(test)]
mod tests;
