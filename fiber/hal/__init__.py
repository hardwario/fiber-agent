import os
import threading
import time
from fiber.common.queue_manager import QueueManager
from fiber.common.thread_manager import pool
from loguru import logger
from fiber.hal.consts import GPIO_POWER_LED
from fiber.hal.devices.probe_manager import ProbeLEDs, ProbePower
from fiber.hal.devices.eeprom import EEPROM
from fiber.common.gpio_manager import gpio_manager
from fiber.server.handlers import ServerHandler, NotFoundError
from fiber.hal.i2c import I2C
from fiber.hal.led_controller import Controller, LedControllerError
from fiber.server.manager import (ServerManager, ServerError, ServerStopEventError)
from fiber.server.serial_number import SerialNumberReadError
from gpiod.line import Value

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
            self.initialize_i2c_devices()

            self.led_controller = Controller(self.probe_leds)
            self.server = ServerManager(self.server_response_queue, self.client_request_queue, self.message_for_server_queue)
            self.handler = ServerHandler(self.eeprom, self.server, self.led_controller, self.probe_leds, self.interface)
        except (ServerError, SystemError) as e:
            self.die(f"Problem attaching Server: {e}")

    def start(self) -> None:
        logger.debug("Starting system thread...")
        system_thread = threading.Thread(target=self.system_main_loop)
        pool.manage_thread(True, system_thread, self.stop_event)
        system_thread.start()

    def system_main_loop(self) -> None:
        logger.info("System: OK")
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

    def initialize_i2c_devices(self):
        try:
            self.local_i2c = I2C(10)
            self.eeprom = EEPROM(self.local_i2c, device_addr=0x56) 
            self.probe_power = ProbePower(self.local_i2c)
            self.probe_leds = ProbeLEDs(self.local_i2c) 
        except (OSError, TypeError) as e:
            self.die(f"i2C initialization failed: {e}") 
        except SerialNumberReadError as e:
            self.critical_state(f"Problem initializing system: {e}")
            self.die(f"Problem attaching EEPROM and SerialNumber: {e}") 
    
    def die(self, error_msg: str) -> None:
        logger.error(error_msg)
        gpio_manager.release()
        raise SystemError(error_msg)

    def critical_state(self, error_msg: str) -> None:
        try:
            logger.critical(error_msg)
            while True:
                gpio_manager.set_value(GPIO_POWER_LED, Value.ACTIVE)
                time.sleep(0.05)
                gpio_manager.set_value(GPIO_POWER_LED, Value.INACTIVE)
                time.sleep(0.05)
        except KeyboardInterrupt:
            logger.info("KeyboardInterrupt received, stopping...")
            gpio_manager.set_value(GPIO_POWER_LED, Value.INACTIVE)

