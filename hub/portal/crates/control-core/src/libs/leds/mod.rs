// LED control module - centralized LED management with dedicated thread

pub mod monitor;
pub mod state;

pub use monitor::LedMonitor;
pub use state::{LineLedState, SharedLedState, SharedLedStateHandle};
