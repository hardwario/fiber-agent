from dataclasses import dataclass
import gpiod
from fiber.hal.consts import CHIP_NAME
from loguru import logger


CONSUMER = "GPIOManager"


@dataclass
class GPIOPin:
    line: int
    direction: gpiod.line.Direction


class GPIOManager:
    pins: list[GPIOPin]
    request: gpiod.LineRequest | None = None

    def __init__(self, chip_name: str):
        try:
            self.chip = gpiod.Chip(chip_name)
        except (FileNotFoundError, OSError) as e:
            logger.error(f"Failed to open chip {chip_name}: {e}")
            raise
        self.config = {}

    def add_pin(self, line: int, direction: gpiod.line.Direction):
        if not isinstance(line, int) or not isinstance(direction, gpiod.line.Direction):
            logger.error("Invalid line or direction type")
            raise ValueError("Invalid line or direction type")

        if self.request is not None:
            self.request.release()
            self.request = None

        self.config[line] = gpiod.LineSettings(direction=direction)

        self.request = self.chip.request_lines(
            consumer=CONSUMER,
            config=self.config,
        )

    def remove_pin(self, line: int):
        if self.request is None:
            return

        self.request.release()
        self.request = None

        del self.config[line]

    def set_value(self, line: int, value: int):
        if self.request is None:
            logger.error("Attempt to set value without an active request")
            raise OSError("No active GPIO request.")

        self.request.set_value(line, value)

    def read_value(self, line: int):
        if self.request is None:
            logger.error("Attempt to read value without an active request")
            raise OSError("No active GPIO request.")

        return self.request.get_value(line)
    
    def release(self):
        if self.request is not None:
            try:
                for pin in self.config.keys():
                    self.set_value(pin, gpiod.line.Value.INACTIVE)
                self.request.release()
            except OSError as e:
                logger.error(f"Error releasing GPIO: {e}")
            finally:
                self.request = None



gpio_manager = GPIOManager(chip_name=CHIP_NAME)