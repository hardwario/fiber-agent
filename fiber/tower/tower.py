import json
import threading
import time
import serial
from fiber.common.queue_manager import QueueManager
from fiber.common.thread_manager import pool
from loguru import logger


class TowerManagerError(Exception):
    def __init__(self, message: str):
        super().__init__(message)


class TowerManager:
    def __init__(self, device: str, tower_data_queue: QueueManager) -> None:
        self.device = device
        self.tower_data_queue = tower_data_queue
        try:
            self.ser = serial.Serial(port=self.device, baudrate=115200, timeout=1)
            self.ser.reset_input_buffer()
            self.ser.reset_output_buffer()
        except serial.SerialException as e:
            raise TowerManagerError(f"Failed to initialize serial connection: {e}")
        self.stop_event = threading.Event()

    def start(self) -> None:
        tower_thread = threading.Thread(target=self.tower_main_loop)
        pool.manage_thread(True, tower_thread, self.stop_event)
        tower_thread.start()

    def tower_main_loop(self) -> None:
        logger.info("Tower: OK")

        while not self.stop_event.is_set():
            if not self.ser.is_open:
                try:
                    self.ser.open()
                    logger.info("Reconnecting to the tower dongle...")

                except serial.SerialException as e:
                    logger.debug(f"Failed to reconnect: {e}")
                    time.sleep(5)
                    continue

            try:
                logger.debug(f"Waiting for tower line...")
                line = self.ser.readline()

                if line:
                    self._process_line(line)
            except serial.SerialException as e:
                logger.error(f"Serial exception: {e}")
                self._handle_serial_exception()
                
    def _process_line(self, line: bytes) -> None:
        try:
            line = line.decode().strip()
            if not line or line.startswith('#'):
                return

            msg = json.loads(line)
            logger.info(f"Tower msg to queue: {msg}")
            self.tower_data_queue.send_qmsg(obj=msg)
        except (json.JSONDecodeError, AttributeError) as e:
            logger.error(f'Caught exception while processing line: {e}')

    def _handle_serial_exception(self) -> None:
        logger.info("Disconnecting from the tower dongle (device disconnected or multiple access on port?)...")
        try:
            self.ser.close()
        except Exception as e:
            raise TowerManagerError(f"Failed to close serial port properly: {e}")
        time.sleep(5)