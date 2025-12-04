// Accelerometer motion detection module

pub mod state;
pub mod monitor;

// Re-export key types for convenience
pub use state::{MotionDetector, MotionState};
pub use monitor::AccelerometerMonitor;
