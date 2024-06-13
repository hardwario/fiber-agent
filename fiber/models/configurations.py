from ipaddress import IPv4Address, IPv4Network
from pydantic import BaseModel, field_validator, ValidationInfo

class SystemConfig(BaseModel):
    '''
    Configuration of system interface.

    Attributes:
        interface: Interface name used by Fiber.
        static_ip: Enable or disable static IP configuration.
        address: Address IP configuration.
        netmask: Netmask configuration.
        gateway: Gateway configuration.
        dns: DNS configuration.
    '''

    interface: str
    '''Interface name used by Fiber.'''
    static_ip: bool
    '''Enable or disable static IP configuration.'''
    address: str | None
    '''Address IP configuration.'''
    netmask: str | int | None
    '''Netmask configuration.'''
    gateway: str | None
    '''Gateway configuration.'''
    dns: str | None
    '''DNS configuration.'''

    @field_validator('address', 'gateway', 'dns', mode='before')
    def validate_ip_addresses(cls, value: str, field: ValidationInfo):
        if value is None:
            return value
        if field.field_name in ['address', 'gateway', 'dns']:
            try:
                IPv4Address(value)
            except ValueError:
                raise ValueError(f"Invalid {field.field_name}: {value}")
        return value

    @field_validator('netmask', mode='before')
    def validate_netmask(cls, value):
        if value is None:
            return value
        try:
            IPv4Network(value, strict=False)
        except ValueError:
            raise ValueError(f"Invalid network: {value}")
        return value


class SensorConfig(BaseModel):
    '''
    Configuration of sensor interface.

    Attributes:
        enabled: Enable or disable sensor.
        report_interval_seconds: Interval to send sensor data to broker.
        sampling_interval_seconds: Interval to read sensor data.
    '''

    enabled: bool
    '''Enable or disable sensor.'''
    report_interval_seconds: int
    '''Interval to send sensor data to broker.'''
    sampling_interval_seconds: int
    '''Interval to read sensor data.'''


class Measurements(BaseModel):
    '''
    Configuration of sensor measurements.

    Attributes:
        report_minimum: Enable or disable minimum value.
        report_maximum: Enable or disable maximum value.
        report_average: Enable or disable average value.
        report_median: Enable or disable median value.
        report_last: Enable or disable last value.
    '''

    report_minimum: bool
    '''Enable or disable minimum value.'''
    report_maximum: bool
    '''Enable or disable maximum value.'''
    report_average: bool
    '''Enable or disable average value.'''
    report_median: bool
    '''Enable or disable median value.'''
    report_last: bool
    '''Enable or disable last value.'''

class MQTTConfig(BaseModel):
    '''
    Configuration of MQTT interface.

    Attributes:
        enabled: Enable or disable MQTT.
        host: MQTT broker hostname or IP address.
        port: MQTT broker port.
    '''

    enabled: bool
    '''Enable or disable MQTT.'''
    host: str
    '''MQTT broker hostname or IP address.'''
    port: int
    '''MQTT broker port.'''

class StorageConfig(BaseModel):
    '''
    Configuration of local storage for sensor data.

    Attributes:
        enabled: Enable or disable local storage.
        name: Name of SQLite database file used for storage.
    '''
    enabled: bool
    '''Enable or disable local storage.'''
    name: str
    '''Name of SQLite database file used for storage.'''

class FiberConfig(BaseModel):
    '''
    Configuration of Fiber.

    Attributes:
        version: Version of configuration file.
        system: System configuration.
        sensor: Sensor configuration.
        measurements: Measurements configuration.
        mqtt: MQTT configuration.
        storage: Storage configuration.
    '''
    version: int
    system: SystemConfig
    sensor: SensorConfig
    measurement: Measurements
    mqtt: MQTTConfig
    storage: StorageConfig

