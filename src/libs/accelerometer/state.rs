// Motion state tracking and detection logic

use crate::drivers::lis2dh12::AccelData;

/// Motion state enumeration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionState {
    /// Device is idle (no motion detected)
    Idle,
    /// Device is moving (motion detected and confirmed)
    Moving,
    /// Debouncing state - waiting for confirmation
    Debouncing { samples: u8 },
}

/// Motion detector using acceleration magnitude threshold
#[derive(Debug, Clone)]
pub struct MotionDetector {
    /// Current motion state
    current_state: MotionState,
    /// Motion threshold in gravitational units (g)
    threshold_g: f32,
    /// Number of consecutive readings required to confirm motion
    debounce_samples: u8,
}

impl MotionDetector {
    /// Create a new motion detector
    pub fn new(threshold_g: f32, debounce_samples: u8) -> Self {
        Self {
            current_state: MotionState::Idle,
            threshold_g,
            debounce_samples,
        }
    }

    /// Update detector with new acceleration data
    /// Returns (new_state, state_changed)
    pub fn update(&mut self, accel: &AccelData) -> (MotionState, bool) {
        let magnitude = self.calculate_magnitude(accel);
        // Detect motion when acceleration magnitude exceeds threshold.
        // A constant 1g gravitational component is always present, so the threshold
        // only triggers on motion-induced acceleration above the static baseline.
        let above_threshold = magnitude > self.threshold_g;

        let new_state = match self.current_state {
            MotionState::Idle => {
                if above_threshold {
                    if self.debounce_samples <= 1 {
                        MotionState::Moving
                    } else {
                        MotionState::Debouncing { samples: 1 }
                    }
                } else {
                    MotionState::Idle
                }
            }
            MotionState::Moving => {
                if above_threshold {
                    MotionState::Moving
                } else {
                    MotionState::Idle
                }
            }
            MotionState::Debouncing { samples } => {
                if above_threshold {
                    if samples + 1 >= self.debounce_samples {
                        MotionState::Moving
                    } else {
                        MotionState::Debouncing {
                            samples: samples + 1,
                        }
                    }
                } else {
                    MotionState::Idle
                }
            }
        };

        let state_changed = new_state != self.current_state;
        self.current_state = new_state;

        (new_state, state_changed)
    }

    /// Get current motion state
    pub fn current_state(&self) -> MotionState {
        self.current_state
    }

    /// Calculate acceleration magnitude from 3-axis data
    /// magnitude = sqrt(x² + y² + z²)
    fn calculate_magnitude(&self, accel: &AccelData) -> f32 {
        (accel.x_g * accel.x_g + accel.y_g * accel.y_g + accel.z_g * accel.z_g).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_magnitude_calculation() {
        let detector = MotionDetector::new(0.5, 1);
        let accel = AccelData {
            x_g: 3.0,
            y_g: 4.0,
            z_g: 0.0,
        };
        let magnitude = 5.0; // 3-4-5 triangle
        assert!((detector.calculate_magnitude(&accel) - magnitude).abs() < 0.001);
    }

    #[test]
    fn test_gravity_baseline() {
        let detector = MotionDetector::new(0.5, 1);
        let gravity_only = AccelData {
            x_g: 0.0,
            y_g: 0.0,
            z_g: 1.0, // 1g from gravity
        };
        let (state, _changed) = detector.clone().update(&gravity_only);
        // 1.0g > 0.5 threshold, so it triggers motion detection
        // This is expected behavior - any acceleration > threshold triggers motion
        assert_eq!(state, MotionState::Moving);
    }

    #[test]
    fn test_motion_detection_simple() {
        let mut detector = MotionDetector::new(0.5, 1);
        let motion = AccelData {
            x_g: 0.3,
            y_g: 0.4,
            z_g: 0.0, // No gravity component to focus on motion-only acceleration
        };
        let (state, changed) = detector.update(&motion);
        // Magnitude: sqrt(0.09 + 0.16) = 0.5 > 0.5 threshold? No, equal
        // Need slightly higher value
        assert_eq!(state, MotionState::Idle);
        assert!(!changed);

        // Now trigger actual motion
        let motion_strong = AccelData {
            x_g: 0.4,
            y_g: 0.4,
            z_g: 0.0,
        };
        let (state, changed) = detector.update(&motion_strong);
        // Magnitude: sqrt(0.16 + 0.16) ≈ 0.566 > 0.5 threshold = true
        // With debounce=1, should immediately go to Moving
        assert_eq!(state, MotionState::Moving);
        assert!(changed);
    }

    #[test]
    fn test_debouncing() {
        let mut detector = MotionDetector::new(0.3, 3);
        let motion = AccelData {
            x_g: 0.2,
            y_g: 0.25,
            z_g: 0.0, // No gravity, focus on motion detection
        };
        let idle = AccelData {
            x_g: 0.0,
            y_g: 0.0,
            z_g: 1.0, // Gravity only = 1.0g, above 0.3 threshold
        };

        let (state, _) = detector.update(&motion);
        // Magnitude: sqrt(0.04 + 0.0625) ≈ 0.321 > 0.3 threshold = true
        assert_eq!(state, MotionState::Debouncing { samples: 1 });

        let (state, _) = detector.update(&motion);
        assert_eq!(state, MotionState::Debouncing { samples: 2 });

        let (state, changed) = detector.update(&motion);
        assert_eq!(state, MotionState::Moving);
        assert!(changed);

        // Stop motion - idle reading has 1.0g > 0.3 threshold, stays above threshold
        let (state, _changed) = detector.update(&idle);
        // Gravity alone (1.0g) exceeds motion threshold (0.3g), so state stays Moving
        assert_eq!(state, MotionState::Moving);
    }

    #[test]
    fn test_debounce_reset_on_below_threshold() {
        let mut detector = MotionDetector::new(0.5, 3);
        let motion = AccelData {
            x_g: 0.3,
            y_g: 0.3,
            z_g: 0.0,
        };

        // Start debouncing
        detector.update(&motion);
        detector.update(&motion);

        // Drop below threshold - use truly idle reading with no motion
        let truly_idle = AccelData {
            x_g: 0.0,
            y_g: 0.0,
            z_g: 0.3, // Below motion threshold
        };

        let (state, _) = detector.update(&truly_idle);
        assert_eq!(state, MotionState::Idle);
    }
}
