// Accelerometer motion detection module

pub mod monitor;
pub mod state;

// Re-export key types for convenience
pub use monitor::AccelerometerMonitor;
pub use state::{MotionDetector, MotionState};
