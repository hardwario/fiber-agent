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

    /// Compute which pattern should be playing based on current state
    /// This function does NOT lock anything - it's read-only logic
    fn compute_pattern(&self, state: &BuzzerPriorityState) -> Option<PatternSource> {
        // If silenced by ACK, don't play any pattern
        if state.silenced {
            return Some(PatternSource::None);
        }

        let new_pattern_source = match (
            state.sensor_critical_active,
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
