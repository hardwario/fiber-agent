import os
import sys
import threading
import click
import signal
import traceback
import netifaces
from fiber.common.consts import POWER_LED, PATH_W1_DEVICES
from loguru import logger
from fiber.client.handler import ClientHandler
from fiber.server.manager import ServerManager
from fiber.common.queue_manager import QueueManager
from fiber.sensor.sensor import Sensor
from fiber.broker.sensor import SensorBroker
from fiber.common.config_manager import ConfigManager
from fiber.models.configurations import FiberConfig


class SystemManager:
    def __init__(self, fiber_config: FiberConfig) -> None:
        self.fiber_config = fiber_config
        self.core_stop_event = threading.Event()
        self._server_manager: ServerManager = None
        self._sensor_broker: SensorBroker = None
        self._sensor_threads: list[Sensor] = []
        self._queues: dict[str, QueueManager] = {name: QueueManager() for name in ["server_response", "client_request", "sensor"]}
    
    def _find_valid_interface(self, interfaces: str) -> str:
        available_interfaces = netifaces.interfaces()
        interface = next((inter for inter in interfaces.split(",") if inter in available_interfaces), None)
        if not interface:
            raise RuntimeError("No valid interface found.")  
        return interface
    
    def start(self) -> None:
        interface = self._find_valid_interface(self.fiber_config.system.interface)
        self._server_manager = ServerManager(interface, self.core_stop_event, *[self._queues[name] for name in ["server_response", "client_request"]])
        self._server_manager.start()

        client_handler = ClientHandler(self.core_stop_event, *[self._queues[name] for name in ["server_response", "client_request"]])
        client_handler.set_indicator_state(probe=POWER_LED, state=True)

        sensor_lock = threading.RLock()

        self._sensor_broker = SensorBroker(client_handler, self.fiber_config, self._queues["sensor"])
        self._sensor_broker.start()

        for channel in range(8):
            sensor_manager = Sensor(channel + 1, f"{PATH_W1_DEVICES}{channel + 1}", False, client_handler, self._queues["sensor"], sensor_lock)
            sensor_manager.start()
            self._sensor_threads.append(sensor_manager)

    def graceful_shutdown(self, signum, frame) -> None:
        logger.success("Performing graceful shutdown")
        if self._server_manager is not None:
            self._server_manager.close()
        if self._sensor_broker is not None:
            self._sensor_broker.close()
        for sensor_thread in self._sensor_threads:
            sensor_thread.close()

        self.core_stop_event.set()
        logger.success("Successful exit")


@logger.catch
@click.command()
@click.option('-c', '--config-path', help='Configuration file path', type=click.Path(exists=True), required=True)
def run(config_path: str) -> None:
    signal.signal(signal.SIGTERM, lambda signum, frame: system_manager.graceful_shutdown(signum, frame))
    if os.getuid() != 0:
        raise SystemError("You must run the process as root")

    try:
        fiber_config = ConfigManager(config_path, FiberConfig).config_data
        system_manager = SystemManager(fiber_config)
        system_manager.start()
    except Exception as e:
        logger.error(f"{e.__class__.__name__}: Critical system error - {traceback.format_exc()}")
        system_manager.graceful_shutdown(None, None)

if __name__ == '__main__':
    sys.exit(run())