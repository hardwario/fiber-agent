import os
import time
from fiber.server.handlers.led_controll import LedControllHandller
from fiber.server.handlers.network_interface import NetworkInterfaceHandler
from fiber.hal.devices.probe_manager import ProbeLEDs
from fiber.hal.led_controller import Controller
from fiber.server.manager import ServerManager
from fiber.hal.devices.eeprom import EEPROM
from loguru import logger


class NotFoundError(Exception):
    pass


class ServerHandler(LedControllHandller, NetworkInterfaceHandler):
    def __init__(self, eeprom: EEPROM, server: ServerManager, led_controller: Controller, led_driver: ProbeLEDs, interface: str) -> None:
        self._uuid = None
        self._request = None
        self._body = None
        self._interface = interface
        logger.info(interface)

        LedControllHandller.__init__(self, led_controller, led_driver,
            server, self._uuid, self._request, self._body,
        )
        NetworkInterfaceHandler.__init__(self, self._interface, eeprom,
            server, self._uuid, self._request, self._body,
        )

        self._message_callbacks = {
            "set_indicator": super()._set_indicator,
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


