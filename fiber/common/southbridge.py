from dataclasses import dataclass
from loguru import logger

@dataclass
class Response:
    voltage_eth: int # tenths of uV
    voltage_bat: int # tenths of uV


class SouthBridge():
    _1wire: list[int]
    leds: list[int]

    def __init__(self):
        self.leds = [0 for _ in range(18)]

    def set_led(self, led_index: int, state: int) -> None:
        self.leds[led_index] = state

    def flush(self) -> None | Response:
        logger.debug(list(zip(self.leds[::2], self.leds[1::2])))
        return Response(0, 0)

    def reset_leds(self) -> None:
        self.leds = [0 for _ in range(18)]
        self.flush()

