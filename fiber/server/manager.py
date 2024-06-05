import threading
from queue import Empty

from loguru import logger
from pydantic import ValidationError

from fiber.common.queue_manager import QueueManager
from fiber.models.request import Request
from fiber.models.response import Response
from fiber.server.display_handler import DisplayControlHandler
from fiber.server.network_handler import NetworkInterfaceHandler


class SystemStopEventError(Exception):
    pass


class NotFoundError(Exception):
    pass


class SystemManager(DisplayControlHandler, NetworkInterfaceHandler):
    def __init__(self, interface: str, core_stop_event: threading.Event, system_response_queue: QueueManager, interface_request_queue: QueueManager) -> None:
        self._system_thread = threading.Thread(target=self._loop)
        self._core_stop_event = core_stop_event

        self.response_queue = system_response_queue
        self.request_queue = interface_request_queue

        DisplayControlHandler.__init__(self)
        NetworkInterfaceHandler.__init__(self, interface)

        self.system_handlers: dict[str, callable] = {
            'set_indicator_state': self._set_indicator_state,
            'update_sensor_display': self._update_sensor_display,
            'get_mac': self._get_mac,
            'get_ip': self._get_ip,
            'get_uptime': self._get_uptime,
            'get_fiber_id': self._get_fiber_id,
            'get_voltage': self._get_voltage,
            'reboot': self._reboot,
        }

    def quit(self) -> None:
        self.south_bridge.reset_leds()
        self._core_stop_event.set()
        if self._system_thread is not None:
            self._system_thread.join()
            if self._system_thread.is_alive():
                logger.error(
                    f'Thread {self._system_thread.name} did not exit in time')
            else:
                logger.info(f'Thread {self._system_thread.name} exited')

    def start(self) -> None:
        logger.debug('Starting system manager thread...')
        self._system_thread.start()

    def _loop(self) -> None:
        logger.info('System: OK')
        while not self._core_stop_event.is_set():
            try:
                uuid, request, body = self.recv()
                self.run_handler(uuid, request, body)
            except SystemStopEventError as e:
                break
            except (KeyError, TypeError, ValueError) as error:
                self.send_err(None, None, str(error))
                self.die(f'Request Error: {error}')
            except NotFoundError:
                self.send_err(uuid, request, 'Command not found')
                self.die(
                    f'Request: Command not found for UUID: {uuid}, Request: {request}')

        self._spi_display.quit()
        self._button_controller.quit()

    def recv(self) -> tuple[str, str, dict | None]:
        while not self._core_stop_event.is_set():
            try:
                msg = self.request_queue.recv_qmsg(
                    self._core_stop_event, timeout=0.1, empty_error=True)
                if not msg:
                    raise SystemStopEventError

                try:
                    received_msg = Request(**msg)
                except ValidationError as e:
                    logger.error(f'Invalid request: {msg}')
                    self.send_err(None, None, str(e))
                    continue

                return received_msg.uuid, received_msg.request, received_msg.body
            except Empty:
                pass
        raise SystemStopEventError

    def run_handler(self, uuid: str, request: str, body: dict[str, str | int] | None) -> None:
        current_handler = self.system_handlers.get(request)
        if not current_handler:
            self.send_err(uuid, request, 'Command not found')
            return

        try:
            if body is None:
                response_content = current_handler()
                self.send_msg(uuid, request, response_content, False)
            else:
                current_handler(body)
        except NotFoundError as e:
            self.send_err(uuid, request, 'Command not found')

    def send_err(self, uuid: str | None, request: str | None, body_error: str) -> None:
        logger.error(
            f'System: Sending error: ("uuid": {uuid}, "response": {request}, "error": True, "body": {body_error})')
        response = Response(uuid=uuid, response=request,
                            error=True, body=body_error)
        self.response_queue.send_qmsg(dict(response))

    def send_msg(self, uuid: str, request: str,  msg: int | str | float, error: bool) -> None:
        logger.debug(f'System: Sending message: {request}')
        response = Response(uuid=uuid, response=request, error=error, body=msg)
        self.response_queue.send_qmsg(dict(response))

    def die(self, error_msg: str) -> None:
        logger.error(error_msg)
        raise SystemError(error_msg)
