// src/acquisition/mod.rs
use crate::model::{ReadingQuality, SensorId, SensorReading};
use chrono::{DateTime, Duration as ChronoDuration, Utc};

/// Configuration for a single sensor acquisition stream.
#[derive(Debug, Clone, Copy)]
pub struct AcquisitionConfig {
    /// Desired sampling interval.
    ///
    /// For 100 samples/minute:
    ///   60_000 ms / 100 = 600 ms
    pub sample_interval: ChronoDuration,
}

impl AcquisitionConfig {
    /// Helper for 100 samples/minute.
    pub fn hundred_per_minute() -> Self {
        Self {
            sample_interval: ChronoDuration::milliseconds(600),
        }
    }
}

/// Backend that can actually read a sensor value.
///
/// For now this is *abstract*:
/// - In tests we use a fake backend.
/// - Later we'll have a OneWire/STM backend implementation.
pub trait SensorBackend {
    fn sensor_id(&self) -> SensorId;

    /// Read the current value for this sensor.
    ///
    /// Returns (value, quality).
    /// `now` is passed so you *could* do time-based behavior,
    /// but for many real sensors you'll ignore it.
    fn read(&mut self, now: DateTime<Utc>) -> (f32, ReadingQuality);
}

/// Acquisition engine for a single sensor.
///
/// Pure logic:
/// - You call `tick(now)` from some scheduler.
/// - When it's time to sample, it calls the backend and returns a SensorReading.
/// - Otherwise it returns None.
pub struct AcquisitionEngine<B: SensorBackend> {
    backend: B,
    config: AcquisitionConfig,
    last_sample_ts: Option<DateTime<Utc>>,
}

impl<B: SensorBackend> AcquisitionEngine<B> {
    pub fn new(backend: B, config: AcquisitionConfig) -> Self {
        Self {
            backend,
            config,
            last_sample_ts: None,
        }
    }

    pub fn sensor_id(&self) -> SensorId {
        self.backend.sensor_id()
    }

    pub fn config(&self) -> AcquisitionConfig {
        self.config
    }

    /// Feed in the "current" time.
    ///
    /// If enough time has elapsed since the last sample (or this is the first),
    /// we ask the backend for a new value and return a SensorReading.
    ///
    /// Otherwise, returns None.
    pub fn tick(&mut self, now: DateTime<Utc>) -> Option<SensorReading> {
        let need_sample = match self.last_sample_ts {
            None => true,
            Some(last) => now - last >= self.config.sample_interval,
        };

        if !need_sample {
            return None;
        }

        let (value, quality) = self.backend.read(now);
        let reading = SensorReading {
            sensor_id: self.backend.sensor_id(),
            ts_utc: now,
            value,
            quality,
        };

        self.last_sample_ts = Some(now);
        Some(reading)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarms::engine::{AlarmConfig, AlarmReason, AlarmState, SensorAlarm};
    use crate::model::SensorReading;
    use chrono::{TimeZone, Utc};

    #[derive(Debug)]
    struct FakeBackend {
        id: SensorId,
        // sequence of values we want to output
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
                // if we run out, repeat last
                self.values.last().copied().unwrap_or(0.0)
            };
            (v, self.quality)
        }
    }

    fn t(ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(ms).unwrap()
    }

    #[test]
    fn acquisition_respects_600ms_interval() {
        let sid = SensorId(42);
        let backend = FakeBackend::new(sid, vec![1.0, 2.0, 3.0, 4.0]);
        let cfg = AcquisitionConfig::hundred_per_minute();
        let mut engine = AcquisitionEngine::new(backend, cfg);

        let mut readings: Vec<SensorReading> = Vec::new();

        // timeline: 0, 200, 400, 600, 800, 1200, ...
        // We expect samples at 0, 600, 1200, ...
        for step in 0..6 {
            let now = t(step * 200);
            if let Some(r) = engine.tick(now) {
                readings.push(r);
            }
        }

        // We should have readings at:
        // step 0 (0 ms), step 3 (600 ms)
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0].ts_utc, t(0));
        assert_eq!(readings[1].ts_utc, t(600));

        // Values should follow the backend sequence
        assert_eq!(readings[0].value, 1.0);
        assert_eq!(readings[1].value, 2.0);
    }

    #[test]
    fn acquisition_and_alarm_integration() {
        let sid = SensorId(1);

        // Values: start normal, enter WARNING, then CRITICAL, then back towards normal.
        let values = vec![25.0, 31.0, 35.0, 42.0, 39.0, 29.0];
        let backend = FakeBackend::new(sid, values);
        let cfg = AcquisitionConfig::hundred_per_minute();
        let mut engine = AcquisitionEngine::new(backend, cfg);

        let alarm_cfg = AlarmConfig {
            warning_low: None,
            warning_high: Some(30.0),
            critical_low: None,
            critical_high: Some(40.0),
            hysteresis: 0.5,
        };
        let mut alarm = SensorAlarm::new(sid, alarm_cfg);

        let mut events = Vec::new();

        // We'll step time by exactly the sampling interval so we always generate a reading.
        let mut now = t(0);
        for _ in 0..6 {
            if let Some(reading) = engine.tick(now) {
                if let Some(ev) = alarm.apply_reading(&reading) {
                    events.push(ev);
                }
            }
            now = now + cfg.sample_interval;
        }

        // We expect transitions:
        // NORMAL -> WARNING (cross 30)
        // WARNING -> CRITICAL (cross 40)
        // CRITICAL -> WARNING or NORMAL depending on hysteresis
        assert!(events.len() >= 2);

        assert_eq!(events[0].from, AlarmState::Normal);
        assert_eq!(events[0].to, AlarmState::Warning);
        assert_eq!(events[0].reason, AlarmReason::ThresholdHigh);

        assert_eq!(events[1].from, AlarmState::Warning);
        assert_eq!(events[1].to, AlarmState::Critical);
        assert_eq!(events[1].reason, AlarmReason::ThresholdHigh);

        // Final state should not be CRITICAL anymore
        assert_ne!(alarm.state(), AlarmState::Critical);
    }
}
