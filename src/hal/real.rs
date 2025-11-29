// src/hal/real.rs
use crate::drivers::buzzer::Buzzer;
use crate::drivers::stm::StmBridge;
use crate::hal::{BuzzerHal, LedHal, LedState, SensorLedHal, SensorLedState};
use std::cell::RefCell;
use std::rc::Rc;

/// Real buzzer HAL using your GPIO-based Buzzer driver.
pub struct GpioBuzzerHal {
    buzzer: Buzzer,
}

impl GpioBuzzerHal {
    pub fn new() -> anyhow::Result<Self> {
        let buzzer = Buzzer::new()?;
        Ok(Self { buzzer })
    }
}

impl BuzzerHal for GpioBuzzerHal {
    fn set_on(&mut self) {
        self.buzzer.on();
    }

    fn set_off(&mut self) {
        self.buzzer.off();
    }
}

/// Real PWRLED HAL using your STM bridge.
///
/// Mapping:
/// - LedState::Off   => PWRLEDG OFF, PWRLEDY OFF
/// - LedState::Green => PWRLEDG ON,  PWRLEDY OFF
/// - LedState::Yellow=> PWRLEDG OFF, PWRLEDY ON
/// - LedState::Red   => PWRLEDG OFF, PWRLEDY ON  (reuse yellow as “alarm”)
pub struct StmLedHal {
    stm: Rc<RefCell<StmBridge>>,
}

impl StmLedHal {
    pub fn new(stm: Rc<RefCell<StmBridge>>) -> Self {
        Self { stm }
    }
}

impl LedHal for StmLedHal {
    fn set_led_state(&mut self, state: LedState) {
        use LedState::*;

        let (g, y) = match state {
            Off => (false, false),
            Green => (true, false),
            Yellow => (false, true),
            Red => (false, true),
        };

        if let Err(e) = self.stm.borrow_mut().set_pwr_leds(g, y) {
            eprintln!("[StmLedHal] Failed to set PWR LEDs: {}", e);
        }
    }
}

/// Real per-sensor LED HAL using STM line LEDs.
///
/// Mapping:
/// - SensorLedState::Off   => both off
/// - SensorLedState::Green => only green on
/// - SensorLedState::Red   => only red on
/// - SensorLedState::Both  => both on
pub struct StmSensorLedHal {
    stm: Rc<RefCell<StmBridge>>,
}

impl StmSensorLedHal {
    pub fn new(stm: Rc<RefCell<StmBridge>>) -> Self {
        Self { stm }
    }
}

impl SensorLedHal for StmSensorLedHal {
    fn set_sensor_led(&mut self, index: u8, state: SensorLedState) {
        use SensorLedState::*;

        let (g, r) = match state {
            Off => (false, false),
            Green => (true, false),
            Red => (false, true),
            Both => (true, true),
        };

        if let Err(e) = self.stm.borrow_mut().set_line_leds(index, g, r) {
            eprintln!("[StmSensorLedHal] Failed to set line LED {}: {}", index, e);
        }
    }
}
