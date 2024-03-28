import fcntl
import time
from fiber.hal.i2c import I2C
import smbus2


class EEPROM:
    def __init__(self, i2c: I2C, device_addr: int = 0x56) -> None:
        self._i2c = i2c
        self._device_addr = device_addr

    def write(self, memory_addr: int, data: bytes) -> None:
        payload = bytes([memory_addr]) + data
        write = smbus2.smbus2.i2c_msg.write(self._device_addr, payload)
        self._i2c.i2c_rdwr(write)

    def read(self, memory_addr: int | None, length: int | None = 1) -> bytes:
        if not isinstance(memory_addr, int) or not isinstance(length, int):
            raise TypeError

        try:
            fcntl.lockf(self._i2c.fd, fcntl.LOCK_EX)
            time.sleep(0.01)

            write = smbus2.smbus2.i2c_msg.write(self._device_addr, [memory_addr])
            read = smbus2.smbus2.i2c_msg.read(self._device_addr, length)
            self._i2c.i2c_rdwr(write, read)

            return bytes(read)
        except OSError:
            raise 
        finally:
            fcntl.lockf(self._i2c.fd, fcntl.LOCK_UN)


