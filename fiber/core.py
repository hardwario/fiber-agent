import os
import sys
import threading
import click
import signal
import traceback
from loguru import logger
from fiber.hal import FiberHAL, FiberHALError
from fiber.hal.southbridge import south_bridge
from fiber.common.thread_manager import pool
from fiber.mqtt.mqtt_bridge import MQTTBridge
from fiber.common.queue_manager import QueueManager
from fiber.sensor.sensor import Sensor
from fiber.system.system import System, SystemError
from fiber.broker.sensor import SensorBroker, SensorBrokerError
from fiber.common.config_manager import ConfigManager, FiberConfig

def perfom_graceful_shutdown(signum, frame) -> None:
    south_bridge.reset_leds()
    pool.exit_all_thread()
    logger.success("Successful exit")

def start_hal_manager(system_config_data: dict[str, bool | str], queues: dict[str, QueueManager]) -> bool:
    try:
        interface = system_config_data.interface
        FiberHAL(interface, *[queues[name] for name in ["server_response", "client_request", "message_for_server"]]).start()
        return pool._check_thread_presence('hal_main_loop')
    except FiberHALError as exc:
        logger.error(f"Problem while connecting to hal: {exc}")
        raise exc

def start_system_manager(pd_config: ConfigManager, queues: dict[str, QueueManager]) -> tuple[bool, System]:
    try:
        interface_manager = System(pd_config, *[queues[name] for name in ["server_response", "client_request", "message_for_server"]])
        interface_manager.start()
        return pool._check_thread_presence('system_main_loop', 10), interface_manager.mqtt_bridge_obj
    except SystemError as exc:
        logger.error(f"Problem while connecting to system: {exc}")
        raise exc

def start_sensor_broker(pd_config_data: FiberConfig, mqtt_instance: MQTTBridge | None, queues: dict[str, QueueManager]) -> SensorBroker:
    try:
        SensorBroker(mqtt_instance, pd_config_data, queues["sensor"]).start()
        return pool._check_thread_presence('sensor_broker_loop', 15)
    except SensorBrokerError as exc:
        logger.error(f"Problem while connecting to SENSOR BROKER: {exc}")
        raise exc

def start_sensor_manager(pd_config_data: FiberConfig, queues: dict[str, QueueManager]) -> None:
    bus_directory = "/sys/bus/w1/devices/w1_bus_master"
    sensor_lock = threading.RLock()

    if pd_config_data.sensor.enabled:
        for channel in range(8):
            sensor_manager = Sensor(channel + 1, f"{bus_directory}{channel + 1}", False, sensor_lock, *[queues[n] for n in ["sensor", "server_response", "client_request", "message_for_server"]])
            sensor_manager.start()
    else:
        logger.info(f"Sensor module is disabled.")

@logger.catch
@click.command()
@click.option('-c', '--config-path', help='Configuration file path', type=click.Path(exists=True), required=True)
def run(config_path: str) -> None:
    signal.signal(signal.SIGTERM, perfom_graceful_shutdown)
    if os.getuid() != 0:
        raise SystemError("You must run the process as root")
    try:
        pd_config = ConfigManager(config_path)
        pd_config_data = pd_config.config_data
        queue_names = ["server_response", "client_request", "message_for_server", "sensor"]
        queues = {name: QueueManager() for name in queue_names}

        if start_hal_manager(pd_config_data.system, queues):
            start_client, mqtt_instance = start_system_manager(pd_config, queues)

            if start_client:
                if start_sensor_broker(pd_config_data, mqtt_instance, queues):
                    start_sensor_manager(pd_config_data, queues)

    except Exception as e:
        traceback_str = ''.join(traceback.format_exception(type(e), e, e.__traceback__))
        logger.error(f"{e.__class__.__name__}: Critical system error - {traceback_str}")
        perfom_graceful_shutdown(None, None)


if __name__ == '__main__':
    sys.exit(run())

