from datetime import datetime
from PIL import ImageDraw, Image
from fiber.display.const import FONT_SMALL
from fiber.display.src.widget import Widget


class DateTimeBanner(Widget):
    _time_text: str
    _date_text: str

    def __init__(self, width: int):
        super().__init__(width, 9)
        self._time_text = ""
        self._date_text = ""

    def update(self):
        now = datetime.now()
        formatted_time = now.strftime("%H:%M:%S")

        if formatted_time != self._time_text:
            self._time_text = formatted_time
            self._date_text = now.strftime("%d/%m/%Y")
            self._changed = True

    def draw(self):
        draw = ImageDraw.Draw(self.fb)
        draw.rectangle((0, 0, self.get_width(), self.get_height()), fill=0)

        draw.text(
            (0, 0),
            self._time_text,
            font=FONT_SMALL,
            fill=255,
        )

        draw.text(
            (
                self.get_width() - draw.textlength(self._date_text, font=FONT_SMALL),
                0,
            ),
            self._date_text,
            font=FONT_SMALL,
            align="right",
            fill=255,
        )
