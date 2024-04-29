from pydantic import BaseModel


class SensorOutput(BaseModel):
    """
    Represents temperature measurement from a sensor.

    Attributes:
        timestamp (int): Timestamp when the measurement was made.
        channel (int): Sensor channel (1-8).
        thermometer (str): Thermometer name.
        temperature (float): The temperature value measured by the thermometer, expressed in degrees Celsius.
    """

    timestamp: int
    """Timestamp when the measurement was made."""
    channel: int
    """Sensor channel (1-8)."""
    thermometer: str
    """Thermometer name."""
    temperature: float
    """The temperature value measured by the thermometer, expressed in degrees Celsius."""
