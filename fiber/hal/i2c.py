import threading
import smbus2


class I2C:
    def __init__(self, bus: int) -> None:
        self._bus = smbus2.smbus2.SMBus(bus)
        self._lock = threading.RLock()
        self.fd = self._bus.fd

    def write_byte_data(self, i2c_addr: int, register: int, value: int) -> None:
        with self._lock:
            self._bus.write_byte_data(i2c_addr, register, value)

    def i2c_rdwr(self, *msgs) -> None:
        with self._lock:
            self._bus.i2c_rdwr(*msgs)
