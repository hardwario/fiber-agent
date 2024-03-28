import yaml
from pydantic import BaseModel
from loguru import logger


class SystemConfig(BaseModel):
    interface: str

class SensorConfig(BaseModel):
    enabled: bool
    report_interval_seconds: int
    sampling_interval_seconds: int

class MQTTConfig(BaseModel):
    enabled: bool
    host: str
    port: int

class ModuleConfig(BaseModel):
    enabled: bool
    dongle: str

class StorageConfig(BaseModel):
    enabled: bool
    name: str

class FiberConfig(BaseModel):
    version: int
    system: SystemConfig
    sensor: SensorConfig
    mqtt: MQTTConfig
    tower: ModuleConfig
    cooper: ModuleConfig
    storage: StorageConfig


class ConfigManagerError(Exception):
    pass

class ConfigManager:
    def __init__(self, config_path: str) -> None:
        self.config_path = config_path
        self.config_data = self.load_config()

    def load_config(self) -> FiberConfig:
        try:
            with open(self.config_path, 'r') as yaml_file:
                parsed_yaml = yaml.safe_load(yaml_file)
            return FiberConfig(**parsed_yaml)
        except (FileNotFoundError, yaml.YAMLError) as e:
            raise Exception(f"Configuration load error: {e}")

    def save_config(self) -> None:
        try:
            with open(self.config_path, 'w') as yaml_file:
                yaml.safe_dump(self.config_data.dict(), yaml_file, default_flow_style=False)
            logger.info("Configuration updated")
        except yaml.YAMLError as e:
            raise ConfigManagerError(f"Configuration save error: {e}")
        
    def set_interface(self, value: str) -> None:
        self.config_data.system.interface = value
        self.save_config()

    def set_report_interval_seconds(self, value: int) -> None:
        self.config_data.sensor.report_interval_seconds = value
        self.save_config()

    def set_sampling_interval_seconds(self, value: int) -> None:
        self.config_data.sensor.sampling_interval_seconds = value
        self.save_config()

    def set_mqtt_hostname(self, value: str) -> None:
        self.config_data.mqtt.host = value
        self.save_config()

    def set_mqtt_port(self, value: int) -> None:
        self.config_data.mqtt.port = value
        self.save_config()
