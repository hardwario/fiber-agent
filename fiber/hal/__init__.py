import threading
from fiber.common.queue_manager import QueueManager
from fiber.common.thread_manager import pool
from fiber.hal.spidisplay import SPIDisplay
from loguru import logger
from fiber.server.handlers import ServerHandler, NotFoundError
from fiber.hal.led_controller import LedController
from fiber.server.manager import (ServerManager, ServerError, ServerStopEventError)

 
class FiberHALError(Exception):
    def __init__(self, message: str):
        super().__init__(message)


class FiberHAL:
    def __init__(self, interface: str, server_response_queue: QueueManager, client_request_queue: QueueManager, message_for_server_queue: QueueManager) -> None:
        self.server_response_queue = server_response_queue
        self.client_request_queue = client_request_queue
        self.message_for_server_queue = message_for_server_queue
        self.interface = interface

        self.stop_event = threading.Event()
        try:
            self.led_controller = LedController()
            self.spi_display = SPIDisplay()
            self.server = ServerManager(self.server_response_queue, self.client_request_queue, self.message_for_server_queue)
            self.handler = ServerHandler(self.server, self.led_controller, self.interface)
            self.led_controller.activate_leds()
            self.spi_display.run_in_thread()
        except (ServerError, SystemError) as e:
            self.die(f"Problem attaching Server: {e}")

    def start(self) -> None:
        logger.debug("Starting hal thread...")
        system_thread = threading.Thread(target=self.hal_main_loop)
        pool.manage_thread(True, system_thread, self.stop_event)
        system_thread.start()

    def hal_main_loop(self) -> None:
        logger.info("HAL: OK")
        while not self.stop_event.is_set():
            try:
                uuid, request, body = self.server.recv(self.stop_event)
                self.handler.run(uuid, request, body)
            except ServerStopEventError as e:
                break
            except (KeyError, TypeError) as error:
                self.server.send_err(None, None)
                self.die(f"Request Error: {error}")
            except NotFoundError:
                self.server.send_err(request, uuid)
                self.die(f"Request: Command not found for UUID: {uuid}, Request: {request}")  
    
    def die(self, error_msg: str) -> None:
        logger.error(error_msg)
        raise SystemError(error_msg)
