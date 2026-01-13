use crate::libs::alarms::AlarmState;
use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_BUFFERED_PERIODS: usize = 100; // 5 hours @ 3-min intervals

/// Count of samples in each alarm state during an aggregation window
#[derive(Debug, Clone, Copy)]
pub struct AlarmStateCounts {
    pub normal: u32,
    pub warning: u32,
    pub alarm: u32,
    pub critical: u32,
    pub disconnected: u32,
    pub reconnecting: u32,
}

impl AlarmStateCounts {
    pub fn new() -> Self {
        Self {
            normal: 0,
            warning: 0,
            alarm: 0,
            critical: 0,
            disconnected: 0,
            reconnecting: 0,
        }
    }

    pub fn increment(&mut self, state: AlarmState) {
        match state {
            AlarmState::Normal => self.normal += 1,
            AlarmState::Warning => self.warning += 1,
            AlarmState::Alarm => self.alarm += 1,
            AlarmState::Critical => self.critical += 1,
            AlarmState::Disconnected => self.disconnected += 1,
            AlarmState::Reconnecting => self.reconnecting += 1,
            AlarmState::NeverConnected => self.disconnected += 1,
        }
    }

    /// Get the dominant (most severe) alarm state
    pub fn dominant(&self) -> AlarmState {
        if self.critical > 0 {
            AlarmState::Critical
        } else if self.alarm > 0 {
            AlarmState::Alarm
        } else if self.warning > 0 {
            AlarmState::Warning
        } else if self.reconnecting > 0 {
            AlarmState::Reconnecting
        } else if self.disconnected > 0 {
            AlarmState::Disconnected
        } else {
            AlarmState::Normal
        }
    }
}

/// Per-sensor statistics for one aggregation window
#[derive(Debug, Clone)]
pub struct SensorAggregation {
    pub line: u8,
    pub sample_count: u32,
    pub disconnected_count: u32,
    pub min_temp_celsius: f32,
    pub max_temp_celsius: f32,
    pub avg_temp_celsius: f32,
    pub alarm_counts: AlarmStateCounts,
    pub alarm_triggered_at: Option<u64>,
    pub window_start_ts: u64,
    pub window_end_ts: u64,
}

impl SensorAggregation {
    pub fn new(line: u8) -> Self {
        Self {
            line,
            sample_count: 0,
            disconnected_count: 0,
            min_temp_celsius: f32::MAX,
            max_temp_celsius: f32::MIN,
            avg_temp_celsius: 0.0,
            alarm_counts: AlarmStateCounts::new(),
            alarm_triggered_at: None,
            window_start_ts: 0,
            window_end_ts: 0,
        }
    }

    /// Add a sample to the aggregation window
    /// `persistent_alarm_ts` is the timestamp when the sensor entered its current alarm state
    /// (managed by AggregationState, persists across windows)
    pub fn add_sample(&mut self, temperature: f32, is_connected: bool, alarm_state: AlarmState, persistent_alarm_ts: Option<u64>) {
        // Track alarm state counts
        self.alarm_counts.increment(alarm_state);

        // Use the persistent alarm timestamp from AggregationState
        self.alarm_triggered_at = persistent_alarm_ts;

        if is_connected {
            // Update temperature statistics using running average
            self.sample_count += 1;
            self.min_temp_celsius = self.min_temp_celsius.min(temperature);
            self.max_temp_celsius = self.max_temp_celsius.max(temperature);

            // Running average formula: new_avg = old_avg + (new_value - old_avg) / count
            self.avg_temp_celsius += (temperature - self.avg_temp_celsius) / self.sample_count as f32;
        } else {
            // Track disconnected samples
            self.disconnected_count += 1;
        }
    }

    /// Check if this sensor has valid samples
    pub fn is_valid(&self) -> bool {
        self.sample_count > 0
    }

    /// Get the dominant alarm state during this window
    pub fn dominant_alarm_state(&self) -> AlarmState {
        self.alarm_counts.dominant()
    }

    /// Reset the aggregation window
    pub fn reset(&mut self, window_start_ts: u64) {
        self.sample_count = 0;
        self.disconnected_count = 0;
        self.min_temp_celsius = f32::MAX;
        self.max_temp_celsius = f32::MIN;
        self.avg_temp_celsius = 0.0;
        self.alarm_counts = AlarmStateCounts::new();
        self.alarm_triggered_at = None;
        self.window_start_ts = window_start_ts;
        self.window_end_ts = 0;
    }

    /// Finalize the window with end timestamp
    pub fn finalize(&mut self, window_end_ts: u64) {
        self.window_end_ts = window_end_ts;
    }
}

/// Completed aggregation period for all 8 sensors
#[derive(Debug, Clone)]
pub struct AggregationPeriod {
    pub sensors: [SensorAggregation; 8],
    pub period_start_ts: u64,
    pub period_end_ts: u64,
}

impl AggregationPeriod {
    pub fn new(sensors: [SensorAggregation; 8], period_start_ts: u64, period_end_ts: u64) -> Self {
        Self {
            sensors,
            period_start_ts,
            period_end_ts,
        }
    }
}

/// Global aggregation state managing all sensors
pub struct AggregationState {
    current_windows: [SensorAggregation; 8],
    window_start: Instant,
    aggregation_interval: Duration,
    completed_periods: VecDeque<AggregationPeriod>,
    /// Track last alarm state per sensor (for detecting transitions)
    last_alarm_states: [AlarmState; 8],
    /// Timestamp when each sensor entered its current alarm state (persists across windows)
    alarm_state_timestamps: [Option<u64>; 8],
}

impl AggregationState {
    pub fn new(aggregation_interval: Duration) -> Self {
        let now = Self::unix_timestamp();
        let current_windows = [
            SensorAggregation::new(0),
            SensorAggregation::new(1),
            SensorAggregation::new(2),
            SensorAggregation::new(3),
            SensorAggregation::new(4),
            SensorAggregation::new(5),
            SensorAggregation::new(6),
            SensorAggregation::new(7),
        ];

        // Initialize window start timestamps
        let mut state = Self {
            current_windows,
            window_start: Instant::now(),
            aggregation_interval,
            completed_periods: VecDeque::with_capacity(MAX_BUFFERED_PERIODS),
            // Initialize all sensors as Normal with no alarm timestamp
            last_alarm_states: [AlarmState::Normal; 8],
            alarm_state_timestamps: [None; 8],
        };

        // Set initial timestamps for all sensors
        for sensor in &mut state.current_windows {
            sensor.window_start_ts = now;
        }

        state
    }

    /// Add a reading to the appropriate sensor's aggregation window
    pub fn add_reading(&mut self, line: u8, temperature: f32, is_connected: bool, alarm_state: AlarmState) {
        let line_idx = line as usize;
        if line_idx >= 8 {
            return;
        }

        // Check if alarm state changed from last reading
        let last_state = self.last_alarm_states[line_idx];
        if alarm_state != last_state {
            // State changed - update persistent timestamp
            self.last_alarm_states[line_idx] = alarm_state;

            // Set timestamp for non-Normal states, clear for Normal
            if matches!(alarm_state, AlarmState::Normal) {
                self.alarm_state_timestamps[line_idx] = None;
            } else {
                self.alarm_state_timestamps[line_idx] = Some(Self::unix_timestamp());
            }
        }

        // Add sample to the current window
        if let Some(sensor) = self.current_windows.get_mut(line_idx) {
            sensor.add_sample(temperature, is_connected, alarm_state, self.alarm_state_timestamps[line_idx]);
        }
    }

    /// Check if it's time to finalize the current window
    pub fn should_finalize_window(&self) -> bool {
        self.window_start.elapsed() >= self.aggregation_interval
    }

    /// Update the aggregation interval (for hot-reload support)
    /// Always resets the window to ensure clean state after any config reload.
    /// This discards partially collected samples but ensures predictable timing.
    pub fn update_interval(&mut self, new_interval: Duration) {
        let changed = new_interval != self.aggregation_interval;
        self.aggregation_interval = new_interval;

        // Always reset window on hot-reload to ensure clean state
        // Even if interval didn't change, other config values may have changed
        eprintln!(
            "[Aggregation] Interval {} (now {:?}), resetting window",
            if changed { "changed" } else { "refreshed" },
            new_interval
        );

        let window_start_ts = Self::unix_timestamp();
        for sensor in &mut self.current_windows {
            sensor.reset(window_start_ts);
        }
        self.window_start = Instant::now();
    }

    /// Finalize the current window and start a new one
    pub fn finalize_window(&mut self) {
        let window_end_ts = Self::unix_timestamp();

        // Finalize all sensor windows
        for sensor in &mut self.current_windows {
            sensor.finalize(window_end_ts);
        }

        // Create aggregation period
        let period = AggregationPeriod::new(
            self.current_windows.clone(),
            self.current_windows[0].window_start_ts,
            window_end_ts,
        );

        // Add to completed periods queue
        if self.completed_periods.len() >= MAX_BUFFERED_PERIODS {
            eprintln!(
                "[Aggregation] WARNING: Buffer full ({} periods), dropping oldest period",
                MAX_BUFFERED_PERIODS
            );
            self.completed_periods.pop_front();
        }
        self.completed_periods.push_back(period);

        eprintln!(
            "[Aggregation] Finalized window: {} samples (sensor 0), {} periods queued",
            self.current_windows[0].sample_count,
            self.completed_periods.len()
        );

        // Reset for new window
        let window_start_ts = Self::unix_timestamp();
        for sensor in &mut self.current_windows {
            sensor.reset(window_start_ts);
        }
        self.window_start = Instant::now();
    }

    /// Take all completed periods for publishing (drains the queue)
    pub fn take_completed_periods(&mut self) -> Vec<AggregationPeriod> {
        let periods: Vec<_> = self.completed_periods.drain(..).collect();
        if !periods.is_empty() {
            eprintln!("[Aggregation] Taking {} periods for MQTT publishing", periods.len());
        }
        periods
    }

    /// Get current UNIX timestamp in seconds
    fn unix_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::from_secs(0))
            .as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alarm_state_counts() {
        let mut counts = AlarmStateCounts::new();
        assert_eq!(counts.normal, 0);

        counts.increment(AlarmState::Normal);
        counts.increment(AlarmState::Normal);
        counts.increment(AlarmState::Warning);

        assert_eq!(counts.normal, 2);
        assert_eq!(counts.warning, 1);
        assert_eq!(counts.dominant(), AlarmState::Warning);
    }

    #[test]
    fn test_sensor_aggregation_running_average() {
        let mut sensor = SensorAggregation::new(0);

        sensor.add_sample(36.0, true, AlarmState::Normal, None);
        assert_eq!(sensor.sample_count, 1);
        assert_eq!(sensor.avg_temp_celsius, 36.0);

        sensor.add_sample(38.0, true, AlarmState::Normal, None);
        assert_eq!(sensor.sample_count, 2);
        assert_eq!(sensor.avg_temp_celsius, 37.0);

        sensor.add_sample(37.0, true, AlarmState::Normal, None);
        assert_eq!(sensor.sample_count, 3);
        assert!((sensor.avg_temp_celsius - 37.0).abs() < 0.01);
    }

    #[test]
    fn test_sensor_aggregation_min_max() {
        let mut sensor = SensorAggregation::new(0);

        sensor.add_sample(36.5, true, AlarmState::Normal, None);
        sensor.add_sample(37.2, true, AlarmState::Normal, None);
        sensor.add_sample(35.8, true, AlarmState::Normal, None);

        assert_eq!(sensor.min_temp_celsius, 35.8);
        assert_eq!(sensor.max_temp_celsius, 37.2);
    }

    #[test]
    fn test_sensor_aggregation_disconnected() {
        let mut sensor = SensorAggregation::new(0);

        sensor.add_sample(36.0, true, AlarmState::Normal, None);
        sensor.add_sample(36.0, true, AlarmState::Normal, None);
        sensor.add_sample(0.0, false, AlarmState::Disconnected, Some(12345));

        assert_eq!(sensor.sample_count, 2);
        assert_eq!(sensor.disconnected_count, 1);
        assert_eq!(sensor.alarm_counts.normal, 2);
        assert_eq!(sensor.alarm_counts.disconnected, 1);
    }

    #[test]
    fn test_aggregation_state_window_finalization() {
        let mut state = AggregationState::new(Duration::from_millis(100));

        state.add_reading(0, 36.5, true, AlarmState::Normal);
        state.add_reading(1, 37.0, true, AlarmState::Normal);

        std::thread::sleep(Duration::from_millis(150));

        assert!(state.should_finalize_window());

        state.finalize_window();
        assert_eq!(state.completed_periods.len(), 1);
        assert_eq!(state.current_windows[0].sample_count, 0);
    }

    #[test]
    fn test_aggregation_state_buffer_overflow() {
        let mut state = AggregationState::new(Duration::from_millis(1));

        // Fill buffer beyond capacity
        for _ in 0..105 {
            std::thread::sleep(Duration::from_millis(2));
            state.finalize_window();
        }

        // Should cap at MAX_BUFFERED_PERIODS
        assert_eq!(state.completed_periods.len(), MAX_BUFFERED_PERIODS);
    }

    #[test]
    fn test_take_completed_periods() {
        let mut state = AggregationState::new(Duration::from_millis(1));

        std::thread::sleep(Duration::from_millis(2));
        state.finalize_window();
        std::thread::sleep(Duration::from_millis(2));
        state.finalize_window();

        assert_eq!(state.completed_periods.len(), 2);

        let periods = state.take_completed_periods();
        assert_eq!(periods.len(), 2);
        assert_eq!(state.completed_periods.len(), 0);
    }
}
