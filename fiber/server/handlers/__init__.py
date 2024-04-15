import os
import time
# from fiber.hal.spidisplay import SPIDisplay

from fiber.display.src.display import Display
from fiber.server.handlers.display_control import DisplayControlHandler
from fiber.server.handlers.network_interface import NetworkInterfaceHandler
from fiber.hal.led_controller import LedController
from fiber.server.manager import ServerManager
from loguru import logger


class NotFoundError(Exception):
    pass


class ServerHandler(DisplayControlHandler, NetworkInterfaceHandler):
    def __init__(self, server: ServerManager, led_controller: LedController, spi_display: Display, interface: str) -> None:
        self._uuid = None
        self._request = None
        self._body = None
        self._interface = interface

        DisplayControlHandler.__init__(self, led_controller, spi_display,
                                       server, self._uuid, self._request, self._body)
        NetworkInterfaceHandler.__init__(self, self._interface,
                                         server, self._uuid, self._request, self._body)

        self._message_callbacks = {
            "set_power_indicator": super()._set_power_indicator,
            "set_probe_indicator": super()._set_probe_indicator,
            "get_mac": super()._get_mac,
            "get_ip": super()._get_ip,
            "get_uptime": super()._get_uptime,
            "get_fiber_id": super()._get_fiber_id,
            "set_id": super()._set_id,
        }

    def run(self, uuid: str, request: str, body: dict[str, str | int]) -> None:
        if request not in self._message_callbacks:
            logger.error(f"Server Handler: Request not found in message callbacks: {request}")
            raise NotFoundError

        self._uuid, self._request, self._body = uuid, request, body
        self._message_callbacks[self._request]()
        self._uuid, self._request, self._body = None, None, None

    def _reboot(self) -> None:
        if self._body is not None:
            time.sleep(self._body["delay"])
        os.system("reboot")
