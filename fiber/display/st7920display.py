import threading
import time

import gpiod
import spidev
from loguru import logger

from fiber.display.const import (BRIGHTNESS_PWM_GPIO, BUZZER_GPIO,
                                 PWM_HALF_PERIOD, RESET_GPIO)
from fiber.display.src.display import Display


class ST7920Display(Display):
    chip: gpiod.Chip
    request: gpiod.LineRequest
    _request_lock: threading.Lock
    _spi: spidev.SpiDev

    _bright_thread: threading.Thread | None = None
    _bright_thread_stop: threading.Event
    _brightness: int

    _buzzer_thread: threading.Thread | None = None
    _buzzer_thread_stop: threading.Event
    _buzzer_on: bool

    def __init__(self, width, height, brightness=0):
        super().__init__(width, height)

        self._bright_thread = threading.Thread(target=self._loop)
        self._buzzer_thread_stop = threading.Event()
        self._bright_thread_stop = threading.Event()
        self._request_lock = threading.Lock()

        self.chip = gpiod.Chip('/dev/gpiochip0')
        with self._request_lock:
            self.request = self.chip.request_lines(
                consumer='ST7920Display',
                config={
                    RESET_GPIO: gpiod.LineSettings(
                        direction=gpiod.line.Direction.OUTPUT,
                    ),
                    BUZZER_GPIO: gpiod.LineSettings(
                        direction=gpiod.line.Direction.OUTPUT,
                    ),
                    BRIGHTNESS_PWM_GPIO: gpiod.LineSettings(
                        direction=gpiod.line.Direction.OUTPUT,
                    ),
                },
            )

            self.request.set_value(BUZZER_GPIO, gpiod.line.Value.ACTIVE)

        self.setup_spi()

        self.set_brightness(brightness)
        self.start_brightness_thread()

    def quit(self):
        with self._request_lock:
            self._bright_thread_stop.set()
            self._buzzer_thread_stop.set()

        if self._bright_thread is not None:
            self._bright_thread.join()
            if self._bright_thread.is_alive():
                logger.error(f'Thread {self._bright_thread.name} did not exit in time')
            else:
                logger.info(f'Thread {self._bright_thread.name} exited')

        if self._buzzer_thread is not None:
            self._buzzer_thread.join()
            if self._buzzer_thread.is_alive():
                logger.error(f'Thread {self._buzzer_thread.name} did not exit in time')
            else:
                logger.info(f'Thread {self._buzzer_thread.name} exited')

        self._spi.close()

        with self._request_lock:
            self.request.release()
            self.chip.close()

    def setup_spi(self):
        with self._request_lock:
            self.request.set_value(RESET_GPIO, gpiod.line.Value.INACTIVE)
            time.sleep(1)
            self.request.set_value(RESET_GPIO, gpiod.line.Value.ACTIVE)

        self._spi = spidev.SpiDev()
        self._spi.open(6, 0)
        self._spi.max_speed_hz = 1000000

        self.send([0x30])  # basic instruction set
        self.send([0x30])  # repeated
        self.send([0x0C])
        self.send([0x01])  # DISPLAY CLEAR
        self.send([0x02])  # RETURN HOME CURSOR
        self.send([0x07])  # ENTRY MODE SET

        self.send([0x34])  # enable RE mode
        self.send([0x34])
        self.send([0x36])  # enable graphics display

    def send(self, data: bytearray, rs: bool = False, rw: bool = False):
        b1 = 0b11111000 | ((rw & 0x01) << 2) | ((rs & 0x01) << 1)
        bytes_to_write = bytearray([b1])
        for b in data:
            bytes_to_write.append(b & 0xF0)
            bytes_to_write.append((b << 4) & 0xF0)

        with self._request_lock:
            xfer = self._spi.xfer2(bytes_to_write)

        return xfer

    def send_row(self, row: int, data: bytes):
        # reverse bits in all bytes
        data = data[::-1]
        data = [int(f'{byte:08b}'[::-1], 2) for byte in data]

        # set line address
        self.send([0x80 + row % 32, 0x80 + (8 if row >= 32 else 0)])

        # send pixel data
        self.send(data, rs=True)

    def draw(self):
        b = self._fb.tobytes()
        rows = [
            b[i * self.get_width() // 8: (i + 1) * self.get_width() // 8]
            for i in range(self.get_height())
        ]

        for row, data in enumerate(rows):
            self.send_row(63 - row, data)
        
        self.send([0x34])  # enable RE mode
        self.send([0x34])
        self.send([0x36])  # enable graphics display

    def start_brightness_thread(self):
        self._bright_thread.start()

    def _loop(self):
        while not self._bright_thread_stop.is_set():
            active_time = PWM_HALF_PERIOD * (self._brightness / 100)
            inactive_time = PWM_HALF_PERIOD - active_time
            if self._brightness > 0:
                with self._request_lock:
                    self.request.set_value(
                        BRIGHTNESS_PWM_GPIO, gpiod.line.Value.ACTIVE)
            time.sleep(active_time/1000)

            if self._brightness < 100:
                with self._request_lock:
                    self.request.set_value(
                        BRIGHTNESS_PWM_GPIO, gpiod.line.Value.INACTIVE)
            time.sleep(inactive_time/1000)

    def set_brightness(self, brightness: int):
        if brightness < 0:
            self._brightness = 0
        elif brightness > 100:
            self._brightness = 100
        else:
            self._brightness = brightness

    def set_buzzer(self, on: bool):
        self._buzzer_on = on
        if self._buzzer_on:
            with self._request_lock:
                self.request.set_value(BUZZER_GPIO, gpiod.line.Value.ACTIVE)
        else:
            with self._request_lock:
                self.request.set_value(BUZZER_GPIO, gpiod.line.Value.INACTIVE)
