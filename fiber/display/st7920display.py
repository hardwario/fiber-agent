import os
import threading
import time

import gpiod
import spidev
from loguru import logger

from fiber.display.const import BUZZER_GPIO, PWM_PERIOD, RESET_GPIO
from fiber.display.src.display import Display


class ST7920Display(Display):
    chip: gpiod.Chip
    request: gpiod.LineRequest
    _spi: spidev.SpiDev

    _brightness: int
    _buzzer_on: bool

    def __init__(self, width, height, brightness=0):
        super().__init__(width, height)

        self._request_lock = threading.Lock()
        self._brightness = brightness

        self._configure_pwm()
        self._configure_gpiod()

        self.setup_spi()
        self.set_brightness(self._brightness)

    def _configure_pwm(self):
        if not self._check_file_presence('/sys/class/pwm/pwmchip0', 3):
            raise RuntimeError('PWM not available')

        if not os.path.exists('/sys/class/pwm/pwmchip0/pwm1'):
            self._export_pwm()

        self._set_pwm_period(PWM_PERIOD)
        self._set_pwm_duty_cycle(self._brightness)
        
        self._enable_pwm()

    def _configure_gpiod(self):
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
                },
            )

            self.request.set_value(BUZZER_GPIO, gpiod.line.Value.ACTIVE)

    def quit(self):
        with self._request_lock:
            self.request.release()
            self.chip.close()
            self._disable_pwm()
            self._unexport_pwm()

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
        reversed_data = [int(f'{byte:08b}'[::-1], 2) for byte in data[::-1]]
        # set line address
        self.send([0x80 + row % 32, 0x80 + (8 if row >= 32 else 0)])
        # send pixel data
        self.send(reversed_data, rs=True)

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

    def set_brightness(self, brightness: int):
        self._brightness = max(0, min(100, brightness))
        self._set_pwm_duty_cycle(self._brightness)

    def set_buzzer(self, on: bool):
        self._buzzer_on = on
        self.request.set_value(BUZZER_GPIO, gpiod.line.Value.ACTIVE if on else gpiod.line.Value.INACTIVE)

    def _check_file_presence(self, path: str, timeout: int) -> bool:
        start = time.time()
        while time.time() - start < timeout:
            if os.path.exists(path):
                return True
            time.sleep(0.1)
        return False

    def _export_pwm(self) -> None:
        '''Export channel pwm0'''
        with open('/sys/class/pwm/pwmchip0/export', 'w') as f:
            f.write('1')  

    def _unexport_pwm(self) -> None:
        '''Cancel export of channel pwm0'''
        with open('/sys/class/pwm/pwmchip0/unexport', 'w') as f:
            f.write('1')  

    def _enable_pwm(self) -> None:
        '''Turn on PWM'''
        with open('/sys/class/pwm/pwmchip0/pwm1/enable', 'w') as f:
            f.write('1')  

    def _disable_pwm(self) -> None:
        '''Switching off PWM'''
        with open('/sys/class/pwm/pwmchip0/pwm1/enable', 'w') as f:
            f.write('0')  # 

    def _set_pwm_period(self, period_ns: int) -> None:
        '''
        Set the period in nanoseconds
            
        Args:
            period_ns (int): The period in nanoseconds.
        '''
        with open('/sys/class/pwm/pwmchip0/pwm1/period', 'w') as f:
            f.write(str(period_ns))

    def _set_pwm_duty_cycle(self, duty_cycle: int) -> None:
        '''
        Set the duty cycle in percent
        
        Args:
            duty_cycle (int): The duty cycle in percent.
        '''
        percent_to_ns = (PWM_PERIOD // 100) * duty_cycle
        with open('/sys/class/pwm/pwmchip0/pwm1/duty_cycle', 'w') as f:
            f.write(str(percent_to_ns))  
