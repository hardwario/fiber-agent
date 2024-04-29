from pydantic import BaseModel

class SystemConfig(BaseModel):
    """
    Configuration of system interface.

    Attributes:
        interface: Interface name used by Fiber.
    """

    interface: str
    """Interface name used by Fiber."""

class SensorConfig(BaseModel):
    """
    Configuration of sensor interface.

    Attributes:
        enabled: Enable or disable sensor.
        report_interval_seconds: Interval to send sensor data to broker.
        sampling_interval_seconds: Interval to read sensor data.
    """

    enabled: bool
    """Enable or disable sensor."""
    report_interval_seconds: int
    """Interval to send sensor data to broker."""
    sampling_interval_seconds: int
    """Interval to read sensor data."""

class MQTTConfig(BaseModel):
    """
    Configuration of MQTT interface.

    Attributes:
        enabled: Enable or disable MQTT.
        host: MQTT broker hostname or IP address.
        port: MQTT broker port.
    """

    enabled: bool
    """Enable or disable MQTT."""
    host: str
    """MQTT broker hostname or IP address."""
    port: int
    """MQTT broker port."""

class StorageConfig(BaseModel):
    """
    Configuration of local storage for sensor data.

    Attributes:
        enabled: Enable or disable local storage.
        name: Name of SQLite database file used for storage.
    """
    enabled: bool
    """Enable or disable local storage."""
    name: str
    """Name of SQLite database file used for storage."""

class FiberConfig(BaseModel):
    """
    Configuration of Fiber.

    Attributes:
        version: Version of configuration file.
        system: System configuration.
        sensor: Sensor configuration.
        mqtt: MQTT configuration.
        storage: Storage configuration.
    """
    version: int
    system: SystemConfig
    sensor: SensorConfig
    mqtt: MQTTConfig
    storage: StorageConfig

