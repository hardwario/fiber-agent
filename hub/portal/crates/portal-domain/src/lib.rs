//! Domain layer: alarm engine, retention, gap detection, audit — pure logic
//! behind traits, no dependency on axum/rusqlite/minijinja/tokio
//! (plan/viewer/stack.md layering). Implemented so far: the alarm engine
//! (M6-A) and the Node threshold precedence rule; retention/gap-detection/
//! audit land later.

pub mod alarm_engine;
pub mod thresholds;
pub mod types;

pub use alarm_engine::{
    ack_validation, held_field_state, held_wired_state, on_connectivity, on_field_states,
    on_native_event, on_wired_snapshot, on_wired_transition,
};
pub use thresholds::{merge_node_thresholds, EffectiveThreshold};
pub use types::{
    is_node_sensor_id, node_sensor_prefix, Activation, ActiveAlarm, ActiveLookup, AlarmAction,
    AlarmSource, AlarmType, ConnectivityKind, ConnectivitySnapshot, Edge, FieldSource, FieldStates,
    NativeEvent, NewAlarm, NotifyKind, NotifyPayload, ParseWireStateError, SessionState, Severity,
    ThresholdBounds, Transport, WireState, WiredSnapshot, WiredTransition, NODE_SENSOR_ID_TAG,
};
