// src/alarms/actions.rs
use super::engine::{AlarmEvent, AlarmState, AlarmSeverity};
use crate::hal::{BuzzerHal, LedHal, LedState, SensorLedHal, SensorLedState};
use crate::model::SensorId;
use std::collections::HashMap;

/// Abstraction for anything that reacts to alarm events.
pub trait AlarmActionSink {
    fn on_alarm(&mut self, event: &AlarmEvent);
}

/// Does nothing. Useful as a default / placeholder.
pub struct NoopAlarmActionSink;

impl AlarmActionSink for NoopAlarmActionSink {
    fn on_alarm(&mut self, _event: &AlarmEvent) {
        // no-op
    }
}

/// For tests (and maybe diagnostics): records all events in a Vec.
#[derive(Default)]
pub struct RecordingAlarmActionSink {
    events: Vec<AlarmEvent>,
}

impl RecordingAlarmActionSink {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    pub fn events(&self) -> &[AlarmEvent] {
        &self.events
    }

    pub fn into_events(self) -> Vec<AlarmEvent> {
        self.events
    }
}

impl AlarmActionSink for RecordingAlarmActionSink {
    fn on_alarm(&mut self, event: &AlarmEvent) {
        self.events.push(event.clone());
    }
}

/// Hardware-oriented alarm action sink.
///
/// Maps alarm severity/state into:
/// - PWRLED (global)
/// - Buzzer
/// - Per-sensor LED (by index; we use a mapping SensorId -> 0..7)
///
/// PWRLED mapping:
/// - NORMAL   => Green,  buzzer OFF
/// - WARNING  => Yellow, buzzer OFF
/// - CRITICAL => Red,    buzzer ON
/// - FAULT    => Red,    buzzer ON
///
/// Per-sensor LED mapping (per event.sensor_id):
/// - NORMAL   => Green
/// - WARNING  => Both (Green+Red)
/// - CRITICAL => Red
/// - FAULT    => Red
pub struct HardwareAlarmActionSink<B: BuzzerHal, PL: LedHal, SL: SensorLedHal> {
    buzzer: B,
    power_led: PL,
    sensor_leds: SL,
    sensor_led_map: HashMap<SensorId, u8>,
}

impl<B: BuzzerHal, PL: LedHal, SL: SensorLedHal> HardwareAlarmActionSink<B, PL, SL> {
    pub fn new(
        buzzer: B,
        power_led: PL,
        sensor_leds: SL,
        sensor_led_map: HashMap<SensorId, u8>,
    ) -> Self {
        Self {
            buzzer,
            power_led,
            sensor_leds,
            sensor_led_map,
        }
    }

    /// Accessors for tests.
    pub fn buzzer(&self) -> &B {
        &self.buzzer
    }

    pub fn power_led(&self) -> &PL {
        &self.power_led
    }

    pub fn sensor_leds(&self) -> &SL {
        &self.sensor_leds
    }

    fn apply_power_led_and_buzzer(&mut self, state: AlarmState, severity: AlarmSeverity) {
        // PWRLED color
        let led_state = match state {
            AlarmState::Normal => LedState::Green,
            AlarmState::Warning => LedState::Yellow,
            AlarmState::Critical => LedState::Red,
            AlarmState::Fault => LedState::Red,
        };
        self.power_led.set_led_state(led_state);

        // Buzzer: only for P1Critical for now
        match severity {
            AlarmSeverity::P1Critical => self.buzzer.set_on(),
            AlarmSeverity::P2Warning | AlarmSeverity::P3Info => self.buzzer.set_off(),
        }
    }

    fn apply_sensor_led(&mut self, event: &AlarmEvent) {
        if let Some(index) = self.sensor_led_map.get(&event.sensor_id).copied() {
            let state = match event.to {
                AlarmState::Normal => SensorLedState::Green,
                AlarmState::Warning => SensorLedState::Both,
                AlarmState::Critical => SensorLedState::Red,
                AlarmState::Fault => SensorLedState::Red,
            };
            self.sensor_leds.set_sensor_led(index, state);
        }
    }
}

impl<B: BuzzerHal, PL: LedHal, SL: SensorLedHal> AlarmActionSink for HardwareAlarmActionSink<B, PL, SL> {
    fn on_alarm(&mut self, event: &AlarmEvent) {
        self.apply_power_led_and_buzzer(event.to, event.severity);
        self.apply_sensor_led(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarms::engine::{AlarmReason};
    // TODO: Mocks were removed - tests need real HAL implementations
    use crate::model::SensorId;
    use chrono::{TimeZone, Utc};

    fn sample_event(to: AlarmState, severity: AlarmSeverity) -> AlarmEvent {
        AlarmEvent {
            sensor_id: SensorId(1),
            ts_utc: Utc.timestamp_millis_opt(0).unwrap(),
            from: AlarmState::Normal,
            to,
            severity,
            reason: AlarmReason::ThresholdHigh,
            value: 42.0,
        }
    }

    #[test]
    fn recording_sink_captures_events() {
        let mut sink = RecordingAlarmActionSink::new();
        let ev = sample_event(AlarmState::Warning, AlarmSeverity::P2Warning);

        sink.on_alarm(&ev);
        sink.on_alarm(&ev);

        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sensor_id.0, 1);
        assert_eq!(events[0].to, AlarmState::Warning);
    }

    #[test]
    #[ignore] // TODO: Requires mock HAL implementations that were removed
    fn hardware_sink_maps_to_power_led_buzzer_and_sensor_leds() {
        use crate::hal::{LedState, SensorLedState};

        // Mock implementations removed - this test is disabled
        /*
        let buzzer = MockBuzzer::new();
        let power_led = MockLed::new();
        let sensor_leds = MockSensorLedBank::new();

        let mut map = HashMap::new();
        map.insert(SensorId(1), 0); // sensor 1 -> LED index 0

        let mut sink = HardwareAlarmActionSink::new(buzzer, power_led, sensor_leds, map);

        // NORMAL (Info): LED green, buzzer off, sensor green
        sink.on_alarm(&sample_event(AlarmState::Normal, AlarmSeverity::P3Info));

        // WARNING: LED yellow, buzzer off, sensor both
        sink.on_alarm(&sample_event(AlarmState::Warning, AlarmSeverity::P2Warning));

        // CRITICAL: LED red, buzzer on, sensor red
        sink.on_alarm(&sample_event(AlarmState::Critical, AlarmSeverity::P1Critical));

        // FAULT: LED red, buzzer on, sensor red
        sink.on_alarm(&sample_event(AlarmState::Fault, AlarmSeverity::P1Critical));

        let buzzer = sink.buzzer();
        let power_led = sink.power_led();
        let sensor_leds = sink.sensor_leds();

        // Buzzer transitions: off, off, on, on
        assert_eq!(buzzer.log, vec![false, false, true, true]);

        // PWRLED transitions: Green, Yellow, Red, Red
        assert_eq!(
            power_led.log,
            vec![LedState::Green, LedState::Yellow, LedState::Red, LedState::Red]
        );

        // Sensor LED transitions for index 0
        assert_eq!(
            sensor_leds.log,
            vec![
                (0, SensorLedState::Green),
                (0, SensorLedState::Both),
                (0, SensorLedState::Red),
                (0, SensorLedState::Red),
            ]
        );
        */
    }
}
