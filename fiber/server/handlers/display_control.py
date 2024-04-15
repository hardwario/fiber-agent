from loguru import logger
from enum import Enum
from fiber.display.src.display import Display
from fiber.hal.led_controller import LedController
from fiber.server.manager import ServerManager
from fiber.hal.southbridge import south_bridge


class IndicatorState(Enum):
    RED = "red"
    GREEN = "green"
    ON = "on"
    OFF = "off"


class NotFoundError(Exception):
    pass


class DisplayControlHandler:
    def __init__(self, led_controller: LedController, spi_display: Display, server: ServerManager, uuid: str, request: str, body:  dict[str, str | int]) -> None:
        self._led_controller = led_controller
        self._spi_display = spi_display
        self._server = server
        self._uuid = uuid
        self._request = request
        self._body = body

    def _set_power_indicator(self) -> None:
        led_output = self._body["output"]
        led_state = self._body["state"]

        if led_state:
            self._led_controller.on(led_output)
        elif not led_state:
            self._led_controller.off(led_output)
        else:
            raise ValueError(f"Invalid temperature value: {led_state}")

    def _set_probe_indicator(self) -> None:
        led_output = self._body["output"]
        temperature = self._body["temperature"]

        try:
            if temperature is None:
                self._led_controller.red(led_output)
            elif isinstance(temperature, (int, float)):
                # self._spi_display.set_value(led_output, temperature)
                self._led_controller.green(led_output)
            else:
                raise ValueError(f"Invalid temperature value: {temperature}")
        except NotFoundError:
            self._server.send_err(self._request, self._uuid)
            logger.error("Problem setting LED")
            south_bridge.reset_leds()
        finally:
            logger.debug(f"Indicator: {led_output}, Temperature: {temperature}")
            response = south_bridge.flush()
            if response is not None:
                # self._spi_display.set_voltage(response.voltage_eth, response.voltage_bat)
                ...
                
