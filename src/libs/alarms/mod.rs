//! Alarm library - Handles threshold-based alarms with state machines, LED control, and callbacks
//!
//! This library provides a reusable alarm system that can be used for sensors, power monitoring,
//! motion detection, and other threshold-based monitoring. It supports:
//! - Multiple alarm states (normal, warning, alarm, critical)
//! - Configurable thresholds with per-line overrides
//! - LED color control with blink patterns (orange via simultaneous red+green)
//! - Event callbacks for logging, alerts, and custom actions
//! - State machine with reconnection handling

pub mod callbacks;
pub mod color;
pub mod controller;
pub mod state;
pub mod threshold;

pub use callbacks::{AlarmCallback, AlarmEvent, LoggingCallback, BuzzerCallback, BuzzerStateCallback, BeepPattern};
pub use color::{BlinkPattern, LedColor};
pub use controller::AlarmController;
pub use state::AlarmState;
pub use threshold::AlarmThreshold;
