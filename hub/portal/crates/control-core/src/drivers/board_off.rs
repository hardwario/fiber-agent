//! The last thing a power-off does to the board (plan/system/factory_test.md §5.2).
//!
//! `systemctl poweroff` halts the CM4 but leaves the 5 V and 3.3 V rails up, and a
//! halted CM4 keeps its GPIOs where they were. Once `fiber_app` has exited, rppal
//! has handed the backlight pin back, so the panel lights up again with its last
//! frame, and the Southbridge keeps the line LEDs and sensor rails as it had them.
//! [`hold_board_off`] runs from the system-shutdown hook, after every service has
//! stopped: backlight PWM low, Southbridge held in reset (its P0–P7 rail enables
//! have 100k pull-downs, its LEDs go dark). The next boot undoes both: the display
//! driver sets the backlight and `StmBridge::new` releases the reset.

use rppal::gpio::Gpio;

use super::display::BL_PIN;
use super::stm::{STM_BOOT0_GPIO, STM_RESET_GPIO};

/// Drive the backlight off and hold the Southbridge in reset, and leave both
/// pins so when this process exits.
pub fn hold_board_off() -> rppal::gpio::Result<()> {
    let gpio = Gpio::new()?;
    let mut bl = gpio.get(BL_PIN)?.into_output_low();
    bl.set_reset_on_drop(false);
    // BOOT0 low first, so a reset released by anything but our own boot path
    // still starts the Southbridge's app, never the ROM bootloader.
    let mut boot0 = gpio.get(STM_BOOT0_GPIO)?.into_output_low();
    boot0.set_reset_on_drop(false);
    let mut nrst = gpio.get(STM_RESET_GPIO)?.into_output_low();
    nrst.set_reset_on_drop(false);
    Ok(())
}
