from dataclasses import dataclass
from datetime import datetime, timedelta

from PIL import ImageDraw

from fiber.display.const import FONT_SMALL
from fiber.display.src.widget import Widget


@dataclass
class FiberSensor:
    channel: int
    value: float | None


class FiberSensorWidget(Widget):
    # Shows up to 4 sensors on the screen at the same time
    # Will swap to the next sensors every 3 secondss

    _sensor_page: int
    _sensors: list[FiberSensor]

    def __init__(self, width: int, page_swap_time: int = 5):
        super().__init__(width, 10 * 4)  # 9 pixels per sensor, 4 sensors
        self._sensor_page = 0
        self._sensors = []
        self._timer = datetime.now() + timedelta(seconds=page_swap_time)

    def set_value(self, channel: int, value: float | None):
        # sets value for sensor, if it does not exist, add it
        sensor: FiberSensor
        for sensor in self._sensors:
            if sensor.channel == channel:
                sensor.value = value
                self._changed = True
                return

        self._sensors.append(FiberSensor(channel, value))
        self._sensors.sort(key=lambda x: x.channel)

    def update(self):
        # every 5 seconds, swap to the next sensors
        if datetime.now() > self._timer:
            self._timer = datetime.now() + timedelta(seconds=5)
            self._changed = True

            if (self._sensor_page + 1) * 4 >= len(self._sensors):
                self._sensor_page = 0
            else:
                self._sensor_page += 1

    def draw(self):
        draw = ImageDraw.Draw(self.fb)
        draw.rectangle((0, 0, self.get_width(), self.get_height()), fill=0)

        for i, sensor in enumerate(
            self._sensors[
                self._sensor_page
                * 4 : min(self._sensor_page * 4 + 4, len(self._sensors))
            ]
        ):

            draw.text(
                (0, i * 9),
                f"Temp {sensor.channel}:",
                font=FONT_SMALL,
                fill=255,
            )

            sensor_text = f"{sensor.value} ºC" if sensor.value is not None else "---"
            draw.text(
                (self.get_width() - draw.textlength(sensor_text), i * 9),
                sensor_text,
                font=FONT_SMALL,
                align="right",
                fill=255,
            )
