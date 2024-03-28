from fiber.common.consts import PROBE_1, PROBE_8, POWER_LED, INDICATOR_RED
from loguru import logger

from fiber.hal.led_controller import Controller
from fiber.server.manager import ServerManager
from fiber.hal.devices.probe_manager import ProbeLEDs



class NotFoundError(Exception):
    pass


class LedControllHandller:
    def __init__(self, led_controller: Controller, led_driver: ProbeLEDs, server: ServerManager, uuid: str, request: str, body:  dict[str, str | int]) -> None:
        self._led_controller = led_controller
        self._led_driver = led_driver
        self._server = server
        self._uuid = uuid
        self._request = request
        self._body = body
        self._led_not_ready = {probe for probe in range(PROBE_1, PROBE_8 + 1)}


    def _set_indicator(self) -> None:
        led_output = self._body["output"]
        led_state = self._body["state"]

        if led_output != POWER_LED:
            if len(self._led_not_ready) == 0:
                self._process_ready_led(led_output, led_state)
            else:
                self._process_not_ready_led(led_output)

        elif led_output == POWER_LED:
            self._process_power_led(led_state)

    def _process_ready_led(self, led_output: int, led_state: str) -> None:
        try:
            if led_state == "red":
                self._led_controller.red(led_output)
            elif led_state == "green":
                self._led_controller.green(led_output)
                
            elif led_state == "on":
                self._led_controller.on(led_output)
            elif led_state == "off":
                self._led_controller.off(led_output)
            else:
                raise NotFoundError

        except NotFoundError:
            self._server.send_err(self._request, self._uuid)
            logger.error("Problem setting LED")

    def _process_not_ready_led(self, led_output: int) -> None:
        try:
            self._led_not_ready.remove(led_output)
        except KeyError:
            pass

        if not self._led_not_ready:
            for probe in range(PROBE_1, PROBE_8 + 1):
                self._led_driver.set_probe_led(probe, INDICATOR_RED)

            self._led_driver.apply()

    def _process_power_led(self, led_state: str) -> None:
        try:
            if led_state == "on":
                self._led_controller.on(POWER_LED)
            elif led_state == "off":
                self._led_controller.off(POWER_LED)
            else:
                raise NotFoundError

        except NotFoundError:
            self._server.send_err(self._request, self._uuid)
            logger.error("Problem setting LED")
