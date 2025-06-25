from dataclasses import dataclass
from loguru import logger
from serial import Serial 

@dataclass
class Response:
    voltage_eth: int # tenths of uV
    voltage_bat: int # tenths of uV


class SouthBridge():
    _1wire: list[int]
    leds: list[int]
    _serial: serial

    def __init__(self):
        self.leds = [0 for _ in range(18)]

        self._serial = Serial(
            port='/dev/ttyAMA2',   # UART device
            baudrate=115200,       # Match this to your STM32 config
            timeout=1              # Timeout in seconds
        )

        sleep(2)

        reset_leds()

    def set_led(self, led_index: int, state: int) -> None:
        logger.debug(f'SouthBridge.set_led({led_index}, {state})')
        self._serial.write(bytes([0x02, led_index, 255 if state > 255 else state]))
        response = self._serial.read(1)
        if response != bytes([0x01]):
            logger.debug(f'SouthBridge.set_led({led_index}, {state}) failed')
            return
        self.leds[led_index] = state

    def read_voltage(self) -> Response:
        logger.debug('SouthBridge.read_voltage()')
        self._serial.write(bytes([0x01, 0, 0, 0]))
        response = self._serial.read(8)
        if response == b'':
            logger.debug('SouthBridge.read_voltage() failed')
            return None
        Response(int.from_bytes(response[0:4], 'little'), int.from_bytes(response[4:8], 'little'))

    def reset_leds(self) -> None:
        self._serial.write(bytes([0x02, 255, 0, 0]))
        if self._serial.read(1) == b'\x00':
            logger.debug('SouthBridge.reset_leds() failed')
            return
        self.leds = [0 for _ in range(18)]

