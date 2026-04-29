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
        // Compare gravity-compensated motion intensity against the threshold so
        // a stationary device (≈1 g total magnitude) does not trip the detector.
        let above_threshold = Self::motion_intensity(accel) > self.threshold_g;

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
    pub fn magnitude(accel: &AccelData) -> f32 {
        (accel.x_g * accel.x_g + accel.y_g * accel.y_g + accel.z_g * accel.z_g).sqrt()
    }

    /// Gravity-compensated motion intensity in g.
    /// A stationary device reads ~1 g total, so subtracting that baseline gives
    /// the magnitude of motion-induced acceleration regardless of orientation.
    pub fn motion_intensity(accel: &AccelData) -> f32 {
        (Self::magnitude(accel) - 1.0).abs()
    }

    /// Identify which of the six box orientations the device is closest to,
    /// based on the dominant gravity axis.
    ///
    /// 1 = top up (+Z), 2 = top down (-Z),
    /// 3 = right up (+X), 4 = left up (-X),
    /// 5 = front up (+Y), 6 = back up (-Y).
    pub fn position(accel: &AccelData) -> u8 {
        let ax = accel.x_g.abs();
        let ay = accel.y_g.abs();
        let az = accel.z_g.abs();

        if az >= ax && az >= ay {
            if accel.z_g >= 0.0 { 1 } else { 2 }
        } else if ax >= ay {
            if accel.x_g >= 0.0 { 3 } else { 4 }
        } else if accel.y_g >= 0.0 {
            5
        } else {
            6
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_magnitude_calculation() {
        let accel = AccelData {
            x_g: 3.0,
            y_g: 4.0,
            z_g: 0.0,
        };
        // 3-4-5 triangle
        assert!((MotionDetector::magnitude(&accel) - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_gravity_baseline_does_not_trigger() {
        let mut detector = MotionDetector::new(0.3, 1);
        let gravity_only = AccelData {
            x_g: 0.0,
            y_g: 0.0,
            z_g: 1.0, // pure gravity
        };
        // Intensity = |1.0 - 1.0| = 0, below threshold → stay Idle.
        let (state, changed) = detector.update(&gravity_only);
        assert_eq!(state, MotionState::Idle);
        assert!(!changed);
    }

    #[test]
    fn test_motion_detection_simple() {
        let mut detector = MotionDetector::new(0.5, 1);
        // Slight acceleration above gravity (1.4g total → intensity 0.4) - below threshold.
        let weak = AccelData {
            x_g: 0.0,
            y_g: 0.0,
            z_g: 1.4,
        };
        let (state, changed) = detector.update(&weak);
        assert_eq!(state, MotionState::Idle);
        assert!(!changed);

        // Strong acceleration (2.0g total → intensity 1.0) - above threshold.
        let strong = AccelData {
            x_g: 0.0,
            y_g: 0.0,
            z_g: 2.0,
        };
        let (state, changed) = detector.update(&strong);
        assert_eq!(state, MotionState::Moving);
        assert!(changed);
    }

    #[test]
    fn test_debouncing() {
        let mut detector = MotionDetector::new(0.3, 3);
        // Magnitude 1.5g → intensity 0.5g, above 0.3 threshold.
        let motion = AccelData {
            x_g: 0.0,
            y_g: 0.0,
            z_g: 1.5,
        };
        // Pure gravity → intensity 0, below threshold.
        let idle = AccelData {
            x_g: 0.0,
            y_g: 0.0,
            z_g: 1.0,
        };

        let (state, _) = detector.update(&motion);
        assert_eq!(state, MotionState::Debouncing { samples: 1 });

        let (state, _) = detector.update(&motion);
        assert_eq!(state, MotionState::Debouncing { samples: 2 });

        let (state, changed) = detector.update(&motion);
        assert_eq!(state, MotionState::Moving);
        assert!(changed);

        let (state, changed) = detector.update(&idle);
        assert_eq!(state, MotionState::Idle);
        assert!(changed);
    }

    #[test]
    fn test_debounce_reset_on_below_threshold() {
        let mut detector = MotionDetector::new(0.5, 3);
        // Intensity = |1.6 - 1.0| = 0.6 > 0.5 threshold.
        let motion = AccelData {
            x_g: 0.0,
            y_g: 0.0,
            z_g: 1.6,
        };

        detector.update(&motion);
        detector.update(&motion);

        // Pure gravity → intensity 0, below threshold.
        let resting = AccelData {
            x_g: 0.0,
            y_g: 0.0,
            z_g: 1.0,
        };

        let (state, _) = detector.update(&resting);
        assert_eq!(state, MotionState::Idle);
    }
}
