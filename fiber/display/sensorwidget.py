from dataclasses import dataclass
from datetime import datetime, timedelta

from PIL import ImageDraw

from fiber.display.const import FONT_SMALL
from fiber.display.src.widget import Widget


@dataclass
class FiberSensor:
    channel: int
    value: float


class FiberSensorWidget(Widget):
    # Shows up to 4 sensors on the screen at the same time
    # Will swap to the next sensors every 3 secondss

    _sensor_page: int
    _sensors: list[FiberSensor]
    _eth_power: float
    _bat_power: float

    def __init__(self, width: int, page_swap_time: int = 5):
        super().__init__(width, 10 * 5)  # 9 pixels per sensor, 4 sensors + 1 for voltage
        self._sensor_page = 0
        self._sensors = []
        self._timer = datetime.now() + timedelta(seconds=page_swap_time)
        self._eth_power = None
        self._bat_power = None

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

    def set_voltage(self, eth_power: float, bat_power: float):
        self._eth_power = eth_power
        self._bat_power = bat_power
        self._changed = True

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

            if sensor.value is not None:
                sensor_text = f"{sensor.value:.2f} ºC"
            else:
                sensor_text = "--- ºC"

            draw.text(
                (self.get_width() - draw.textlength(sensor_text), i * 9),
                sensor_text,
                font=FONT_SMALL,
                align="right",
                fill=255,
            )
        
        draw.text(
            (0, 36),
            f"PoE/Bat:",
            font=FONT_SMALL,
            fill=255,
        )

        if self._eth_power is not None and self._bat_power is not None:
            power_text = f"{self._eth_power:.2f}/{self._bat_power:.2f}mV"
        else:
            power_text = "---/--- mV"
        draw.text(
            (self.get_width() - draw.textlength(power_text), 36),
            power_text,
            font=FONT_SMALL,
            align="right",
            fill=255,
        )