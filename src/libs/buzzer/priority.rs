//! Buzzer priority manager for coordinating multiple alarm sources
//!
//! Manages priority between battery critical alarms and sensor critical alarms.
//! Ensures that sensor critical alarms always take priority, and when both are
//! active, alternates between them to alert the user to both conditions.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::pattern::BuzzerPattern;
use super::controller::BuzzerController;
use crate::libs::config::BuzzerTiming;

/// Injectable clock for testing. Returns the current Instant.
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

fn default_clock() -> Clock {
    Arc::new(|| Instant::now())
}

/// Manages buzzer pattern priority between multiple alarm sources
pub struct BuzzerPriorityManager {
    state: Arc<Mutex<BuzzerPriorityState>>,
    buzzer: Arc<Mutex<BuzzerController>>,
    clock: Clock,
}

struct BuzzerPriorityState {
    /// Is a sensor in critical state?
    sensor_critical_active: bool,
    /// Is battery in critical state?
    battery_critical_active: bool,
    /// Buzzer silenced by ACK (resets on next NEW alarm transition)
    silenced: bool,
    /// Which pattern is currently playing?
    current_pattern_source: PatternSource,
    /// When did we switch to the current pattern?
    pattern_switch_time: Instant,
    /// Duration to play current pattern before switching (for interleaving)
    pattern_duration: Duration,
    /// Last pattern we set to avoid redundant updates
    last_set_pattern: Option<PatternSource>,
    /// Button silence deadline (sensor only). None = not silenced by button.
    sensor_silenced_until: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PatternSource {
    None,
    SensorCritical,
    BatteryCritical,
}

impl BuzzerPriorityState {
    fn new() -> Self {
        Self {
            sensor_critical_active: false,
            battery_critical_active: false,
            silenced: false,
            current_pattern_source: PatternSource::None,
            pattern_switch_time: Instant::now(),
            pattern_duration: Duration::from_secs(2),
            last_set_pattern: None,
            sensor_silenced_until: None,
        }
    }
}

impl BuzzerPriorityManager {
    /// Create a new buzzer priority manager
    pub fn new(buzzer: Arc<Mutex<BuzzerController>>) -> Self {
        Self {
            state: Arc::new(Mutex::new(BuzzerPriorityState::new())),
            buzzer,
            clock: default_clock(),
        }
    }

    #[cfg(test)]
    fn new_with_clock(buzzer: Arc<Mutex<BuzzerController>>, clock: Clock) -> Self {
        Self {
            state: Arc::new(Mutex::new(BuzzerPriorityState::new())),
            buzzer,
            clock,
        }
    }

    /// Set battery critical state
    /// When true, battery buzzer will sound (unless sensor critical takes priority)
    pub fn set_battery_critical(&self, is_critical: bool) {
        // Decide what pattern to use - lock state only briefly
        let pattern_to_set = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.battery_critical_active = is_critical;
            eprintln!(
                "[BuzzerPriority] Battery critical: {}",
                if is_critical { "ON" } else { "OFF" }
            );
            // Compute decision without holding locks
            self.compute_pattern(&state)
        }; // Release state lock here

        // Update buzzer if pattern changed
        self.apply_pattern(pattern_to_set);
    }

    /// Set sensor critical state
    /// When true, sensor buzzer will sound with highest priority
    pub fn set_sensor_critical(&self, is_critical: bool) {
        // Decide what pattern to use - lock state only briefly
        let pattern_to_set = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let was_critical = state.sensor_critical_active;
            state.sensor_critical_active = is_critical;
            // New alarm transition (off→on) clears silence
            if is_critical && !was_critical {
                state.silenced = false;
            }
            eprintln!(
                "[BuzzerPriority] Sensor critical: {}",
                if is_critical { "ON" } else { "OFF" }
            );
            // Compute decision without holding locks
            self.compute_pattern(&state)
        }; // Release state lock here

        // Update buzzer if pattern changed
        self.apply_pattern(pattern_to_set);
    }

    /// Silence the buzzer (from alarm ACK). Stops current pattern but re-arms
    /// for new alarms. The silence is cleared when a new alarm triggers.
    pub fn silence(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.silenced = true;
            state.last_set_pattern = Some(PatternSource::None);
            eprintln!("[BuzzerPriority] Buzzer silenced by ACK");
        }
        if let Ok(buzzer) = self.buzzer.lock() {
            buzzer.stop();
        }
    }

    /// Silence sensor beep for 30 minutes via physical button.
    /// Only suppresses SensorCritical pattern; battery continues.
    /// Cleared by timer expiry or `on_new_sensor_alarm()`.
    pub fn silence_sensor_30min(&self) {
        let pattern_to_set = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let deadline = (self.clock)() + Duration::from_secs(30 * 60);
            state.sensor_silenced_until = Some(deadline);
            state.last_set_pattern = None; // Force re-evaluation
            eprintln!("[BuzzerPriority] Sensor buzzer silenced by button for 30 min");
            self.compute_pattern(&state)
        };
        self.apply_pattern(pattern_to_set);
    }

    /// Called when a specific sensor transitions into critical/disconnected.
    /// Clears the button silence so the user hears the new alarm.
    /// Does NOT clear MQTT ACK silence (`silenced` field).
    pub fn on_new_sensor_alarm(&self) {
        let pattern_to_set = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.sensor_silenced_until.is_some() {
                state.sensor_silenced_until = None;
                state.last_set_pattern = None; // Force re-evaluation
                eprintln!("[BuzzerPriority] Button silence cleared — new sensor alarm");
                self.compute_pattern(&state)
            } else {
                None // No change needed
            }
        };
        self.apply_pattern(pattern_to_set);
    }

    /// Returns true if the sensor-critical pattern is currently audible.
    /// Used by the button handler to decide whether to consume a press.
    pub fn is_sensor_beeping(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.sensor_critical_active {
            return false;
        }
        if state.silenced {
            return false;
        }
        if let Some(deadline) = state.sensor_silenced_until {
            if (self.clock)() < deadline {
                return false;
            }
        }
        true
    }

    /// Returns true if the button-triggered sensor silence is active.
    /// Used by the display renderer to show the mute icon.
    pub fn is_button_silenced(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(deadline) = state.sensor_silenced_until {
            (self.clock)() < deadline
        } else {
            false
        }
    }

    /// Compute which pattern should be playing based on current state
    /// This function does NOT lock anything - it's read-only logic
    fn compute_pattern(&self, state: &BuzzerPriorityState) -> Option<PatternSource> {
        // If silenced by ACK, don't play any pattern
        if state.silenced {
            return Some(PatternSource::None);
        }

        // Check button silence (sensor-only, time-limited)
        let sensor_active = if let Some(deadline) = state.sensor_silenced_until {
            if (self.clock)() >= deadline {
                state.sensor_critical_active
            } else {
                false // Sensor suppressed by button silence
            }
        } else {
            state.sensor_critical_active
        };

        let new_pattern_source = match (
            sensor_active,
            state.battery_critical_active,
        ) {
            // Only sensor critical: play sensor pattern
            (true, false) => {
                //eprintln!("[BuzzerPriority] Decision: Sensor critical (priority)");
                Some(PatternSource::SensorCritical)
            }
            // Only battery critical: play battery pattern
            (false, true) => {
                //eprintln!("[BuzzerPriority] Decision: Battery critical");
                Some(PatternSource::BatteryCritical)
            }
            // Both critical: alternate between patterns every 2 seconds
            (true, true) => {
                let elapsed = (self.clock)().duration_since(state.pattern_switch_time);
                if elapsed > state.pattern_duration {
                    // Time to switch patterns
                    let next = if state.current_pattern_source == PatternSource::SensorCritical {
                        eprintln!("[BuzzerPriority] Decision: Switching to Battery critical");
                        PatternSource::BatteryCritical
                    } else {
                        eprintln!("[BuzzerPriority] Decision: Switching to Sensor critical");
                        PatternSource::SensorCritical
                    };
                    Some(next)
                } else {
                    // Keep current pattern - return None to indicate no change
                    None
                }
            }
            // Neither critical: stop buzzer
            (false, false) => {
                eprintln!("[BuzzerPriority] Decision: Stop buzzer (all clear)");
                Some(PatternSource::None)
            }
        };

        new_pattern_source
    }

    /// Apply a pattern change if needed
    /// Only locks buzzer when actually changing the pattern
    fn apply_pattern(&self, new_pattern: Option<PatternSource>) {
        match new_pattern {
            Some(pattern_source) => {
                // Check if this is actually a change from what we last set
                let should_update = {
                    let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.last_set_pattern != Some(pattern_source)
                }; // Release state lock

                if should_update {
                    // Lock buzzer only to update pattern
                    if let Ok(buzzer) = self.buzzer.lock() {
                        match pattern_source {
                            PatternSource::SensorCritical => {
                                let timing = BuzzerTiming {
                                    on_ms: 200,
                                    off_ms: 100,
                                };
                                buzzer.set_repeating_pattern(BuzzerPattern::CriticalBeep(timing));
                            }
                            PatternSource::BatteryCritical => {
                                let timing = BuzzerTiming {
                                    on_ms: 200,
                                    off_ms: 100,
                                };
                                buzzer.set_repeating_pattern(BuzzerPattern::CriticalBeep(timing));
                            }
                            PatternSource::None => {
                                buzzer.stop();
                            }
                        }
                    } // Release buzzer lock

                    // Update state with what we just set
                    if let Ok(mut state) = self.state.lock() {
                        state.last_set_pattern = Some(pattern_source);
                        state.current_pattern_source = pattern_source;
                        state.pattern_switch_time = (self.clock)();
                    }
                }
            }
            None => {
                // Pattern decision returned None - means keep current pattern
                // Nothing to do
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    /// Create a mock clock where time can be advanced manually.
    fn mock_clock() -> (Clock, Arc<dyn Fn(Duration) + Send + Sync>) {
        let base = Instant::now();
        let offset_ms = Arc::new(AtomicU64::new(0));
        let offset_clone = offset_ms.clone();

        let clock: Clock = Arc::new(move || {
            base + Duration::from_millis(offset_ms.load(AtomicOrdering::Relaxed))
        });

        let advance: Arc<dyn Fn(Duration) + Send + Sync> = Arc::new(move |d: Duration| {
            offset_clone.fetch_add(d.as_millis() as u64, AtomicOrdering::Relaxed);
        });

        (clock, advance)
    }

    /// Helper: evaluate sensor_active given state and clock (mirrors compute_pattern logic)
    fn eval_sensor_active(state: &BuzzerPriorityState, clock: &Clock) -> bool {
        if let Some(deadline) = state.sensor_silenced_until {
            if (clock)() >= deadline {
                state.sensor_critical_active
            } else {
                false
            }
        } else {
            state.sensor_critical_active
        }
    }

    /// Helper: evaluate is_sensor_beeping logic
    fn eval_is_sensor_beeping(state: &BuzzerPriorityState, clock: &Clock) -> bool {
        if !state.sensor_critical_active { return false; }
        if state.silenced { return false; }
        if let Some(deadline) = state.sensor_silenced_until {
            if (clock)() < deadline { return false; }
        }
        true
    }

    /// Helper: evaluate pattern source given sensor_active and battery_active
    fn eval_pattern(sensor_active: bool, battery_active: bool) -> PatternSource {
        match (sensor_active, battery_active) {
            (true, false) => PatternSource::SensorCritical,
            (false, true) => PatternSource::BatteryCritical,
            (true, true) => PatternSource::SensorCritical, // sensor takes priority initially
            (false, false) => PatternSource::None,
        }
    }

    #[test]
    fn silence_sensor_30min_suppresses_sensor_only() {
        let (clock, _advance) = mock_clock();
        let mut state = BuzzerPriorityState::new();
        state.sensor_critical_active = true;
        state.battery_critical_active = true;
        state.sensor_silenced_until = Some((clock)() + Duration::from_secs(30 * 60));

        let sensor_active = eval_sensor_active(&state, &clock);
        assert!(!sensor_active, "sensor should be suppressed by button silence");
        assert!(state.battery_critical_active, "battery should NOT be suppressed");

        let pattern = eval_pattern(sensor_active, state.battery_critical_active);
        assert_eq!(pattern, PatternSource::BatteryCritical);
    }

    #[test]
    fn silence_sensor_expires_after_30min() {
        let (clock, advance) = mock_clock();
        let mut state = BuzzerPriorityState::new();
        state.sensor_critical_active = true;
        state.sensor_silenced_until = Some((clock)() + Duration::from_secs(30 * 60));

        // Before expiry
        assert!(!eval_sensor_active(&state, &clock));

        // Advance past 30 minutes
        advance(Duration::from_secs(31 * 60));

        // After expiry
        assert!(eval_sensor_active(&state, &clock), "sensor should resume after 30min");
    }

    #[test]
    fn on_new_sensor_alarm_clears_button_silence() {
        let (clock, _advance) = mock_clock();
        let mut state = BuzzerPriorityState::new();
        state.sensor_critical_active = true;
        state.sensor_silenced_until = Some((clock)() + Duration::from_secs(30 * 60));

        // Simulate on_new_sensor_alarm: clears sensor_silenced_until
        state.sensor_silenced_until = None;

        assert!(eval_sensor_active(&state, &clock), "sensor should resume after new alarm");
    }

    #[test]
    fn on_new_sensor_alarm_does_not_clear_mqtt_silence() {
        let (_clock, _advance) = mock_clock();
        let mut state = BuzzerPriorityState::new();
        state.sensor_critical_active = true;
        state.silenced = true; // MQTT ACK

        // Simulate on_new_sensor_alarm: only clears sensor_silenced_until
        state.sensor_silenced_until = None; // Already None

        assert!(state.silenced, "MQTT silence should NOT be cleared by new sensor alarm");
    }

    #[test]
    fn mqtt_silence_still_mutes_battery() {
        let mut state = BuzzerPriorityState::new();
        state.battery_critical_active = true;
        state.silenced = true; // MQTT ACK

        // MQTT silence mutes everything (checked first in compute_pattern)
        assert!(state.silenced);
        // In compute_pattern, `if state.silenced { return Some(PatternSource::None); }`
        // so pattern is None regardless of battery state
    }

    #[test]
    fn button_silence_does_not_mute_battery() {
        let (clock, _advance) = mock_clock();
        let mut state = BuzzerPriorityState::new();
        state.battery_critical_active = true;
        state.sensor_critical_active = false;
        state.sensor_silenced_until = Some((clock)() + Duration::from_secs(30 * 60));

        let sensor_active = eval_sensor_active(&state, &clock);
        let pattern = eval_pattern(sensor_active, state.battery_critical_active);
        assert_eq!(pattern, PatternSource::BatteryCritical,
            "battery should still beep when only button silence is active");
    }

    #[test]
    fn is_sensor_beeping_reflects_both_silences() {
        let (clock, _advance) = mock_clock();

        // Sensor active, no silence → beeping
        let mut state = BuzzerPriorityState::new();
        state.sensor_critical_active = true;
        assert!(eval_is_sensor_beeping(&state, &clock));

        // Sensor active, MQTT silenced → not beeping
        state.silenced = true;
        assert!(!eval_is_sensor_beeping(&state, &clock));

        // Sensor active, button silenced (not MQTT) → not beeping
        state.silenced = false;
        state.sensor_silenced_until = Some((clock)() + Duration::from_secs(1800));
        assert!(!eval_is_sensor_beeping(&state, &clock));

        // Sensor inactive → not beeping
        state.sensor_critical_active = false;
        state.sensor_silenced_until = None;
        state.silenced = false;
        assert!(!eval_is_sensor_beeping(&state, &clock));

        // Sensor active, both silenced → not beeping
        state.sensor_critical_active = true;
        state.silenced = true;
        state.sensor_silenced_until = Some((clock)() + Duration::from_secs(1800));
        assert!(!eval_is_sensor_beeping(&state, &clock));
    }
}
