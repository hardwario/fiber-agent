from pydantic import BaseModel


class MeasurementValues(BaseModel):
    average: float | int | None
    median: float | int | None
    minimum: float | int | None
    maximum: float | int | None

class Measurement(BaseModel):
    timestamp: int
    value: MeasurementValues
    count: int

