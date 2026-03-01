//! Buzzer pattern definitions and types

use crate::libs::config::BuzzerTiming;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

/// Buzzer control patterns with configurable timings
#[derive(Debug, Clone, PartialEq)]
pub enum BuzzerPattern {
    /// Buzzer is off (inactive)
    Off,
    /// Disconnected beep: repeating pattern (e.g., 1000ms on, 2000ms off)
    DisconnectedBeep(BuzzerTiming),
    /// Critical beep: urgent pattern (e.g., 500ms on, 500ms off)
    CriticalBeep(BuzzerTiming),
    /// Battery mode beep: reminder beep when on battery power (e.g., 100ms on, 100ms off)
    BatteryModeBeep(BuzzerTiming),
    /// VIN disconnect beep: long single beep when AC power is lost (e.g., 2000ms on)
    VinDisconnectBeep(BuzzerTiming),
    /// Happy reconnection beep: celebratory 3-beep pattern with PWM
    /// Plays once (1050ms total) then automatically stops
    ReconnectionHappy {
        /// Frequency in Hz for PWM during beeps (e.g., 150 Hz for nice tone)
        frequency_hz: u32,
    },
}

/// Internal buzzer state (pattern and timing)
#[derive(Debug, Clone)]
pub struct BuzzerStateInner {
    /// Current pattern being played
    pub pattern: BuzzerPattern,
    /// When this pattern started (for timing calculations)
    pub pattern_start_time: Instant,
}

impl BuzzerStateInner {
    /// Create new buzzer state with Off pattern
    pub fn new() -> Self {
        Self {
            pattern: BuzzerPattern::Off,
            pattern_start_time: Instant::now(),
        }
    }
}

impl Default for BuzzerStateInner {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared buzzer volume handle for thread-safe volume control
pub type SharedBuzzerVolume = std::sync::Arc<AtomicU8>;

/// Shared buzzer state with notification (wrapper for thread coordination)
pub struct SharedBuzzerState {
    /// Internal state protected by mutex
    state: Mutex<BuzzerStateInner>,
    /// Condition variable for notifying the buzzer thread of pattern changes
    notify: Condvar,
    /// Buzzer volume (0 = muted, 1-100 = active). Shared with external consumers.
    volume: SharedBuzzerVolume,
}

impl SharedBuzzerState {
    /// Create new buzzer state with Off pattern and default volume (100%)
    pub fn new() -> Self {
        Self {
            state: Mutex::new(BuzzerStateInner::new()),
            notify: Condvar::new(),
            volume: std::sync::Arc::new(AtomicU8::new(100)),
        }
    }

    /// Create new buzzer state with an externally-shared volume handle.
    /// This allows the MQTT monitor to update the volume without direct access to the buzzer state.
    pub fn new_with_volume(volume: SharedBuzzerVolume) -> Self {
        Self {
            state: Mutex::new(BuzzerStateInner::new()),
            notify: Condvar::new(),
            volume,
        }
    }

    /// Get a copy of the current pattern and timing info
    pub fn read(&self) -> BuzzerStateInner {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Update pattern and notify the buzzer thread
    pub fn set_pattern(&self, pattern: BuzzerPattern) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.pattern = pattern;
            state.pattern_start_time = Instant::now();
        }
        self.notify.notify_one();
    }

    /// Wait for pattern change notification
    /// If a pattern is active, use a timeout for periodic timing updates
    /// If no pattern is active, wait indefinitely for the next pattern
    pub fn wait_for_event(&self) {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // If buzzer is off, wait indefinitely for next pattern change
        // Otherwise, use 50ms timeout to check pattern timing
        let timeout = if guard.pattern == BuzzerPattern::Off {
            std::time::Duration::from_secs(u64::MAX)  // Wait indefinitely (effectively)
        } else {
            std::time::Duration::from_millis(50)  // Check timing every 50ms
        };

        let _ = self.notify.wait_timeout(guard, timeout);
    }

    /// Get the current buzzer volume (0 = muted, 1-100 = active)
    pub fn get_volume(&self) -> u8 {
        self.volume.load(Ordering::Relaxed)
    }

    /// Set the buzzer volume (0 = muted, 1-100 = active)
    /// Wakes the buzzer thread so it can apply the new volume immediately.
    pub fn set_volume(&self, volume: u8) {
        let clamped = volume.min(100);
        self.volume.store(clamped, Ordering::Relaxed);
        self.notify.notify_one();
    }
}

impl Default for SharedBuzzerState {
    fn default() -> Self {
        Self::new()
    }
}
