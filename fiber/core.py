import os
import signal
import subprocess
import sys
import threading
import time
import traceback

import click
import netifaces
from loguru import logger

from fiber.broker.sensor import SensorBroker
from fiber.client.handler import InterfaceHandler
from fiber.common.config_manager import load_config_from_file
from fiber.common.consts import POWER_LED
from fiber.common.queue_manager import QueueManager
from fiber.models.configurations import FiberConfig, SystemConfig
from fiber.sensor.sensor import Sensor
from fiber.server.manager import SystemManager


class CoreManager:
    def __init__(self, config_path: str) -> None:
        self.config_path = config_path
        self.fiber_config = load_config_from_file(config_path, FiberConfig)
        self.core_stop_event = threading.Event()
        self._system_manager: SystemManager | None = None
        self._sensor_broker: SensorBroker | None = None
        self._sensor_threads: list[Sensor] = []
        self._queues: dict[str, QueueManager] = {name: QueueManager()
                                                 for name in ['system_response', 'interface_request', 'sensor']}

    def _get_connection_name(self, timeout: int=15, interval: int=1) -> str:
        start_time = time.time()
        while time.time() - start_time < timeout:
            result = subprocess.run(['nmcli', '-g', 'GENERAL.CONNECTION', 'device', 'show', self.valid_interface], stdout=subprocess.PIPE, check=True)
            connection_name = result.stdout.strip()
            if connection_name:
                logger.info(f'Connection found for interface {self.valid_interface}: {connection_name}')
                return connection_name
            else:
                logger.info(f'No connection found for interface {self.valid_interface}')
            time.sleep(interval)
        raise RuntimeError(f'Failed to find connection for interface {self.valid_interface} after {timeout} seconds')

    def _set_network_properties(self, system_config: SystemConfig) -> None:
        connection_name = self._get_connection_name()

        if system_config.static_ip:
            subprocess.call(['nmcli', 'con', 'mod', connection_name,
                            'ipv4.addresses', f'{system_config.address}/{system_config.netmask}',
                            'ipv4.gateway', system_config.gateway, 'ipv4.dns',system_config.dns,
                            'ipv4.method', 'manual'])
        else:
            subprocess.call(['nmcli', 'con', 'mod', connection_name, 'ipv4.method', 'auto'])

        subprocess.call(['nmcli', 'con', 'down', connection_name])
        subprocess.call(['nmcli', 'con', 'up', connection_name])

    def _configure_network(self, interfaces: str) -> str:
        available_interfaces = netifaces.interfaces()
        self.valid_interface = next((inter for inter in interfaces if inter in available_interfaces), None)
        if not self.valid_interface:
            raise RuntimeError('No valid interface found.')
        
        system_config = self.fiber_config.system
        self._set_network_properties(system_config)

    def activate(self) -> None:
        interfaces = self.fiber_config.system.interface.split(',')
        self._configure_network(interfaces)

        self._system_manager = SystemManager(self.valid_interface, self.core_stop_event, *[
            self._queues[name] for name in ['system_response', 'interface_request']])
        self._system_manager.start()

        interface_handler = InterfaceHandler(
            self.core_stop_event, *[self._queues[name] for name in ['system_response', 'interface_request']])
        interface_handler.set_indicator_state(probe=POWER_LED, state=True)

        if not self.fiber_config.sensor.enabled:
            logger.info('Sensor disabled. Skipping sensor setup')
            return

        self._sensor_broker = SensorBroker(self.config_path, self.fiber_config,
                                           interface_handler, self._queues['sensor'])
        self._sensor_broker.start()

        single_sensor_lock = threading.RLock()
        for channel in range(1, 9):
            sensor_manager = Sensor(channel, interface_handler, self._queues['sensor'], single_sensor_lock, self.core_stop_event)
            sensor_manager.start()
            self._sensor_threads.append(sensor_manager)

    def graceful_shutdown(self, signum, frame) -> None:
        logger.success('Performing graceful shutdown')
        if self._system_manager is not None:
            self._system_manager.quit()
        if self._sensor_broker is not None:
            self._sensor_broker.quit()
        for sensor_thread in self._sensor_threads:
            sensor_thread.quit()

        logger.success('Successful exit')

@logger.catch
@click.command()
@click.option('-c', '--config-path', help='Configuration file path', type=click.Path(exists=True), required=True)
def run(config_path: str) -> None:
    signal.signal(signal.SIGTERM, lambda signum,
                  frame: core_manager.graceful_shutdown(signum, frame))
    if os.getuid() != 0:
        raise SystemError('You must run the process as root')

    try:
        core_manager = CoreManager(config_path)
        core_manager.activate()
    except Exception as e:
        logger.error(f'{e.__class__.__name__}: Critical system error - {traceback.format_exc()}')
        if 'core_manager' in locals() and core_manager:
            core_manager.graceful_shutdown(None, None)
        sys.exit(1)


if __name__ == '__main__':
    sys.exit(run())
