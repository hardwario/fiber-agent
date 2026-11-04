//! Factory test of the front panel and radios, driven over SSH by the
//! provisioning station (`fiberctl factory-test run`). While it runs it owns
//! the LCD, the buttons, the buzzer and the LEDs.

pub mod machine;
pub mod radios;
pub mod render;
pub mod runtime;
pub mod scan;

pub use machine::Btn;
pub use render::{draw, Frame};
pub use runtime::{
    fail, frame, is_active, on_press, owns_leds, register, start, status, stop, take_leds_dirty,
    Hardware,
};
