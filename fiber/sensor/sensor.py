__version__ = '1.0.0'
import os
import re
import threading
import time

from loguru import logger

from fiber.common.consts import PATH_W1_DEVICES
from fiber.client.handler import InterfaceHandler
from fiber.common.queue_manager import QueueManager
from fiber.models.sensor import SensorOutput


class SensorError(Exception):
    def __init__(self, message: str):
        super().__init__(message)


class Sensor:
    def __init__(self, channel: int, interface_handler: InterfaceHandler, sensor_broker_queue: QueueManager, sensor_lock: threading.RLock, core_stop_event: threading.Event) -> None:
        self.sensor_thread = threading.Thread(target=self._loop)
        self._stop_event = core_stop_event

        self.channel = channel
        
        self.bus_directory = f'{PATH_W1_DEVICES}{self.channel}'
        self.sensor_broker_queue = sensor_broker_queue
        self.sensor_lock = sensor_lock
        self.known: dict[str, float | None] = {}
        try:
            self.therm_bulk_read = os.path.join(
                self.bus_directory, 'therm_bulk_read')
        except (OSError, TypeError) as e:
            raise SensorError(e)
        self.interface = interface_handler

    def quit(self) -> None:
        self._stop_event.set()

        if self.sensor_thread is not None:
            self.sensor_thread.join()
            if self.sensor_thread.is_alive():
                logger.error('Thread did not exit in time')
            else:
                logger.info(f'Thread {self.sensor_thread.name} exited')

    def start(self) -> None:
        logger.debug('Starting sensor thread...')
        self.sensor_thread.start()

    def _loop(self) -> None:
        logger.info(f'Sensor {self.channel}: OK')

        while not self._stop_event.is_set():
            try:
                self.trigger_bulk_read()

                try:
                    logger.debug(
                        f'Scanning for thermometers on channel {self.channel}')
                    thermometers = os.listdir(self.bus_directory)
                except OSError:
                    time.sleep(1)
                    continue

                for thermometer in thermometers:
                    if not self.process_thermometer(thermometer):
                        break

            except OSError as e:
                raise SensorError(f'Thermometer scan failed: {e}')

            self.update_sensor()
            time.sleep(1)

    def process_thermometer(self, thermometer: str) -> tuple[float | None, bool]:
        thermometer_path = os.path.join(self.bus_directory, thermometer)
        if not os.path.isdir(thermometer_path) or re.match(r'^28-[0-9a-f]{12}$', thermometer) is None:
            return True

        temperature_path = os.path.join(thermometer_path, 'temperature')

        try:
            with self.sensor_lock:
                with open(temperature_path, 'r') as f:
                    current_temperature = round(
                        int(f.readline().strip()) / 1000, 2)
                time.sleep(0.01)

            self.known[thermometer] = current_temperature

            sensor_data = SensorOutput(
                timestamp=int(time.time()),
                channel=self.channel,
                thermometer=thermometer,
                temperature=current_temperature
            )
            if current_temperature is not None:
                logger.info(
                    f'CHANNEL {self.channel} to queue - Temperature: ' f'{current_temperature} C')

            self.sensor_broker_queue.send_qmsg(sensor_data.model_dump())
        except (OSError, ValueError):
            self.known.pop(thermometer, None)
            return False
        return True

    def update_sensor(self) -> None:
        if len(self.known) == 0:
            self.interface.update_sensor_display(self.channel, None)
        else:
            temperature = next(iter(self.known.values()))
            self.interface.update_sensor_display(self.channel, temperature)

    def trigger_bulk_read(self) -> None:
        try:
            if os.path.exists(self.therm_bulk_read):
                with threading.Lock():
                    with open(self.therm_bulk_read, 'r+') as f:
                        f.write('trigger\n')
        except OSError as e:
            raise SensorError(f'Error triggering bulk read: {e}')
