
from threading import Event, Lock, Thread

from loguru import logger

from fiber.display.examplewidgets import DateTimeBanner
from fiber.display.sensorwidget import FiberSensorWidget
from fiber.display.st7920display import ST7920Display


class SPIDisplay:
    def __init__(self):
        self._display_thread = Thread(target=self._loop)
        self._stop_event = Event()

        self._lock = Lock()

        self.start_brightness = 40
        self.display = ST7920Display(width=128, height=64, brightness=self.start_brightness)
        self.sensor_widget = FiberSensorWidget(width=self.display.get_width())

        self.display.add_widget(DateTimeBanner(128), 0, 0, 0)
        self.display.add_widget(self.sensor_widget, 0, 16, 0)

    def quit(self) -> None:
        with self._lock:
            self._stop_event.set()

            if self._display_thread is not None:
                self._display_thread.join()

                if self._display_thread.is_alive():
                    logger.error('Thread did not exit in time')
                else:
                    logger.info(f'Thread {self._display_thread.name} exited')

    def start(self) -> None:
        logger.info('SPI Display: OK')
        self._display_thread.start()

    def _loop(self) -> None:
        while not self._stop_event.wait(0.2):
            self.display.run()

        self.display.quit()

    def set_value(self, channel: int, value: float | None) -> None:
        with self._lock:
            self.sensor_widget.set_value(channel, value)

    def set_voltage(self, eth_power: float, bat_power: float) -> None:
        with self._lock:
            self.sensor_widget.set_voltage(eth_power / 100, bat_power / 100)
