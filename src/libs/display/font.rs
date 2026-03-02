//! Custom font based on ProFont 9-point with patched '%' glyph

use embedded_graphics::{
    image::ImageRaw,
    mono_font::{mapping::StrGlyphMapping, DecorationDimensions, MonoFont},
    geometry::Size,
};

const CHARS_PER_ROW: u32 = 32;

const GLYPH_MAPPING: StrGlyphMapping =
    StrGlyphMapping::new("\0 ~\0\u{00A0}\u{00FF}", '?' as usize - ' ' as usize);

/// ProFont 9-point with corrected '%' glyph
pub const PROFONT_9_POINT: MonoFont = MonoFont {
    image: ImageRaw::new(
        include_bytes!("ProFont9Point.raw"),
        CHARS_PER_ROW * 6,
    ),
    character_size: Size::new(6, 11),
    character_spacing: 0,
    baseline: 8,
    underline: DecorationDimensions::new(10, 1),
    strikethrough: DecorationDimensions::new(6, 1),
    glyph_mapping: &GLYPH_MAPPING,
};
