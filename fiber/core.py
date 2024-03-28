import os
import sys
import click
import signal
import traceback
from loguru import logger
from fiber.tower.tower import TowerManager
from fiber.common.thread_manager import pool
from fiber.hal import FiberHAL, FiberHALError
from fiber.mqtt.mqtt_bridge import MQTTBridge
from fiber.cooper.cooper import CooperManager
from fiber.common.queue_manager import QueueManager
from fiber.sensor.sensor import Sensor, SensorError
from fiber.broker.modules import ModuleBroker, ModuleBrokerError
from fiber.system.system import System, SystemError
from fiber.broker.sensor import SensorBroker, SensorBrokerError
from fiber.common.config_manager import ConfigManager, FiberConfig
from fiber.common.gpio_manager import gpio_manager

def perfom_graceful_shutdown(signum, frame) -> None:
    gpio_manager.release()
    pool.exit_all_thread()
    logger.success("Successful exit")

def start_hal_manager(system_config_data: dict[str, bool | str], queues: dict[str, QueueManager]) -> bool:
    try:
        interface = system_config_data.interface
        FiberHAL(interface, *[queues[name] for name in ["server_response", "client_request", "message_for_server"]]).start()
        return pool._check_thread_presence('system_main_loop')
    except FiberHALError as e:
        raise FiberHALError(f"Problem while connection to system: {e}")

def start_system_manager(pd_config: ConfigManager, queues: dict[str, QueueManager]) -> tuple[bool, System]:
    try:
        interface_manager = System(pd_config, *[queues[name] for name in ["server_response", "client_request", "message_for_server"]])
        interface_manager.start()
        return pool._check_thread_presence('interface_main_loop', 10), interface_manager.mqtt_bridge_obj
    except SystemError as e:
        raise SystemError(f"Problem while connection to client: {e}")
        
def start_modules_broker(mqtt_instance: MQTTBridge | None, queues: dict[str, QueueManager]) -> bool:
    try:
        ModuleBroker("Tower", mqtt_instance, queues["tower"]).start()
        ModuleBroker("Cooper", mqtt_instance, queues["cooper"]).start()
        return pool._check_thread_presence('module_broker_loop', 10)
    except ModuleBrokerError as e:
        raise ModuleBrokerError(f"Problem while connecting to brocker: {e}")

def start_module_manager(module_type: str, module_config_data: dict[str, bool | str], manager: TowerManager | CooperManager, queues) -> None:
    dongle_path = module_config_data.dongle
    if module_config_data.enabled and os.path.exists(dongle_path):
        manager(dongle_path, queues[module_type]).start()
    else:
        logger.info(f"{module_type.capitalize()} module is not initialized.")

def start_sensor_broker(pd_config_data: FiberConfig, mqtt_instance: MQTTBridge | None, queues: dict[str, QueueManager]) -> SensorBroker:
    try:
        SensorBroker(mqtt_instance, pd_config_data, queues["sensor"]).start()
        return pool._check_thread_presence('sensor_broker_loop', 15)
    except SensorBrokerError as e:
        raise SensorError(f"Problem while connecting to SENSOR MQTT: {e}")

def start_sensor_manager(pd_config_data: FiberConfig, queues: dict[str, QueueManager]) -> None:
    bus_directory = "/sys/bus/w1/devices/w1_bus_master"
    if pd_config_data.sensor.enabled:
        for channel in range(8):
            sensor_manager = Sensor(channel + 1, f"{bus_directory}{channel + 1}", True, *[queues[n] for n in ["sensor", "server_response", "client_request", "message_for_server"]])
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
        queue_names = ["server_response", "client_request", "message_for_server", "tower", "cooper", "sensor"]
        queues = {name: QueueManager() for name in queue_names}

        
        if start_hal_manager(pd_config_data.system, queues):
            start_client, mqtt_instance = start_system_manager(pd_config, queues)

            if start_client:
                if start_modules_broker(mqtt_instance, queues):
                    start_module_manager("tower", pd_config_data.tower, TowerManager, queues)
                    start_module_manager("cooper", pd_config_data.cooper, CooperManager, queues)

                if start_sensor_broker(pd_config_data, mqtt_instance, queues):
                    start_sensor_manager(pd_config_data, queues)

    except Exception as e:
        traceback_str = ''.join(traceback.format_exception(type(e), e, e.__traceback__))
        logger.error(f"{e.__class__.__name__}: Critical system error - {traceback_str}")
        perfom_graceful_shutdown(None, None)


if __name__ == '__main__':
    sys.exit(run())

