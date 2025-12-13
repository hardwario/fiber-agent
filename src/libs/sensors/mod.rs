// Temperature sensor reading and monitoring module

pub mod reader;
pub mod status;
pub mod monitor;
pub mod state;
pub mod aggregation;

// Re-export key types for convenience
pub use monitor::SensorMonitor;
pub use state::{SensorReading, SharedSensorState, SharedSensorStateHandle, create_shared_sensor_state};
pub use aggregation::{AggregationState, AggregationPeriod, SensorAggregation, AlarmStateCounts};
