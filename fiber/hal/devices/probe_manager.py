import copy
import time

from fiber.hal.consts import PROBE_INDICATOR_CONFIG, GPIO_LED_ENABLE, INDICATOR_ON_ADDR, INDICATOR_OFF_ADDR, POWER_LED_ADDR, PROBE_LED_ADDR_1, PROBE_LED_ADDR_2, GPIO_POWER_LED
from fiber.common.consts import INDICATOR_ON_TAG, INDICATOR_OFF_TAG, INDICATOR_GREEN, INDICATOR_RED
from fiber.common.gpio_manager import gpio_manager

from loguru import logger
from fiber.hal.i2c import I2C
from gpiod.line import Value, Direction

# HAL > hardware abstraction layer

class ProbePower:
    def __init__(self, i2c: I2C | None) -> None:
        if not isinstance(i2c, I2C):
            raise TypeError
        
        gpio_manager.add_pin(GPIO_POWER_LED, Direction.OUTPUT)
        self._i2c = i2c

        try:
            self._i2c.write_byte_data(POWER_LED_ADDR, 0x01, INDICATOR_ON_ADDR)
            self._i2c.write_byte_data(POWER_LED_ADDR, 0x03, INDICATOR_ON_ADDR)
        except OSError:
            raise 


class ProbeLEDs:
    def __init__(self, i2c: I2C | None) -> None:
        if not isinstance(i2c, I2C):
            raise TypeError
        
        gpio_manager.add_pin(GPIO_LED_ENABLE, Direction.OUTPUT)

        self._i2c = i2c
        self._prev = {}
        self._config = {}

        try:
            gpio_manager.set_value(GPIO_LED_ENABLE, Value.ACTIVE)
            time.sleep(0.01)
            self._i2c.write_byte_data(PROBE_LED_ADDR_1, 0x17, INDICATOR_ON_ADDR)
            self._i2c.write_byte_data(PROBE_LED_ADDR_2, 0x17, INDICATOR_ON_ADDR)
            time.sleep(0.01)
            self._i2c.write_byte_data(PROBE_LED_ADDR_1, 0x00, INDICATOR_ON_ADDR)
            self._i2c.write_byte_data(PROBE_LED_ADDR_2, 0x00, INDICATOR_ON_ADDR)
        except OSError:
            raise 

    def set_probe_led(self, probe: int, state: str) -> None:
        if probe not in self._config:

            self._config[probe] = copy.deepcopy(PROBE_INDICATOR_CONFIG[probe])

        if state in [INDICATOR_RED, INDICATOR_GREEN]:
            self._handle_single_color_state(probe, state)
        elif state == INDICATOR_OFF_TAG:
            self._activate_or_turn_off_colors(probe, activate=False)
        elif state == INDICATOR_ON_TAG:
            self._activate_or_turn_off_colors(probe, activate=True)
        else:
            logger.error(
                f"Unexpected state value: {state}. "
                f"Expected values are INDICATOR_RED, INDICATOR_GREEN, "
                f"INDICATOR_ON, or INDICATOR_OFF."
            )
            raise ValueError

    def _handle_single_color_state(self, probe: int, state: str) -> None:
        if probe not in self._prev:
            self._prev[probe] = {INDICATOR_RED: 0, INDICATOR_GREEN: 0}

        for color in [INDICATOR_RED, INDICATOR_GREEN]:
            self._config[probe][color]["value"] = INDICATOR_OFF_ADDR
            self._config[probe][color]["disabled"] = color != state
            self._prev[probe][color] = 0

        self._config[probe][state]["value"] = INDICATOR_ON_ADDR
        self._config[probe][state]["disabled"] = False
        self._prev[probe][state] = self._config[probe][state]["value"]

    def _activate_or_turn_off_colors(self, probe: int, activate: bool = True) -> None:
        if activate:
            prev_values = self._prev.get(probe)
            if prev_values:
                for color in [INDICATOR_RED, INDICATOR_GREEN]:
                    self._config[probe][color]["value"] = prev_values[color]

            for color in [INDICATOR_RED, INDICATOR_GREEN]:
                self._config[probe][color]["disabled"] = False
        else:
            for color in [INDICATOR_RED, INDICATOR_GREEN]:
                self._config[probe][color]["disabled"] = True

    def apply(self) -> None:
        for _, config in self._config.items():
            try:
                red_config = config["red"]
                green_config = config["green"]

                red_value = red_config["value"] if not red_config["disabled"] else INDICATOR_OFF_ADDR
                green_value = green_config["value"] if not green_config["disabled"] else INDICATOR_OFF_ADDR

                self._i2c.write_byte_data(
                    red_config["address"],
                    red_config["register"],
                    red_value,
                )

                self._i2c.write_byte_data(
                    green_config["address"],
                    green_config["register"],
                    green_value,
                )

            except OSError:
                raise

        self._config = {}