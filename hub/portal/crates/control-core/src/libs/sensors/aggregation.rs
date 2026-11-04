use crate::libs::alarms::AlarmState;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_BUFFERED_PERIODS: usize = 100; // 5 hours @ 3-min intervals

/// Count of samples in each alarm state during an aggregation window
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AlarmStateCounts {
    pub normal: u32,
    pub warning: u32,
    pub critical: u32,
    pub disconnected: u32,
    pub reconnecting: u32,
}

impl AlarmStateCounts {
    pub fn new() -> Self {
        Self {
            normal: 0,
            warning: 0,
            critical: 0,
            disconnected: 0,
            reconnecting: 0,
        }
    }

    pub fn increment(&mut self, state: AlarmState) {
        match state {
            AlarmState::Normal => self.normal += 1,
            AlarmState::Warning => self.warning += 1,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub fn add_sample(
        &mut self,
        temperature: f32,
        is_connected: bool,
        alarm_state: AlarmState,
        persistent_alarm_ts: Option<u64>,
    ) {
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
            self.avg_temp_celsius +=
                (temperature - self.avg_temp_celsius) / self.sample_count as f32;
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        Self::new_at(aggregation_interval, Self::unix_timestamp())
    }

    /// A state whose first window starts on the interval's UTC boundary at
    /// or before `now_ts` (plan/control/storage_intervals.md §4, S1): after a
    /// restart inside a window the partial bucket keeps the window's key.
    fn new_at(aggregation_interval: Duration, now_ts: u64) -> Self {
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
        let window_start_ts = state.window_boundary(now_ts);
        for sensor in &mut state.current_windows {
            sensor.window_start_ts = window_start_ts;
        }

        state
    }

    /// Add a reading to the appropriate sensor's aggregation window
    pub fn add_reading(
        &mut self,
        line: u8,
        temperature: f32,
        is_connected: bool,
        alarm_state: AlarmState,
    ) {
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
            sensor.add_sample(
                temperature,
                is_connected,
                alarm_state,
                self.alarm_state_timestamps[line_idx],
            );
        }
    }

    /// Check if it's time to finalize the current window.
    ///
    /// Windows of whole seconds close on UTC multiples of the interval, so a
    /// stored bucket (plan/control/storage_intervals.md §4) starts at
    /// 10:00, 10:10, … whatever the process start time; the first window
    /// after start is the partial one. Sub-second intervals (tests) fall back
    /// to elapsed time.
    pub fn should_finalize_window(&self) -> bool {
        self.should_finalize_window_at(Self::unix_timestamp())
    }

    fn should_finalize_window_at(&self, now_ts: u64) -> bool {
        match self.interval_secs() {
            Some(iv) => now_ts / iv > self.current_windows[0].window_start_ts / iv,
            None => self.window_start.elapsed() >= self.aggregation_interval,
        }
    }

    fn interval_secs(&self) -> Option<u64> {
        let iv = self.aggregation_interval.as_secs();
        (iv > 0).then_some(iv)
    }

    /// The UTC boundary the window closes on: `now` rounded down to the
    /// interval, or `now` itself for sub-second intervals.
    fn window_boundary(&self, now_ts: u64) -> u64 {
        match self.interval_secs() {
            Some(iv) => (now_ts / iv) * iv,
            None => now_ts,
        }
    }

    /// Update the aggregation interval (for hot-reload support).
    ///
    /// The same interval is a no-op: the open window keeps its samples. A
    /// new interval first closes the open window at `now` as a partial
    /// period, so its samples are stored rather than dropped, then starts the
    /// next window on the new interval's UTC boundary at or before `now`.
    pub fn update_interval(&mut self, new_interval: Duration) {
        self.update_interval_at(new_interval, Self::unix_timestamp());
    }

    fn update_interval_at(&mut self, new_interval: Duration, now_ts: u64) {
        if new_interval == self.aggregation_interval {
            return;
        }
        eprintln!(
            "[Aggregation] Interval changed {:?} -> {:?}, closing the open window",
            self.aggregation_interval, new_interval
        );

        let window_start_ts = self.current_windows[0].window_start_ts;
        let has_samples = self
            .current_windows
            .iter()
            .any(|w| w.sample_count + w.disconnected_count > 0);
        if has_samples {
            self.push_period(window_start_ts, now_ts.max(window_start_ts));
        }

        self.aggregation_interval = new_interval;
        let next_start = self.window_boundary(now_ts);
        for sensor in &mut self.current_windows {
            sensor.reset(next_start);
        }
        self.window_start = Instant::now();
    }

    /// Finalize the current window and start a new one
    pub fn finalize_window(&mut self) {
        self.finalize_window_at(Self::unix_timestamp());
    }

    fn finalize_window_at(&mut self, now_ts: u64) {
        // A window that started on or after the boundary (sub-second tests,
        // a clock step back) ends at `now`, never before its own start.
        let window_end_ts = self
            .window_boundary(now_ts)
            .max(self.current_windows[0].window_start_ts);

        let window_start_ts = self.current_windows[0].window_start_ts;
        self.push_period(window_start_ts, window_end_ts);

        // Reset for new window: it starts where this one ended.
        for sensor in &mut self.current_windows {
            sensor.reset(window_end_ts);
        }
        self.window_start = Instant::now();
    }

    /// Close the open windows as one period `[start, end)` and queue it.
    fn push_period(&mut self, period_start_ts: u64, period_end_ts: u64) {
        for sensor in &mut self.current_windows {
            sensor.finalize(period_end_ts);
        }
        let period =
            AggregationPeriod::new(self.current_windows.clone(), period_start_ts, period_end_ts);
        if self.completed_periods.len() >= MAX_BUFFERED_PERIODS {
            eprintln!(
                "[Aggregation] WARNING: Buffer full ({} periods), dropping oldest period",
                MAX_BUFFERED_PERIODS
            );
            self.completed_periods.pop_front();
        }
        self.completed_periods.push_back(period);
    }

    /// Close the open window first when `now` is already past its end, so a
    /// sample taken after the boundary lands in the window it belongs to.
    pub fn roll_over_if_due(&mut self) {
        if self.should_finalize_window() {
            self.finalize_window();
        }
    }

    #[cfg(test)]
    pub(crate) fn set_window_start_for_test(&mut self, window_start_ts: u64) {
        for w in self.current_windows.iter_mut() {
            w.window_start_ts = window_start_ts;
        }
    }

    #[cfg(test)]
    pub(crate) fn completed_periods_for_test(&self) -> &VecDeque<AggregationPeriod> {
        &self.completed_periods
    }

    #[cfg(test)]
    pub(crate) fn current_windows_sample_count(&self, line: usize) -> u32 {
        self.current_windows[line].sample_count
    }

    /// Take all completed periods for publishing (drains the queue)
    pub fn take_completed_periods(&mut self) -> Vec<AggregationPeriod> {
        let periods: Vec<_> = self.completed_periods.drain(..).collect();
        if !periods.is_empty() {
            eprintln!(
                "[Aggregation] Taking {} periods for MQTT publishing",
                periods.len()
            );
        }
        periods
    }

    /// Save completed periods to a JSON file for crash recovery
    pub fn save_pending(&self, path: &Path) -> Result<(), std::io::Error> {
        let periods: Vec<_> = self.completed_periods.iter().cloned().collect();
        if periods.is_empty() {
            // Nothing to save, remove stale file if it exists
            let _ = std::fs::remove_file(path);
            return Ok(());
        }
        let data = serde_json::to_string(&periods)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, data)?;
        eprintln!(
            "[Aggregation] Saved {} pending periods to {:?}",
            periods.len(),
            path
        );
        Ok(())
    }

    /// Load previously saved periods on startup
    pub fn load_pending(path: &Path) -> Vec<AggregationPeriod> {
        match std::fs::read_to_string(path) {
            Ok(data) => {
                let _ = std::fs::remove_file(path); // Consume on load
                match serde_json::from_str::<Vec<AggregationPeriod>>(&data) {
                    Ok(periods) => {
                        eprintln!(
                            "[Aggregation] Loaded {} pending periods from {:?}",
                            periods.len(),
                            path
                        );
                        periods
                    }
                    Err(e) => {
                        eprintln!(
                            "[Aggregation] Warning: Failed to parse pending periods: {}",
                            e
                        );
                        Vec::new()
                    }
                }
            }
            Err(_) => Vec::new(),
        }
    }

    /// Prepend previously saved periods to the front of the queue
    pub fn prepend_periods(&mut self, periods: Vec<AggregationPeriod>) {
        if periods.is_empty() {
            return;
        }
        eprintln!(
            "[Aggregation] Prepending {} recovered periods to queue",
            periods.len()
        );
        for period in periods.into_iter().rev() {
            self.completed_periods.push_front(period);
        }
        // Cap at max
        while self.completed_periods.len() > MAX_BUFFERED_PERIODS {
            self.completed_periods.pop_front();
        }
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

    /// Whole-second windows close on UTC multiples of the interval and the
    /// next one starts exactly there, so stored buckets line up across Hubs
    /// and restarts.
    #[test]
    fn whole_second_windows_close_on_utc_boundaries() {
        let mut state = AggregationState::new(Duration::from_secs(600));

        let now = AggregationState::unix_timestamp();
        for w in state.current_windows.iter_mut() {
            w.window_start_ts = now - 700;
        }
        assert!(state.should_finalize_window());
        state.add_reading(0, 5.0, true, AlarmState::Normal);
        state.finalize_window();

        let period = &state.completed_periods[0];
        assert_eq!(period.period_end_ts % 600, 0);
        assert_eq!(period.period_start_ts, now - 700);
        assert_eq!(
            state.current_windows[0].window_start_ts,
            period.period_end_ts
        );
        assert!(!state.should_finalize_window());
    }

    /// A new aggregation interval stores the open window as a partial
    /// period instead of dropping it, then starts on the new UTC boundary.
    #[test]
    fn an_interval_change_flushes_the_open_window() {
        let mut state = AggregationState::new(Duration::from_secs(600));
        for w in state.current_windows.iter_mut() {
            w.window_start_ts = 1_200;
        }
        state.add_reading(2, 6.0, true, AlarmState::Normal);
        state.add_reading(2, 8.0, true, AlarmState::Normal);

        state.update_interval_at(Duration::from_secs(300), 1_650);

        assert_eq!(state.completed_periods.len(), 1);
        let period = &state.completed_periods[0];
        assert_eq!(
            (period.period_start_ts, period.period_end_ts),
            (1_200, 1_650)
        );
        assert_eq!(period.sensors[2].sample_count, 2);
        assert_eq!(period.sensors[2].avg_temp_celsius, 7.0);
        assert_eq!(state.current_windows[2].sample_count, 0);
        assert_eq!(state.current_windows[0].window_start_ts, 1_500);
        assert_eq!(state.aggregation_interval, Duration::from_secs(300));
    }

    /// An empty window has nothing to store: the change queues no period.
    #[test]
    fn an_interval_change_on_an_empty_window_queues_nothing() {
        let mut state = AggregationState::new(Duration::from_secs(600));
        state.update_interval_at(Duration::from_secs(60), 1_650);
        assert!(state.completed_periods.is_empty());
        assert_eq!(state.current_windows[0].window_start_ts, 1_620);
    }

    /// Re-applying the same interval (a reload that changed only the sample
    /// interval or another key) keeps the open window and its samples.
    #[test]
    fn the_same_interval_keeps_the_open_window() {
        let mut state = AggregationState::new(Duration::from_secs(600));
        for w in state.current_windows.iter_mut() {
            w.window_start_ts = 1_200;
        }
        state.add_reading(0, 5.0, true, AlarmState::Normal);
        state.update_interval_at(Duration::from_secs(600), 1_650);
        assert!(state.completed_periods.is_empty());
        assert_eq!(state.current_windows[0].sample_count, 1);
        assert_eq!(state.current_windows[0].window_start_ts, 1_200);
    }

    /// The first window starts on the UTC boundary at or before `now`, so a
    /// restart inside a window keeps that window's `minute_ts`.
    #[test]
    fn the_first_window_starts_on_the_utc_boundary() {
        let state = AggregationState::new_at(Duration::from_secs(600), 1_234);
        assert!(state
            .current_windows
            .iter()
            .all(|w| w.window_start_ts == 1_200));
        assert!(!state.should_finalize_window_at(1_799));
        assert!(state.should_finalize_window_at(1_800));

        let mut state = state;
        state.add_reading(1, 4.0, true, AlarmState::Normal);
        state.finalize_window_at(1_805);
        let period = &state.completed_periods[0];
        assert_eq!(
            (period.period_start_ts, period.period_end_ts),
            (1_200, 1_800)
        );
        assert_eq!(state.current_windows[1].window_start_ts, 1_800);
    }
}
