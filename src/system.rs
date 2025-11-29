// src/system.rs
use crate::acquisition::{AcquisitionConfig, AcquisitionEngine, SensorBackend};
use crate::alarms::engine::{AlarmConfig, AlarmEvent, SensorAlarm};
use crate::model::{SensorId, SensorReading};
use chrono::{DateTime, Utc};

/// Output of a single node tick: optionally a reading and/or an alarm event.
#[derive(Debug)]
pub struct NodeOutput {
    pub reading: Option<SensorReading>,
    pub alarm_event: Option<AlarmEvent>,
}

/// Trait for "one sensor pipeline":
/// acquisition + alarm logic bundled together.
pub trait SensorNode {
    fn sensor_id(&self) -> SensorId;

    /// Tick this node with the current time.
    ///
    /// - May return a new SensorReading if the acquisition interval elapsed.
    /// - May return an AlarmEvent if the alarm state changed.
    fn tick(&mut self, now: DateTime<Utc>) -> NodeOutput;
}

/// Generic implementation of a sensor node using:
/// - AcquisitionEngine<B>
/// - SensorAlarm
pub struct GenericSensorNode<B: SensorBackend> {
    acquisition: AcquisitionEngine<B>,
    alarm: SensorAlarm,
}

impl<B: SensorBackend> GenericSensorNode<B> {
    pub fn new(backend: B, acq_cfg: AcquisitionConfig, alarm_cfg: AlarmConfig) -> Self {
        let sensor_id = backend.sensor_id();
        let acquisition = AcquisitionEngine::new(backend, acq_cfg);
        let alarm = SensorAlarm::new(sensor_id, alarm_cfg);
        Self { acquisition, alarm }
    }
}

impl<B: SensorBackend + 'static> SensorNode for GenericSensorNode<B> {
    fn sensor_id(&self) -> SensorId {
        self.acquisition.sensor_id()
    }

    fn tick(&mut self, now: DateTime<Utc>) -> NodeOutput {
        let reading = self.acquisition.tick(now);
        let mut alarm_event = None;

        if let Some(ref r) = reading {
            if let Some(ev) = self.alarm.apply_reading(r) {
                alarm_event = Some(ev);
            }
        }

        NodeOutput {
            reading,
            alarm_event,
        }
    }
}

/// Result of ticking the whole system at a given time.
#[derive(Debug)]
pub struct SystemTickResult {
    pub readings: Vec<SensorReading>,
    pub alarm_events: Vec<AlarmEvent>,
}

/// Multi-sensor orchestrator.
/// Holds a list of nodes and fans in/out the calls.
pub struct SensorSystem {
    nodes: Vec<Box<dyn SensorNode>>,
}

impl SensorSystem {
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    pub fn add_node(&mut self, node: Box<dyn SensorNode>) {
        self.nodes.push(node);
    }

    /// Tick all sensor nodes with the same "now".
    ///
    /// Returns all readings + alarm events produced in this tick.
    pub fn tick(&mut self, now: DateTime<Utc>) -> SystemTickResult {
        let mut readings = Vec::new();
        let mut alarm_events = Vec::new();

        for node in self.nodes.iter_mut() {
            let out = node.tick(now);
            if let Some(r) = out.reading {
                readings.push(r);
            }
            if let Some(ev) = out.alarm_event {
                alarm_events.push(ev);
            }
        }

        SystemTickResult {
            readings,
            alarm_events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acquisition::{AcquisitionConfig, SensorBackend};
    use crate::alarms::engine::{AlarmConfig, AlarmReason, AlarmState};
    use crate::model::{ReadingQuality, SensorReading};
    use chrono::{TimeZone, Utc};

    #[derive(Debug)]
    struct FakeBackend {
        id: SensorId,
        values: Vec<f32>,
        index: usize,
        quality: ReadingQuality,
    }

    impl FakeBackend {
        fn new(id: SensorId, values: Vec<f32>) -> Self {
            Self {
                id,
                values,
                index: 0,
                quality: ReadingQuality::Ok,
            }
        }
    }

    impl SensorBackend for FakeBackend {
        fn sensor_id(&self) -> SensorId {
            self.id
        }

        fn read(&mut self, _now: DateTime<Utc>) -> (f32, ReadingQuality) {
            let v = if self.index < self.values.len() {
                let v = self.values[self.index];
                self.index += 1;
                v
            } else {
                self.values.last().copied().unwrap_or(0.0)
            };
            (v, self.quality)
        }
    }

    fn t(ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(ms).unwrap()
    }

    #[test]
    fn multi_sensor_acquisition_respects_interval_per_sensor() {
        let acq_cfg = AcquisitionConfig::hundred_per_minute();

        // Sensor 1: values 1.0, 2.0, 3.0...
        let backend1 = FakeBackend::new(SensorId(1), vec![1.0, 2.0, 3.0]);
        // Sensor 2: values 10.0, 20.0, 30.0...
        let backend2 = FakeBackend::new(SensorId(2), vec![10.0, 20.0, 30.0]);

        let alarm_cfg = AlarmConfig::default();

        let node1 = Box::new(GenericSensorNode::new(backend1, acq_cfg, alarm_cfg));
        let node2 = Box::new(GenericSensorNode::new(backend2, acq_cfg, alarm_cfg));

        let mut system = SensorSystem::new();
        system.add_node(node1);
        system.add_node(node2);

        let mut all_readings: Vec<SensorReading> = Vec::new();

        // Step time every 200ms for 6 steps
        // We expect samples at 0ms and 600ms for each sensor.
        for step in 0..6 {
            let now = t(step * 200);
            let result = system.tick(now);
            all_readings.extend(result.readings);
        }

        // Expect 4 readings total: 2 sensors * 2 samples.
        assert_eq!(all_readings.len(), 4);

        // Sort by (sensor_id, ts_utc) for predictable checks
        all_readings.sort_by_key(|r| (r.sensor_id.0, r.ts_utc));

        // Sensor 1 at 0ms: 1.0, at 600ms: 2.0
        assert_eq!(all_readings[0].sensor_id, SensorId(1));
        assert_eq!(all_readings[0].ts_utc, t(0));
        assert_eq!(all_readings[0].value, 1.0);

        assert_eq!(all_readings[1].sensor_id, SensorId(1));
        assert_eq!(all_readings[1].ts_utc, t(600));
        assert_eq!(all_readings[1].value, 2.0);

        // Sensor 2 at 0ms: 10.0, at 600ms: 20.0
        assert_eq!(all_readings[2].sensor_id, SensorId(2));
        assert_eq!(all_readings[2].ts_utc, t(0));
        assert_eq!(all_readings[2].value, 10.0);

        assert_eq!(all_readings[3].sensor_id, SensorId(2));
        assert_eq!(all_readings[3].ts_utc, t(600));
        assert_eq!(all_readings[3].value, 20.0);
    }

    #[test]
    fn per_sensor_alarm_logic_is_independent() {
        let acq_cfg = AcquisitionConfig::hundred_per_minute();

        // Sensor 1: will cross warning_high 30.0
        let backend1 = FakeBackend::new(SensorId(1), vec![25.0, 32.0, 35.0]);
        // Sensor 2: stays below threshold
        let backend2 = FakeBackend::new(SensorId(2), vec![20.0, 22.0, 23.0]);

        let alarm_cfg = AlarmConfig {
            warning_low: None,
            warning_high: Some(30.0),
            critical_low: None,
            critical_high: None,
            hysteresis: 0.5,
        };

        let node1 = Box::new(GenericSensorNode::new(backend1, acq_cfg, alarm_cfg));
        let node2 = Box::new(GenericSensorNode::new(backend2, acq_cfg, alarm_cfg));

        let mut system = SensorSystem::new();
        system.add_node(node1);
        system.add_node(node2);

        let mut events: Vec<AlarmEvent> = Vec::new();

        let mut now = t(0);
        for _ in 0..4 {
            let result = system.tick(now);
            events.extend(result.alarm_events);
            now = now + acq_cfg.sample_interval;
        }

        // We expect at least one event for sensor 1 entering WARNING.
        assert!(events.iter().any(|e| {
            e.sensor_id == SensorId(1)
                && e.from == AlarmState::Normal
                && e.to == AlarmState::Warning
                && e.reason == AlarmReason::ThresholdHigh
        }));

        // We do NOT expect any events for sensor 2 (always below threshold)
        assert!(!events.iter().any(|e| e.sensor_id == SensorId(2)));
    }
}
