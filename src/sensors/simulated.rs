// src/sensors/simulated.rs
use crate::acquisition::SensorBackend;
use crate::model::{ReadingQuality, SensorId};
use chrono::{DateTime, Utc};

/// Simulated temperature backend:
/// - value = base_c + amplitude_c * sin(2π * t / period_s)
/// - t taken from `now` in seconds.
///
/// This lets you exercise acquisition, alarms, logging, LEDs, and buzzer
/// without having real sensors connected.
pub struct SimulatedTemperatureBackend {
    id: SensorId,
    base_c: f32,
    amplitude_c: f32,
    period_s: f32,
}

impl SimulatedTemperatureBackend {
    /// `base_c`      = center temperature (e.g., 4.0 for a fridge)
    /// `amplitude_c` = peak deviation (e.g., 2.0 → range [2.0, 6.0])
    /// `period_s`    = period of the sine in seconds (e.g., 300s = 5 min)
    pub fn new(id: SensorId, base_c: f32, amplitude_c: f32, period_s: f32) -> Self {
        Self {
            id,
            base_c,
            amplitude_c,
            period_s,
        }
    }
}

impl SensorBackend for SimulatedTemperatureBackend {
    fn sensor_id(&self) -> SensorId {
        self.id
    }

    fn read(&mut self, now: DateTime<Utc>) -> (f32, ReadingQuality) {
        let t = now.timestamp_millis() as f32 / 1000.0; // seconds
        let omega = 2.0 * std::f32::consts::PI / self.period_s.max(1.0);
        let phase = (t * omega).sin();
        let temp = self.base_c + self.amplitude_c * phase;
        (temp, ReadingQuality::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn simulated_backend_stays_within_expected_range() {
        let id = SensorId(1);
        let base = 4.0;
        let amp = 2.0;
        let period = 60.0;
        let mut backend = SimulatedTemperatureBackend::new(id, base, amp, period);

        // Sample a few points over one period
        let mut min_v = f32::MAX;
        let mut max_v = f32::MIN;

        for i in 0..10 {
            let t = (i as i64) * 6_000; // 6 seconds apart
            let now = Utc.timestamp_millis_opt(t).unwrap();
            let (val, q) = backend.read(now);
            assert_eq!(q, ReadingQuality::Ok);
            if val < min_v {
                min_v = val;
            }
            if val > max_v {
                max_v = val;
            }
        }

        // Expected range approximately [base-amp, base+amp]
        assert!(min_v >= base - amp - 0.1);
        assert!(max_v <= base + amp + 0.1);
    }
}
