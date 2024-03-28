from pydantic import BaseModel, validator


PATH_FIBER_FILE = "/var/fiber/"

class TowerOutput(BaseModel):
    items: list

    @validator('items', each_item=True)
    def check_item(cls, item):
        if not isinstance(item, (str, float, int)):
            raise ValueError(f'Invalid type: {type(item).__name__}')
        return item

class CooperOutput(BaseModel):
    rssi: int | None = None
    id: str | None = None
    sequence: int | None = None
    uptime: int | None = None
    type: str | None = None
    altitude: int | None = None
    co2_conc: int | None = None
    humidity: float | None = None
    illuminance: int | None = None
    motion_count: int | None = None
    orientation: int | None = None
    press_count: int | None = None
    pressure: int | None = None
    sound_level: int | None = None
    temperature: float | None = None
    voc_conc: int | None = None
    voltage: float | None = None
    gw: str | None = None

class SensorOutput(BaseModel):
    timestamp: int
    channel: int
    thermometer: str
    temperature: float