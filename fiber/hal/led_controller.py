import threading
from loguru import logger
from fiber.common.consts import POWER_LED, PROBE_1, PROBE_8, PROBE_INDEX
from fiber.hal.southbridge import south_bridge


class LedControllerError(Exception):
    pass


class LedController:
    def __init__(self) -> None:
        self._thread = None
        self._stop_event = threading.Event()
        self._lock = threading.RLock()

    def _set_led_colors(self, probe: int, color_green: int, color_red: int) -> None:
        if PROBE_1 <= probe <= PROBE_8 or probe == POWER_LED:
            with self._lock:
                south_bridge.set_led(PROBE_INDEX[probe][0], color_green)
                south_bridge.set_led(PROBE_INDEX[probe][1], color_red)
                south_bridge.flush()
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
