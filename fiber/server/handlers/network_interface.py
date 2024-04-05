import time
import netifaces
from loguru import logger
from fiber.server.manager import ServerManager

class NetworkInterfaceHandler:
    def __init__(self, interface: str, server: ServerManager, uuid: str, request: str, body:  dict[str, str | int]) -> None:
        self._interface = interface
        self._server = server
        self._uuid = uuid
        self._request = request
        self._body = body
        self.serial_number = 1

    def _send_message(self, key: str, value: str | int | float) -> None:
        try:
            self._server.send_msg(self._request, self._uuid, value, False)
            logger.debug(f"Hand: Sent {key}: {value}")
        except LookupError as e:
            self._server.send_err(self._request, self._uuid)
            logger.error(f"Hand: Problem sending {key}: {e}")
    
    def _set_id(self) -> None:
        id = self._body["id"]

        if not isinstance(id, int):
            self._server.send_err(self._request, self._uuid)
        else:
            self.serial_number = id

    def _get_mac(self) -> None:
        try:
            mac_address = self._wait_for_mac_network_interface()
        except (KeyError, IndexError) as e:
            logger.error(f"No MAC address found, restart the system")
            raise

        self._send_message("MAC address", mac_address)

    def _get_ip(self) -> None: 
        try:
            ip_address = self._wait_for_ip_network_interface()
        except (KeyError, IndexError) as e:
            logger.error(f"No IP address found, restart the system")
            raise

        self._send_message("IP address", ip_address)

    def _get_uptime(self) -> None:
        with open('/proc/uptime', 'r') as f:
            uptime_seconds = float(f.readline().split()[0])
            self._send_message("uptime", uptime_seconds)

    def _get_fiber_id(self) -> None:
        fiber_id = self.serial_number
        self._send_message("Fiber ID", fiber_id)

    def _wait_for_ip_network_interface(self) -> str:
        while True:
            try:
                addrs = netifaces.ifaddresses(self._interface)
                ip_address = addrs[netifaces.AF_INET][0]['addr']
                return ip_address
            except (KeyError, IndexError):
                logger.debug("Network interface not available yet. Retrying...")
                time.sleep(1)

    def _wait_for_mac_network_interface(self) -> str:
        while True:
            try:
                addrs = netifaces.ifaddresses(self._interface)
                mac_address = addrs[netifaces.AF_LINK][0]['addr']
                return mac_address
            except (KeyError, IndexError):
                logger.debug("Network interface not available yet. Retrying...")
                time.sleep(1)
