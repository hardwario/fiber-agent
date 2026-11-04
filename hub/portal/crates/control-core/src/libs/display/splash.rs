//! Boot splash screen — the build's brand logo, drawn once at startup.
//!
//! Rendered by the display monitor between St7920 init and the main
//! render loop, then dwelled on for SPLASH_DURATION before normal
//! screens take over. The bitmap matches the panel's native 128×64
//! geometry so it can be drawn 1:1 without scaling.

use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle};

/// How long the splash stays on screen before the normal display loop
/// takes over. Long enough for the user to register the logo, short
/// enough not to block boot-time UX.
pub const SPLASH_DURATION: std::time::Duration = std::time::Duration::from_secs(2);

/// The build's brand splash — 128×64, 1 = lit, 0 = off — from
/// `portal/branding/<brand>/splash.pbm` (plan/portal/branding.md §2). Same
/// unpacked convention as the icons in `icons.rs` (~8 KB rodata).
///
/// GENERATED, not hand-drawn. ProXimos' is rasterised from its logo.svg by
/// `scripts/gen_splash_bitmap.py` — the same asset the web portal serves as its
/// logo — re-run it instead of editing the PBM, or the panel and the browser
/// drift apart. Only the wordmark is taken: the lockup's baseline rule and the
/// small "lite" beneath it land at 2 and 5 px at this width, which a 1-bit
/// panel renders as a smear. FIBER's comes from
/// `portal/branding/scripts/gen-fiber-assets.py`.
static BRAND_BITMAP: &[[u8; 128]; 64] = &proximos_brand::SPLASH;

/// Draw the brand's logo to the given display target.
///
/// Uses the same row-scan horizontal-run line approach as
/// `icons::draw_wifi_bitmap` to avoid emitting one Pixel per lit cell.
pub fn render_splash<D>(display: &mut D)
where
    D: DrawTarget<Color = BinaryColor>,
    D::Error: core::fmt::Debug,
{
    let style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

    for (y, row) in BRAND_BITMAP.iter().enumerate() {
        let mut run_start: Option<usize> = None;
        for (x, &pix) in row.iter().enumerate() {
            match (run_start, pix != 0) {
                (None, true) => run_start = Some(x),
                (Some(start), false) => {
                    let _ = Line::new(
                        Point::new(start as i32, y as i32),
                        Point::new(x as i32 - 1, y as i32),
                    )
                    .into_styled(style)
                    .draw(display);
                    run_start = None;
                }
                _ => {}
            }
        }
        if let Some(start) = run_start {
            let _ = Line::new(
                Point::new(start as i32, y as i32),
                Point::new(row.len() as i32 - 1, y as i32),
            )
            .into_styled(style)
            .draw(display);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::Pixel;

    /// A 128×64 frame that records what was lit.
    struct Frame([[u8; 128]; 64]);

    impl OriginDimensions for Frame {
        fn size(&self) -> Size {
            Size::new(128, 64)
        }
    }

    impl DrawTarget for Frame {
        type Color = BinaryColor;
        type Error = core::convert::Infallible;
        fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
        where
            I: IntoIterator<Item = Pixel<Self::Color>>,
        {
            for Pixel(p, c) in pixels {
                if (0..128).contains(&p.x) && (0..64).contains(&p.y) && c == BinaryColor::On {
                    self.0[p.y as usize][p.x as usize] = 1;
                }
            }
            Ok(())
        }
    }

    /// The panel shows exactly the brand's bitmap: the run-length drawing loses no pixel and
    /// adds none.
    #[test]
    fn the_splash_draws_the_brand_bitmap_exactly() {
        let mut frame = Frame([[0; 128]; 64]);
        render_splash(&mut frame);
        assert_eq!(&frame.0, BRAND_BITMAP);
    }
}
