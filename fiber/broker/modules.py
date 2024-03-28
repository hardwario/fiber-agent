from abc import ABC, abstractmethod
import threading
from fiber.broker.consts import TowerOutput, CooperOutput
from pydantic import ValidationError
import json
from fiber.common.queue_manager import QueueManager
from fiber.common.thread_manager import pool
from loguru import logger

from fiber.mqtt.mqtt_bridge import MQTTBridge


class ModuleBrokerError(Exception):
    pass


class AlreadyRunningThread(ModuleBrokerError):
    pass


class MessageProcessor(ABC):
    @staticmethod
    def create_processor(name: str) -> 'MessageProcessor':
        processor_mapping = {
            'Tower': TowerMessageProcessor,
            'Cooper': CooperMessageProcessor
        }
        processor_class = processor_mapping.get(name)
        if processor_class:
            return processor_class()
        raise ModuleBrokerError(f"Unsupported module type: {name}")

    @abstractmethod
    def process_message(self, msg: dict | list, mqtt_bridge: MQTTBridge) -> None:
        pass

class TowerMessageProcessor(MessageProcessor):
    def process_message(self, msg: list[float | dict], mqtt_bridge: MQTTBridge) -> None:
        try:
            TowerOutput(**msg)
            mqtt_bridge.send_tower_data(data=msg)
        except ValidationError as e:
            logger.error(f"Not valid TOWER data: {msg}")

class CooperMessageProcessor(MessageProcessor):
    def process_message(self, msg: dict[str, int | str | float], mqtt_bridge: MQTTBridge) -> None:
        try:
            CooperOutput(**msg)
            mqtt_bridge.send_cooper_data(data=msg)
        except ValidationError as e:
            logger.error(f"Not valid COOPER data: {e}")

class ModuleBroker:
    def __init__(self, name: str, mqtt: MQTTBridge | None, active_module_queue: QueueManager, mqtt_enabled: bool=True) -> None:
        self._mqtt = mqtt
        self._name = name
        self.active_module_queue = active_module_queue

        self._processor = MessageProcessor.create_processor(self._name)
        self._stop_event = threading.Event()
        self._thread = None

    def start(self) -> None:
        if self._thread is None:
            self._thread = threading.Thread(target=self.module_broker_loop)
            pool.manage_thread(True, self._thread, self._stop_event)
            self._thread.start()
        else:
            raise AlreadyRunningThread("Thread is already running")

    def module_broker_loop(self) -> None:
        logger.info(f'Broker {self._name}: OK')

        while not self._stop_event.is_set():
            try:
                msg = self.active_module_queue.recv_qmsg(self._stop_event, timeout=0.1)
                if self._mqtt and msg is not None:
                    self._processor.process_message(msg, self._mqtt)
            except json.JSONDecodeError as exc: 
                logger.error(f"Error in {self._name}: {exc}")