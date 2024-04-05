import time
import spidev
import gpiod

from fiber.display.const import RESET_GPIO, CHIP_SELECT_GPIO
from fiber.display.src.display import Display


class ST7920Display(Display):
    chip: gpiod.Chip
    request: gpiod.LineRequest
    _spi: spidev.SpiDev

    def __init__(self, width, height):
        super().__init__(width, height)

        self.chip = gpiod.Chip("/dev/gpiochip0")
        self.request = self.chip.request_lines(
            consumer="ST7920Display",
            config={
                RESET_GPIO: gpiod.LineSettings(
                    direction=gpiod.line.Direction.OUTPUT,
                ),
                CHIP_SELECT_GPIO: gpiod.LineSettings(
                    direction=gpiod.line.Direction.OUTPUT,
                ),
            },
        )

        self.setup_spi()

    def setup_spi(self):
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

        self.request.set_value(CHIP_SELECT_GPIO, gpiod.line.Value.ACTIVE)
        xfer = self._spi.xfer2(bytes_to_write)
        self.request.set_value(CHIP_SELECT_GPIO, gpiod.line.Value.INACTIVE)

        return xfer

    def send_row(self, row: int, data: bytes):
        row_buf = data[row * self.get_width() : (row + 1) * self.get_width()]
        bits = [1 if byte != 0 else 0 for byte in row_buf]

        compressed_bits = [0] * (len(bits) // 8)
        for i in range(0, len(bits), 8):
            compressed_bits[i // 8] = sum([bits[i + j] << (7 - j) for j in range(8)])

        # set line address
        self.send([0x80 + row % 32, 0x80 + (8 if row >= 32 else 0)])

        # send pixel data
        self.send(compressed_bits, rs=True)

    def draw(self):
        b = self._fb.tobytes()
        rows = [
            b[i * self.get_width() // 8 : (i + 1) * self.get_width() // 8]
            for i in range(self.get_height())
        ]

        for i, row in enumerate(rows):
            self.send_row(i, row)
