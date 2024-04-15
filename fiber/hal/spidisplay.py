
from fiber.common.thread_manager import pool
from threading import Thread, Event, Lock
from fiber.display.examplewidgets import DateTimeBanner
from fiber.display.sensorwidget import FiberSensorWidget
from fiber.display.st7920display import ST7920Display


class SPIDisplay:
    def __init__(self):
        self.display_thread = None
        self._stop_event = Event()
        self._lock = Lock()

        self.display = ST7920Display()
        self._sensor_widget = FiberSensorWidget(width=self.display.get_width())

        self.display.add_widget(DateTimeBanner(128), 0, 0, 0)
        self.display.add_widget(self._sensor_widget, 0, 16, 0)

    def run_in_thread(self) -> None:
        if self.display_thread is not None:
            self._stop_event.set()
            self.display_thread.join()
        self._stop_event.clear()
        self.display_thread = Thread(target=self.display_main_loop)
        pool.manage_thread(True, self.display_thread, self._stop_event)
        self.display_thread.start()

    def display_main_loop(self) -> None:
        while not self._stop_event.wait(1):
            self.display.run()

    def set_value(self, channel: int, value: float | None) -> None:
        with self._lock:
            self._sensor_widget.set_value(channel, value)

    def set_voltage(self, eth_power: float, bat_power: float) -> None:
        with self._lock:
            self._sensor_widget.set_voltage(eth_power / 100, bat_power / 100)