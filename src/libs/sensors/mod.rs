// Temperature sensor reading and monitoring module

pub mod aggregation;
pub mod monitor;
pub mod reader;
pub mod state;
pub mod status;

// Re-export key types for convenience
pub use aggregation::{AggregationPeriod, AggregationState, AlarmStateCounts, SensorAggregation};
pub use monitor::SensorMonitor;
pub use state::{
    create_shared_sensor_state, SensorReading, SharedSensorState, SharedSensorStateHandle,
};
