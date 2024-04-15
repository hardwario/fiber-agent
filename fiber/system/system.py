import threading
import time
import schedule
from fiber.common.consts import POWER_LED
from fiber.common.queue_manager import QueueManager
from fiber.client.handler import ClientHandler
from fiber.common.thread_manager import pool
from loguru import logger
from fiber.mqtt.mqtt_bridge import MQTTBridge, MQTTError
from fiber.common.config_manager import ConfigManager


class SystemError(Exception):
    def __init__(self, message: str):
        super().__init__(message)


class System:
    def __init__(self, pd_config: ConfigManager, server_response_queue: QueueManager, client_request_queue: QueueManager, message_for_server_queue: QueueManager) -> None:
        self.server_response_queue = server_response_queue
        self.client_request_queue = client_request_queue
        self.message_for_server_queue = message_for_server_queue
        self.stop_event = threading.Event()
        try:
            self.client_handler = self.initialize_client_handler()

            if pd_config.config_data.mqtt.enabled:
                self.mqtt_bridge_obj = self.initialize_mqtt_bridge(pd_config.config_path)
            else:
                self.mqtt_bridge_obj = None
        except SystemError as e:
            raise

    def start(self) -> None:
        system_thread = threading.Thread(target=self.system_main_loop)
        pool.manage_thread(True, system_thread, self.stop_event)
        system_thread.start()

    def system_main_loop(self) -> None:
        self.client_handler.set_power_indicator(probe=POWER_LED, state=True)
        if self.mqtt_bridge_obj:
            schedule.every(1).minute.do(self.mqtt_bridge_obj.send_beacon).run()

        logger.info("System: OK")
        while not self.stop_event.is_set():
            try:
                schedule.run_pending()
                time.sleep(0.1)
            except MQTTError as e:
                raise SystemError(e)

    def initialize_client_handler(self) -> ClientHandler:
        client_handler = ClientHandler(self.server_response_queue, self.client_request_queue, self.message_for_server_queue, self.stop_event)
        return client_handler

    def initialize_mqtt_bridge(self, config_path: str) -> MQTTBridge:
        mqtt_bridge = MQTTBridge(self.client_handler, config_path)
        return mqtt_bridge
