//! What the LCD shows during the factory test. Drawn into any
//! `DrawTarget<BinaryColor>` so it is testable without the panel.

use embedded_graphics::{
    mono_font::MonoTextStyle,
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Alignment, Text},
};

use super::machine::Pattern;
use crate::libs::display::font::PROFONT_9_POINT;

pub const WIDTH: u32 = 128;
pub const HEIGHT: u32 = 64;

/// One frame of the factory test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A display test pattern; `phase` toggles the blink.
    Pattern { pattern: Pattern, phase: bool },
    /// Up to five centred lines of text, the first one as a title.
    Text(Vec<String>),
}

pub fn draw<D: DrawTarget<Color = BinaryColor>>(target: &mut D, frame: &Frame) {
    let _ = target.clear(BinaryColor::Off);
    match frame {
        Frame::Pattern { pattern, phase } => match pattern {
            Pattern::Blink => {
                if *phase {
                    let _ = target.clear(BinaryColor::On);
                }
            }
            Pattern::AllOn => {
                let _ = target.clear(BinaryColor::On);
            }
            Pattern::Checker => {
                let pixels = (0..HEIGHT as i32).flat_map(|y| {
                    (0..WIDTH as i32).filter_map(move |x| {
                        ((x + y) % 2 == 0).then_some(Pixel(Point::new(x, y), BinaryColor::On))
                    })
                });
                let _ = target.draw_iter(pixels);
            }
        },
        Frame::Text(lines) => {
            let style = MonoTextStyle::new(&PROFONT_9_POINT, BinaryColor::On);
            for (i, line) in lines.iter().take(5).enumerate() {
                let y = 10 + 12 * i as i32;
                let _ = Text::with_alignment(line, Point::new(64, y), style, Alignment::Center)
                    .draw(target);
                if i == 0 {
                    let _ = Rectangle::new(Point::new(0, y + 2), Size::new(WIDTH, 1))
                        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
                        .draw(target);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::mock_display::MockDisplay;

    fn lit(frame: &Frame) -> usize {
        let mut d: MockDisplay<BinaryColor> = MockDisplay::new();
        d.set_allow_overdraw(true);
        d.set_allow_out_of_bounds_drawing(true);
        // MockDisplay is 64x64; enough to tell the patterns apart.
        draw(&mut d, frame);
        (0..64)
            .flat_map(|y| (0..64).map(move |x| Point::new(x, y)))
            .filter(|p| d.get_pixel(*p) == Some(BinaryColor::On))
            .count()
    }

    #[test]
    fn patterns_light_all_half_or_nothing() {
        assert_eq!(
            lit(&Frame::Pattern {
                pattern: Pattern::AllOn,
                phase: false
            }),
            64 * 64
        );
        assert_eq!(
            lit(&Frame::Pattern {
                pattern: Pattern::Checker,
                phase: false
            }),
            64 * 64 / 2
        );
        assert_eq!(
            lit(&Frame::Pattern {
                pattern: Pattern::Blink,
                phase: false
            }),
            0
        );
        assert_eq!(
            lit(&Frame::Pattern {
                pattern: Pattern::Blink,
                phase: true
            }),
            64 * 64
        );
    }

    #[test]
    fn text_draws_something() {
        assert!(lit(&Frame::Text(vec!["BUTTONS".into(), "Press UP".into()])) > 0);
    }
}
