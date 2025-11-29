// src/alarms/engine.rs
use crate::model::{ReadingQuality, SensorId, SensorReading};
use chrono::{DateTime, Utc};

/// Current alarm state for a sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmState {
    Normal,
    Warning,
    Critical,
    Fault,
}

/// Severity classification for alarms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmSeverity {
    /// P1 - CRITICAL
    P1Critical,
    /// P2 - WARNING
    P2Warning,
    /// P3 - informational (e.g. recovery to normal)
    P3Info,
}

/// Why a transition happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmReason {
    ThresholdHigh,
    ThresholdLow,
    FaultDetected,
    RecoverToNormal,
}

/// Static configuration for one sensor's alarm rules.
#[derive(Debug, Clone, Copy)]
pub struct AlarmConfig {
    /// Low warning threshold (Optional, e.g. for too-cold alarm)
    pub warning_low: Option<f32>,
    /// High warning threshold
    pub warning_high: Option<f32>,
    /// Low critical threshold
    pub critical_low: Option<f32>,
    /// High critical threshold
    pub critical_high: Option<f32>,
    /// Hysteresis band (same unit as value, e.g. °C)
    pub hysteresis: f32,
}

impl Default for AlarmConfig {
    fn default() -> Self {
        Self {
            warning_low: None,
            warning_high: None,
            critical_low: None,
            critical_high: None,
            hysteresis: 0.5,
        }
    }
}

/// One alarm transition event (from NORMAL → WARNING, etc.).
#[derive(Debug, Clone)]
pub struct AlarmEvent {
    pub sensor_id: SensorId,
    pub ts_utc: DateTime<Utc>,
    pub from: AlarmState,
    pub to: AlarmState,
    pub severity: AlarmSeverity,
    pub reason: AlarmReason,
    pub value: f32,
}

/// Alarm logic for a single sensor.
/// Pure logic: no I/O, no UART, no GPIO, no DB.
pub struct SensorAlarm {
    sensor_id: SensorId,
    config: AlarmConfig,
    state: AlarmState,
}

impl SensorAlarm {
    pub fn new(sensor_id: SensorId, config: AlarmConfig) -> Self {
        Self {
            sensor_id,
            config,
            state: AlarmState::Normal,
        }
    }

    pub fn state(&self) -> AlarmState {
        self.state
    }

    pub fn sensor_id(&self) -> SensorId {
        self.sensor_id
    }

    pub fn config(&self) -> AlarmConfig {
        self.config
    }

    /// Feed a new reading into the alarm state machine.
    ///
    /// Returns Some(AlarmEvent) if the state changed, otherwise None.
    pub fn apply_reading(&mut self, reading: &SensorReading) -> Option<AlarmEvent> {
        // sanity check: must belong to this alarm
        if reading.sensor_id != self.sensor_id {
            debug_assert_eq!(reading.sensor_id, self.sensor_id);
            return None;
        }

        // Skip alarm evaluation for "Other" quality readings.
        // This is used for sensors waiting to be discovered (no ROM yet).
        // We don't want to trigger alarms just because discovery hasn't found the sensor yet.
        if reading.quality == ReadingQuality::Other {
            return None;
        }

        let from_state = self.state;
        let mut to_state = self.state;
        let mut reason: Option<AlarmReason> = None;

        // 1) Quality-based FAULT (for actual sensor faults, not "waiting for discovery")
        if reading.quality != ReadingQuality::Ok {
            if self.state != AlarmState::Fault {
                to_state = AlarmState::Fault;
                reason = Some(AlarmReason::FaultDetected);
            }
        } else {
            // 2) Threshold-based logic (value in °C or whatever)
            let v = reading.value;
            let cfg = self.config;

            match self.state {
                AlarmState::Normal => {
                    // Check critical first (highest priority)
                    if let Some(ch) = cfg.critical_high {
                        if v >= ch {
                            to_state = AlarmState::Critical;
                            reason = Some(AlarmReason::ThresholdHigh);
                        }
                    }
                    if to_state == AlarmState::Normal {
                        if let Some(cl) = cfg.critical_low {
                            if v <= cl {
                                to_state = AlarmState::Critical;
                                reason = Some(AlarmReason::ThresholdLow);
                            }
                        }
                    }

                    // Then warning
                    if to_state == AlarmState::Normal {
                        if let Some(wh) = cfg.warning_high {
                            if v >= wh {
                                to_state = AlarmState::Warning;
                                reason = Some(AlarmReason::ThresholdHigh);
                            }
                        }
                    }
                    if to_state == AlarmState::Normal {
                        if let Some(wl) = cfg.warning_low {
                            if v <= wl {
                                to_state = AlarmState::Warning;
                                reason = Some(AlarmReason::ThresholdLow);
                            }
                        }
                    }
                }

                AlarmState::Warning => {
                    // Escalate to CRITICAL if beyond critical thresholds.
                    if let Some(ch) = cfg.critical_high {
                        if v >= ch {
                            to_state = AlarmState::Critical;
                            reason = Some(AlarmReason::ThresholdHigh);
                        }
                    }
                    if to_state == AlarmState::Warning {
                        if let Some(cl) = cfg.critical_low {
                            if v <= cl {
                                to_state = AlarmState::Critical;
                                reason = Some(AlarmReason::ThresholdLow);
                            }
                        }
                    }

                    // De-escalate back to NORMAL when inside normal band with hysteresis
                    if to_state == AlarmState::Warning {
                        let above_low_ok = cfg
                            .warning_low
                            .map(|wl| v > wl + cfg.hysteresis)
                            .unwrap_or(true);

                        let below_high_ok = cfg
                            .warning_high
                            .map(|wh| v < wh - cfg.hysteresis)
                            .unwrap_or(true);

                        if above_low_ok && below_high_ok {
                            to_state = AlarmState::Normal;
                            reason = Some(AlarmReason::RecoverToNormal);
                        }
                    }
                }

                AlarmState::Critical => {
                    // Stay in CRITICAL if still beyond critical thresholds (with hysteresis)
                    let high_crit = cfg
                        .critical_high
                        .map(|ch| v >= ch - cfg.hysteresis)
                        .unwrap_or(false);
                    let low_crit = cfg
                        .critical_low
                        .map(|cl| v <= cl + cfg.hysteresis)
                        .unwrap_or(false);

                    if !(high_crit || low_crit) {
                        // We left the critical band. Maybe Warning, maybe Normal.
                        let high_warn = cfg
                            .warning_high
                            .map(|wh| v >= wh - cfg.hysteresis)
                            .unwrap_or(false);
                        let low_warn = cfg
                            .warning_low
                            .map(|wl| v <= wl + cfg.hysteresis)
                            .unwrap_or(false);

                        if high_warn || low_warn {
                            to_state = AlarmState::Warning;
                            reason = Some(AlarmReason::ThresholdHigh);
                        } else {
                            to_state = AlarmState::Normal;
                            reason = Some(AlarmReason::RecoverToNormal);
                        }
                    }
                }

                AlarmState::Fault => {
                    // First good reading after FAULT: go back to NORMAL
                    to_state = AlarmState::Normal;
                    reason = Some(AlarmReason::RecoverToNormal);
                }
            }
        }

        if to_state != from_state {
            self.state = to_state;

            let severity = match to_state {
                AlarmState::Critical => AlarmSeverity::P1Critical,
                AlarmState::Fault => AlarmSeverity::P1Critical,
                AlarmState::Warning => AlarmSeverity::P2Warning,
                AlarmState::Normal => AlarmSeverity::P3Info,
            };

            Some(AlarmEvent {
                sensor_id: self.sensor_id,
                ts_utc: reading.ts_utc,
                from: from_state,
                to: to_state,
                severity,
                reason: reason.unwrap_or(AlarmReason::RecoverToNormal),
                value: reading.value,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ReadingQuality, SensorReading};
    use chrono::{TimeZone, Utc};

    fn sensor_id() -> SensorId {
        SensorId(1)
    }

    fn reading_at(value: f32, ms: i64) -> SensorReading {
        SensorReading {
            sensor_id: sensor_id(),
            ts_utc: Utc.timestamp_millis_opt(ms).unwrap(),
            value,
            quality: ReadingQuality::Ok,
        }
    }

    fn bad_reading(ms: i64) -> SensorReading {
        SensorReading {
            sensor_id: sensor_id(),
            ts_utc: Utc.timestamp_millis_opt(ms).unwrap(),
            value: 0.0,
            quality: ReadingQuality::Timeout,
        }
    }

    #[test]
    fn normal_to_warning_and_back_with_hysteresis() {
        let cfg = AlarmConfig {
            warning_low: None,
            warning_high: Some(30.0),
            critical_low: None,
            critical_high: Some(40.0),
            hysteresis: 0.5,
        };

        let mut alarm = SensorAlarm::new(sensor_id(), cfg);
        assert_eq!(alarm.state(), AlarmState::Normal);

        // Below warning_high => NORMAL
        let ev = alarm.apply_reading(&reading_at(29.0, 0));
        assert!(ev.is_none());
        assert_eq!(alarm.state(), AlarmState::Normal);

        // Cross warning_high => WARNING
        let ev = alarm.apply_reading(&reading_at(31.0, 1000)).expect("event");
        assert_eq!(ev.to, AlarmState::Warning);
        assert_eq!(alarm.state(), AlarmState::Warning);

        // Slightly below threshold but inside hysteresis band => stay WARNING
        let ev = alarm.apply_reading(&reading_at(29.7, 2000));
        assert!(ev.is_none());
        assert_eq!(alarm.state(), AlarmState::Warning);

        // Go clearly back into normal band (below 30 - 0.5 = 29.5) => NORMAL
        let ev = alarm.apply_reading(&reading_at(29.0, 3000)).expect("event");
        assert_eq!(ev.from, AlarmState::Warning);
        assert_eq!(ev.to, AlarmState::Normal);
        assert_eq!(ev.reason, AlarmReason::RecoverToNormal);
        assert_eq!(alarm.state(), AlarmState::Normal);
    }

    #[test]
    fn warning_to_critical_and_back() {
        let cfg = AlarmConfig {
            warning_low: None,
            warning_high: Some(30.0),
            critical_low: None,
            critical_high: Some(40.0),
            hysteresis: 0.5,
        };

        let mut alarm = SensorAlarm::new(sensor_id(), cfg);

        // Enter WARNING
        let _ = alarm.apply_reading(&reading_at(32.0, 0));
        assert_eq!(alarm.state(), AlarmState::Warning);

        // Enter CRITICAL
        let ev = alarm.apply_reading(&reading_at(41.0, 1000)).expect("event");
        assert_eq!(ev.from, AlarmState::Warning);
        assert_eq!(ev.to, AlarmState::Critical);
        assert_eq!(alarm.state(), AlarmState::Critical);

        // Drop slightly but still within critical-hysteresis band => stay CRITICAL
        let ev = alarm.apply_reading(&reading_at(39.8, 2000));
        assert!(ev.is_none());
        assert_eq!(alarm.state(), AlarmState::Critical);

        // Go clearly below critical band but still above warning => back to WARNING
        let ev = alarm.apply_reading(&reading_at(38.0, 3000)).expect("event");
        assert_eq!(ev.from, AlarmState::Critical);
        assert_eq!(ev.to, AlarmState::Warning);
        assert_eq!(alarm.state(), AlarmState::Warning);
    }

    #[test]
    fn fault_on_bad_quality_and_recovery() {
        let cfg = AlarmConfig {
            warning_low: Some(10.0),
            warning_high: Some(30.0),
            critical_low: None,
            critical_high: None,
            hysteresis: 0.5,
        };

        let mut alarm = SensorAlarm::new(sensor_id(), cfg);
        assert_eq!(alarm.state(), AlarmState::Normal);

        // Bad quality => FAULT
        let ev = alarm.apply_reading(&bad_reading(0)).expect("event");
        assert_eq!(ev.to, AlarmState::Fault);
        assert_eq!(ev.reason, AlarmReason::FaultDetected);
        assert_eq!(alarm.state(), AlarmState::Fault);

        // First good reading => back to NORMAL
        let ev = alarm
            .apply_reading(&reading_at(20.0, 1000))
            .expect("event");
        assert_eq!(ev.from, AlarmState::Fault);
        assert_eq!(ev.to, AlarmState::Normal);
        assert_eq!(ev.reason, AlarmReason::RecoverToNormal);
        assert_eq!(alarm.state(), AlarmState::Normal);
    }
}
