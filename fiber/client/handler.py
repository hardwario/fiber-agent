import threading
from loguru import logger

from fiber.client.validator import ClientDataValidator
from fiber.client.manager import ClientManager
from fiber.common.queue_manager import QueueManager


class ClientHandler(ClientManager, ClientDataValidator):
    def __init__(self, server_response_queue: QueueManager, client_request_queue: QueueManager, message_for_server_queue: QueueManager, stop_event: threading.Event) -> None:
        super().__init__(server_response_queue, client_request_queue, message_for_server_queue, stop_event)

    def get_mac(self) -> str:
        mac =  self.get_response("get_mac")
        return mac

    def get_ip(self) -> str:
        ip = self.get_response("get_ip")
        return ip

    def get_fiber_id(self) -> int:
        fiber_id = self.get_response("get_fiber_id")
        return fiber_id

    def get_uptime(self) -> float:
        uptime = self.get_response("get_uptime")
        return uptime

    def set_power_indicator(self, probe: int, state: bool) -> None:
        self.validate_probe(probe)
        logger.debug(f"Probe: {probe}, State: {state}")

        body = {"output": probe, "state": state}
        self.send_request("set_power_indicator", body)

    def set_probe_indicator(self, probe: int, temperature: float | None) -> None:
        self.validate_probe(probe)
        logger.debug(f"Probe: {probe}, Temperature: {temperature}")

        body = {"output": probe, "temperature": temperature}
        self.send_request("set_probe_indicator", body)

    def reboot(self, delay: int = 0) -> None:
        body = {"delay": delay}
        self.send_request("reboot", body)

    def set_fiber_id(self, fiber_id: int) -> None:
        if not isinstance(fiber_id, int) or not (2159017983 >= fiber_id >= 2157969408):
            raise TypeError("Invalid fiber ID")

        body = {"id": fiber_id}
        self.send_request("set_id", body)
