import threading
from loguru import logger
from fiber.common.consts import POWER_LED, PROBE_1, PROBE_8, PROBE_INDEX
from fiber.common.southbridge import SouthBridge


class LedControllerError(Exception):
    pass


class LedController:
    def __init__(self, south_bridge: SouthBridge) -> None:
        self._thread = None
        self._lock = threading.RLock()
        self.south_bridge = south_bridge

    def _set_led_colors(self, probe: int, color_green: int, color_red: int) -> None:
        if PROBE_1 <= probe <= PROBE_8 or probe == POWER_LED:
            with self._lock:
                self.south_bridge.set_led(PROBE_INDEX[probe][0], color_green)
                self.south_bridge.set_led(PROBE_INDEX[probe][1], color_red)
                self.south_bridge.flush()
        else:
            logger.error(f"Invalid probe value: {probe}")

    def activate_leds(self) -> None:
        self.on(POWER_LED)
        for probe in range(PROBE_1, PROBE_8 + 1):
            self.red(probe)

    def red(self, probe: int) -> None:
        self._set_led_colors(probe=probe, color_green=0, color_red=100)

    def green(self, probe: int) -> None:
        self._set_led_colors(probe=probe, color_green=100, color_red=0)

    def on(self, probe: int) -> None:
        self._set_led_colors(probe=probe, color_green=100, color_red=0)

    def off(self, probe: int) -> None:
        self._set_led_colors(probe=probe, color_green=0, color_red=0)
