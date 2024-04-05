from pydantic import BaseModel


PATH_FIBER_FILE = "/var/fiber/"

class SensorOutput(BaseModel):
    timestamp: int
    channel: int
    thermometer: str
    temperature: float