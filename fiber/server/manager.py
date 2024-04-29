import threading
from queue import Empty
from fiber.common.queue_manager import QueueManager
from loguru import logger
from pydantic import ValidationError
from fiber.server.display_handler import DisplayControlHandler
from fiber.server.network_handler import NetworkInterfaceHandler
from fiber.models.request_response import Request, Response


class ServerStopEventError(Exception):
    pass

class ServerError(Exception):
    pass

class NotFoundError(Exception):
    pass


class ServerManager(DisplayControlHandler, NetworkInterfaceHandler):
    def __init__(self, interface: str, core_stop_event: threading.Event, server_response_queue: QueueManager, client_request_queue: QueueManager) -> None:
        self._server_thread = threading.Thread(target=self._loop)
        self._core_stop_event = core_stop_event

        self.server_response_queue = server_response_queue
        self.client_request_queue = client_request_queue
        self._interface = interface

        DisplayControlHandler.__init__(self)
        NetworkInterfaceHandler.__init__(self, self._interface)
        
        self.server_handlers: dict[str, callable[..., any]] = {
            "set_indicator_state": self._set_indicator_state,
            "set_indicator_color": self._set_indicator_color,
            "get_mac": self._get_mac,
            "get_ip": self._get_ip,
            "get_uptime": self._get_uptime,
            "get_fiber_id": self._get_fiber_id,
            "reboot": self._reboot,
        }

    def close(self) -> None:
        self.south_bridge.reset_leds()
        self._core_stop_event.set()
        if self._server_thread is not None:
            self._server_thread.join()
            if self._server_thread.is_alive():
                logger.error(f"Thread {self._server_thread.name} did not exit in time")
            else:
                logger.info(f"Thread {self._server_thread.name} exited")

    def start(self) -> None:
        logger.debug("Starting server manager thread...")
        self._server_thread.start()

    def _loop(self) -> None:
        logger.info("Server: OK")
        while not self._core_stop_event.is_set():
            try:
                uuid, request, body = self.recv()
                self.run_handler(uuid, request, body)
            except ServerStopEventError as e:
                break
            except (KeyError, TypeError, ValueError) as error:
                self.send_err(None, None, str(error))
                self.die(f"Request Error: {error}")
            except NotFoundError:
                self.send_err(uuid, request, "Command not found")
                self.die(f"Request: Command not found for UUID: {uuid}, Request: {request}")

        self._spi_display.close()
    
    def recv(self) -> tuple[str, str, dict | None]:
        while not self._core_stop_event.is_set():
            try:
                msg = self.client_request_queue.recv_qmsg(self._core_stop_event, timeout=0.1, empty_error=True)
                if not msg:
                    raise ServerStopEventError
                
                try:
                    received_msg = Request(**msg)
                except ValidationError as e:
                    logger.error(f"Invalid request: {msg}")
                    self.send_err(None, None, str(e))
                    continue

                return received_msg.uuid, received_msg.request, received_msg.body
            except Empty:
                pass
        raise ServerStopEventError
    
    def run_handler(self, uuid: str, request: str, body: dict[str, str | int] | None) -> None:
        current_handler = self.server_handlers.get(request)
        if not current_handler:
            self.send_err(uuid, request, "Command not found")
            return

        try:
            if body is None:
                response_content = current_handler()
                self.send_msg(uuid, request, response_content, False)
            else:
                current_handler(body)
        except NotFoundError as e:
            self.send_err(uuid, request, "Command not found")

    def send_err(self, uuid: str, request: str, body_error: str) -> None:
        logger.error(f'Server: Sending error: ("uuid": {uuid}, "response": {request}, "error": True, "body": {body_error})')
        response = Response(uuid=uuid, response=request, error=True, body=body_error)
        self.server_response_queue.send_qmsg(dict(response))

    def send_msg(self, uuid: str, request: str,  msg: int | str | float, error: bool) -> None:
        logger.debug(f"Server: Sending message: {request}")
        response = Response(uuid=uuid, response=request, error=error, body=msg)
        self.server_response_queue.send_qmsg(dict(response))

    def die(self, error_msg: str) -> None:
        logger.error(error_msg)
        raise SystemError(error_msg)
