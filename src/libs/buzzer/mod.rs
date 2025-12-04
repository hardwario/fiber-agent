//! Independent buzzer control library
//!
//! Provides a modular, thread-safe buzzer controller that can be used
//! independently from sensor monitoring. Manages buzzer patterns and
//! timing via a dedicated control thread.
//!
//! ## Usage
//!
//! ```ignore
//! use fiber_app::libs::buzzer::BuzzerController;
//! use fiber_app::libs::buzzer::BuzzerPattern;
//!
//! // Create controller
//! let buzzer = BuzzerController::new()?;
//!
//! // Set continuous alarm pattern
//! buzzer.set_repeating_pattern(pattern);
//!
//! // Or play a celebratory pattern once
//! buzzer.play_once(BuzzerPattern::ReconnectionHappy { frequency_hz: 150 });
//!
//! // Stop buzzer
//! buzzer.stop();
//!
//! // Shutdown
//! buzzer.shutdown()?;
//! ```

pub mod controller;
pub mod pattern;
pub mod priority;
mod thread;

pub use controller::BuzzerController;
pub use pattern::{BuzzerPattern, SharedBuzzerState};
pub use priority::BuzzerPriorityManager;
