from enum import Enum

from loguru import logger
from pydantic import ValidationError

from fiber.common.consts import VALID_PROBES
from fiber.common.southbridge import SouthBridge
from fiber.display.spidisplay import SPIDisplay
from fiber.models.indicators import SensorDisplayBody, StateIndicatorBody
from fiber.server.led_controller import LedController


class NotFoundError(Exception):
    pass


class ProbeState(Enum):
    ACTIVE = 1
    INACTIVE = 0


class DisplayControlHandler:
    def __init__(self):
        self._display_probes: dict[int, ProbeState] = {probe: ProbeState.INACTIVE 
                                                       for probe in VALID_PROBES}
        self.south_bridge = SouthBridge()
        self._led_controller = LedController(self.south_bridge)
        self._spi_display = SPIDisplay()
        self._spi_display.start()

    def _set_indicator_state(self, body: dict[str, int | bool]) -> None:
        try:
            verified_body = StateIndicatorBody(**body)
        except ValidationError:
            logger.error(f'Invalid body: {body}')
            return

        led_output = verified_body.output
        led_state = verified_body.state

        if led_state:
            self._led_controller.on(led_output)
        elif not led_state:
            self._led_controller.off(led_output)
        else:
            raise ValueError(f'Invalid state: {led_state}')

    def _update_sensor_display(self, body: dict[str, None | float | int]) -> None:
        try:
            verified_body = SensorDisplayBody(**body)
        except ValidationError:
            logger.error(f'Invalid body: {body}')
            return

        led_output = verified_body.output
        temperature = verified_body.temperature

        if led_output not in self._display_probes:
            logger.error(f'Invalid probe: {led_output}')
            return

        if temperature is None:
            self._led_controller.red(led_output)
            if self._display_probes[led_output] == ProbeState.ACTIVE:
                self._spi_display.set_value(led_output, temperature)
        else:
            self._led_controller.green(led_output)
            self._spi_display.set_value(led_output, temperature)
            self._display_probes[led_output] = ProbeState.ACTIVE
        try:
            response = self.south_bridge.flush()
            if response is not None:
                self._spi_display.set_voltage(
                    response.voltage_eth, response.voltage_bat)
        except NotFoundError:
            logger.error('Problem setting LED')
            self.south_bridge.reset_leds()
            raise
