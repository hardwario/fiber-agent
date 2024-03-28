__version__ = "1.0.0"
import os
import re
import threading
import time
from fiber.common.consts import INDICATOR_GREEN, INDICATOR_RED
from fiber.common.queue_manager import QueueManager
from fiber.client.handler import ClientHandler
from fiber.common.thread_manager import pool
from loguru import logger


class SensorError(Exception):
    def __init__(self, message: str):
        super().__init__(message)

class Sensor:
    def __init__(self, channel: int, bus_directory: str, bulk_read: bool, sensor_temperature_queue: QueueManager, server_response_queue: QueueManager, client_request_queue: QueueManager, message_for_server_queue: QueueManager) -> None:
        self.channel = channel
        self.bus_directory = bus_directory
        self.bulk_read = bulk_read
        self.sensor_temperature_queue = sensor_temperature_queue
        self.server_response_queue = server_response_queue
        self.client_request_queue = client_request_queue
        self.message_for_server_queue = message_for_server_queue
        self.stop_event = threading.Event()
        self.knownen = {}
        try:
            self.therm_bulk_read = os.path.join(bus_directory, "therm_bulk_read")
        except (OSError, TypeError) as e:
            raise SensorError(e)
        self.client = ClientHandler(server_response_queue, client_request_queue, message_for_server_queue, self.stop_event)

    def start(self) -> None:
        sensor_thread = threading.Thread(target=self.sensor_main_loop)
        pool.manage_thread(True, sensor_thread, self.stop_event)
        sensor_thread.start()

    def sensor_main_loop(self) -> None:
        logger.info(f"Sensor {self.channel}: OK")

        while not self.stop_event.is_set():
            try:
                if self.bulk_read:
                    self.trigger_bulk_read()

                try:
                    thermometers = os.listdir(self.bus_directory)
                except OSError:
                    time.sleep(1)
                    continue

                for thermometer in thermometers:
                    if not self.process_thermometer(thermometer):
                        break

            except OSError as e:
                raise SensorError(f"Thermometer scan failed: {e}")

            self.update_indicator()
            time.sleep(1)

    def process_thermometer(self, thermometer: str) -> None:
        thermometer_path = os.path.join(self.bus_directory, thermometer)
        if not os.path.isdir(thermometer_path) or re.match(r"^28-[0-9a-f]{12}$", thermometer) is None:
            return True

        ts = int(time.time())
        self.knownen[thermometer] = ts

        temperature_path = os.path.join(thermometer_path, "temperature")
        try:
            with open(temperature_path, "r") as f:
                temperature = round(int(f.readline().strip()) / 1000, 2)

            data = {
                "timestamp": ts,
                "channel": self.channel,
                "thermometer": thermometer,
                "temperature": temperature,
            }
            logger.info(f'CHANNEL {self.channel} to queue - Temperature: ' f"{temperature} C")


            self.sensor_temperature_queue.send_qmsg(data)
        except (OSError, ValueError):
            self.knownen.pop(thermometer)
            return False
        return True

    def update_indicator(self) -> None:
        if len(self.knownen) == 0:
            self.client.set_indicator(self.channel, INDICATOR_RED)
        else:
            self.client.set_indicator(self.channel, INDICATOR_GREEN)

    def trigger_bulk_read(self) -> None:
        try:
            if os.path.exists(self.therm_bulk_read):
                with threading.Lock():
                    with open(self.therm_bulk_read, "w") as f:
                        f.write("trigger\n")
        except OSError as e:
            raise SensorError(f"Error triggering bulk read: {e}")
