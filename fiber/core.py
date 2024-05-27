import os
import signal
import subprocess
import sys
import threading
import traceback

import click
import netifaces
from loguru import logger

from fiber.broker.sensor import SensorBroker
from fiber.client.handler import InterfaceHandler
from fiber.common.config_manager import load_config_from_file
from fiber.common.consts import PATH_W1_DEVICES, POWER_LED
from fiber.common.queue_manager import QueueManager
from fiber.models.configurations import FiberConfig
from fiber.sensor.sensor import Sensor
from fiber.server.manager import SystemManager


def get_connection_name(interface):
    result = subprocess.run(['nmcli', '-g', 'GENERAL.CONNECTION',
                            'device', 'show', interface], stdout=subprocess.PIPE)
    return result.stdout.decode().strip()


def set_network_properties(static_ip: bool, interface: str, address: str, netmask: str, gateway: str, dns: str):
    connection_name = get_connection_name(interface)

    if static_ip:
        subprocess.call(['nmcli', 'con', 'mod', connection_name, 'ipv4.addresses',
                        f'{address}/{netmask}', 'ipv4.gateway', gateway, 'ipv4.dns', dns, 'ipv4.method', 'manual'])
    else:
        subprocess.call(
            ['nmcli', 'con', 'mod', connection_name, 'ipv4.method', 'auto'])

    subprocess.call(['nmcli', 'con', 'down', connection_name])
    subprocess.call(['nmcli', 'con', 'up', connection_name])


class CoreManager:
    def __init__(self, config_path: str) -> None:
        self.config_path = config_path
        self.fiber_config = load_config_from_file(config_path, FiberConfig)
        self.netw_interface = None
        self.core_stop_event = threading.Event()
        self._system_manager: SystemManager | None = None
        self._sensor_broker: SensorBroker | None = None
        self._sensor_threads: list[Sensor] = []
        self._queues: dict[str, QueueManager] = {name: QueueManager()
                                                 for name in ['system_response', 'interface_request', 'sensor']}

    def _configure_network(self, interfaces: str) -> str:
        available_interfaces = netifaces.interfaces()
        self.netw_interface = next(
            (inter for inter in interfaces if inter in available_interfaces), None)
        if not self.netw_interface:
            raise RuntimeError('No valid interface found.')

        set_network_properties(
            self.fiber_config.system.static_ip,
            self.netw_interface,
            self.fiber_config.system.address,
            self.fiber_config.system.netmask,
            self.fiber_config.system.gateway,
            self.fiber_config.system.dns
        )

    def activate(self) -> None:
        interfaces = self.fiber_config.system.interface.split(',')
        self._configure_network(interfaces)

        logger.info(self.netw_interface)

        self._system_manager = SystemManager(self.netw_interface, self.core_stop_event, *[
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
        for channel in range(8):
            sensor_manager = Sensor(channel + 1, f'{PATH_W1_DEVICES}{channel + 1}',
                                    False, interface_handler, self._queues['sensor'], single_sensor_lock)
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
                  frame: system_manager.graceful_shutdown(signum, frame))
    if os.getuid() != 0:
        raise SystemError('You must run the process as root')

    try:
        system_manager = CoreManager(config_path)
        system_manager.activate()
    except Exception as e:
        logger.error(
            f'{e.__class__.__name__}: Critical system error - {traceback.format_exc()}')
        system_manager.graceful_shutdown(None, None)


if __name__ == '__main__':
    sys.exit(run())
