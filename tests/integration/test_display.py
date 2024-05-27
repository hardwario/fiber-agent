import spidev
import gpiod
import time
import sys

from copy import deepcopy

from fiber.display.const import RESET_GPIO


class ST7920:
    def __init__(self):
        self.chip = gpiod.Chip('/dev/gpiochip0')
        self.request = self.chip.request_lines(
            consumer='ST7920Display',
            config={
                RESET_GPIO: gpiod.LineSettings(
                    direction=gpiod.line.Direction.OUTPUT,
                ),
            },
        )

        self.request.set_value(RESET_GPIO, gpiod.line.Value.INACTIVE)
        time.sleep(1)
        self.request.set_value(RESET_GPIO, gpiod.line.Value.ACTIVE)

        time.sleep(0.1)

        self.spi = spidev.SpiDev()
        self.spi.open(6, 0)
        self.spi.max_speed_hz = 500000  # needs tuning - up to 1.8 MHz

        self.send(0, 0, 0x30)  # basic instruction set
        self.send(0, 0, 0x30)  # repeated
        self.send(0, 0, 0x0C)
        self.send(0, 0, 0x01)  # DISPLAY CLEAR
        self.send(0, 0, 0x07)  # ENTRY MODE SET

        self.send(0, 0, 0x34)  # enable RE mode
        self.send(0, 0, 0x34)
        self.send(0, 0, 0x36)  # enable graphics display

        self.set_rotation(0)  # rotate to 0 degrees

        self.clear()
        self.currentlydisplayedfbuff = None
        self.redraw()

    def set_rotation(self, rot):
        if rot == 0 or rot == 2:
            self.width = 128
            self.height = 64
        elif rot == 1 or rot == 3:
            self.width = 64
            self.height = 128
        self.rot = rot

    def send(self, rs, rw, cmds):
        if type(cmds) is int:  # if a single arg, convert to a list
            cmds = [cmds]
        b1 = 0b11111000 | ((rw & 0x01) << 2) | ((rs & 0x01) << 1)
        bytes = []
        for cmd in cmds:
            bytes.append(cmd & 0xF0)
            bytes.append((cmd & 0x0F) << 4)
        xfer = self.spi.xfer2([b1] + bytes)
        return xfer

    def clear(self):
        self.fbuff = [[0] * (128 // 8) for i in range(64)]

    def line(self, x1, y1, x2, y2, set=True):
        diffX = abs(x2 - x1)
        diffY = abs(y2 - y1)
        shiftX = 1 if (x1 < x2) else -1
        shiftY = 1 if (y1 < y2) else -1
        err = diffX - diffY
        drawn = False
        while not drawn:
            self.plot(x1, y1, set)
            if x1 == x2 and y1 == y2:
                drawn = True
                continue
            err2 = 2 * err
            if err2 > -diffY:
                err -= diffY
                x1 += shiftX
            if err2 < diffX:
                err += diffX
                y1 += shiftY

    def fill_rect(self, x1, y1, x2, y2, set=True):
        for y in range(y1, y2 + 1):
            self.line(x1, y, x2, y, set)

    def rect(self, x1, y1, x2, y2, set=True):
        self.line(x1, y1, x2, y1, set)
        self.line(x2, y1, x2, y2, set)
        self.line(x2, y2, x1, y2, set)
        self.line(x1, y2, x1, y1, set)

    def plot(self, x, y, set):
        if x < 0 or x >= self.width or y < 0 or y >= self.height:
            return
        if set:
            if self.rot == 0:
                self.fbuff[y][x // 8] |= 1 << (7 - (x % 8))
            elif self.rot == 1:
                self.fbuff[x][15 - (y // 8)] |= 1 << (y % 8)
            elif self.rot == 2:
                self.fbuff[63 - y][15 - (x // 8)] |= 1 << (x % 8)
            elif self.rot == 3:
                self.fbuff[63 - x][y // 8] |= 1 << (7 - (y % 8))
        else:
            if self.rot == 0:
                self.fbuff[y][x // 8] &= ~(1 << (7 - (x % 8)))
            elif self.rot == 1:
                self.fbuff[x][15 - (y // 8)] &= ~(1 << (y % 8))
            elif self.rot == 2:
                self.fbuff[63 - y][15 - (x // 8)] &= ~(1 << (x % 8))
            elif self.rot == 3:
                self.fbuff[63 - x][y // 8] &= ~(1 << (7 - (y % 8)))

    def _send_line(self, row, dx1, dx2):
        self.send(
            0, 0, [0x80 + row % 32, 0x80 + ((dx1 // 16) + (8 if row >= 32 else 0))]
        )  # set address
        self.send(1, 0, self.fbuff[row][dx1 // 8 : (dx2 // 8) + 1])

    def redraw(self, dx1=0, dy1=0, dx2=127, dy2=63, full=False):
        if (
            self.currentlydisplayedfbuff == None
        ):  # first redraw always affects the complete LCD
            for row in range(0, 64):
                self._send_line(row, 0, 127)
            self.currentlydisplayedfbuff = deepcopy(
                self.fbuff
            )  # currentlydisplayedfbuff is initialized here
        else:  # redraw has been called before, since currentlydisplayedfbuff is already initialized
            for row in range(dy1, dy2 + 1):
                if full or (
                    self.currentlydisplayedfbuff[row] != self.fbuff[row]
                ):  # redraw row if full=True or changes are detected
                    self._send_line(row, dx1, dx2)
                    self.currentlydisplayedfbuff[row][dx1 // 8 : (dx2 // 8) + 1] = (
                        self.fbuff[row][dx1 // 8 : (dx2 // 8) + 1]
                    )


def main():
    s = ST7920()
    s.clear()
    s.line(1, 1, 50, 50)
    s.redraw()


if __name__ == '__main__':
    main()
