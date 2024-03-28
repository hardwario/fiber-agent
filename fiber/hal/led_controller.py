import threading
import time
from loguru import logger
from fiber.hal.consts import LED_REPEAT_CYCLE_MS, GPIO_POWER_LED
from fiber.common.consts import *
from fiber.common.thread_manager import pool
from fiber.hal.devices.probe_manager import ProbeLEDs
from fiber.common.gpio_manager import gpio_manager
from gpiod.line import Value



class LEDDriverError(Exception):
    pass


class LedControllerError(Exception):
    pass


class Controller:
    def __init__(self, probe_led_driver: ProbeLEDs) -> None:
        gpio_manager.set_value(GPIO_POWER_LED, Value.ACTIVE)
        self._thread = None
        self._stop_event = threading.Event()
        self._led_driver = probe_led_driver

        self._registered_blinking = {
            POWER_LED: False,
            PROBE_1: False,
            PROBE_2: False,
            PROBE_3: False,
            PROBE_4: False,
            PROBE_5: False,
            PROBE_6: False,
            PROBE_7: False,
            PROBE_8: False,
        }

        self._lock = threading.RLock()
        try:
            gpio_manager.set_value(GPIO_POWER_LED, Value.ACTIVE)
        except OSError as e:
            logger.error(f"GPIO Chip Initialization Error: {e}")
            raise SystemError(f"GPIO Chip Initialization Error: {e}")

    def run_in_thread(self) -> None:
        if self._thread is not None:
            self._stop_event.set()
            self._thread.join()

        self._stop_event.clear()
        self._thread = threading.Thread(target=self.controller_loop)
        pool.manage_thread(True, self._thread, self._stop_event)
        self._thread.start()

    def controller_loop(self) -> None:
        prev = True
        while not self._stop_event.is_set():
            state = INDICATOR_ON_TAG if prev else INDICATOR_OFF_TAG
            with self._lock:
                self._update_leds(state, prev)
                self._led_driver.apply()

            prev = not prev
            time.sleep(LED_REPEAT_CYCLE_MS)

    def _update_leds(self, state: str, prev: bool) -> None:
        for probe, is_blinking in self._registered_blinking.items():
            if is_blinking:
                if probe != POWER_LED:
                    self._led_driver.set_probe_led(probe, state)
                else:
                    led_state = Value.ACTIVE if prev else Value.INACTIVE
                    gpio_manager.set_value(GPIO_POWER_LED, led_state)

    def _update_led_state(self, probe: int, state: str) -> None:
        with self._lock:
            if probe != POWER_LED:
                self._led_driver.set_probe_led(probe, state)
                self._led_driver.apply()
            else:
                led_state = Value.ACTIVE if state == INDICATOR_ON_TAG else Value.INACTIVE
                gpio_manager.set_value(GPIO_POWER_LED, led_state)

    def red(self, probe: int) -> None:
        self._update_led_state(probe, INDICATOR_RED)

    def green(self, probe: int) -> None:
        self._update_led_state(probe, INDICATOR_GREEN)

    def blink(self, probe: int) -> None:
        with self._lock:
            self._registered_blinking[probe] = True

    def on(self, probe: int) -> None:
        self._update_led_state(probe, INDICATOR_ON_TAG)

    def off(self, probe: int) -> None:
        self._update_led_state(probe, INDICATOR_OFF_TAG)

