import threading
import time

import gpiod
from loguru import logger

from fiber.common.consts import GPIO_LINES, PATH_CHIP
from fiber.display.spidisplay import SPIDisplay


class ButtonController:
    def __init__(self, spi_display: SPIDisplay) -> None:
        self._button_thread = threading.Thread(target=self._loop)
        self._stop_event = threading.Event()

        self.chip = gpiod.Chip(PATH_CHIP)
        self.last_press = {gpio: 0 for gpio in GPIO_LINES}

        self.st7920_display = spi_display.display
        self.sensor_widget = spi_display.sensor_widget
        self.current_brightness = spi_display.start_brightness

        self._configure_lines()

    def _configure_lines(self) -> None:
        config = {
            gpio: gpiod.LineSettings(
                direction=gpiod.line.Direction.INPUT,
                edge_detection=gpiod.line.Edge.RISING,
            ) for gpio in GPIO_LINES
        }
        self.request = self.chip.request_lines(
            consumer='button_controller', config=config)

    def quit(self) -> None:
        self._stop_event.set()

        if self._button_thread is not None:
            self._button_thread.join(timeout=10)

            if self._button_thread.is_alive():
                logger.error('Thread did not exit in time')
            else:
                logger.info(f'Thread {self._button_thread.name} exited')

    def start(self) -> None:
        logger.debug('Starting buttons...')
        self._button_thread.start()

    def _loop(self) -> None:
        logger.info('Button controller: OK')
        try:
            while not self._stop_event.is_set():
                time.sleep(0.05)

                try:
                    is_press = self.request.wait_edge_events(0)
                except gpiod.LineRequestError as e:
                    logger.error(f'Error waiting for edge events: {e}')
                    break

                if is_press:
                    try:
                        press_events = self.request.read_edge_events()
                    except gpiod.LineRequestError as e:
                        logger.error(f'Error reading edge events: {e}')
                        break

                    current_time = time.time()
                    for event in press_events:
                        line_offset = event.line_offset

                        if current_time - self.last_press[line_offset] > 0.15:
                            self.last_press[line_offset] = current_time

                            if line_offset == 23 and self.current_brightness < 100:
                                self.current_brightness += 20
                                self.st7920_display.set_brightness(
                                    self.current_brightness)
                            elif line_offset == 24:
                                self.sensor_widget.freeze_page(freeze_time=30)
                            elif line_offset == 25 and self.current_brightness > 0:
                                self.current_brightness -= 20
                                self.st7920_display.set_brightness(
                                    self.current_brightness)

                            self.st7920_display.set_buzzer(False)
                            time.sleep(0.02)
                            self.st7920_display.set_buzzer(True)

        except (OSError, gpiod.LineRequestError, gpiod.ChipError) as e:
            logger.error(f'Error in buttons loop: {e}')
        finally:
            try:
                self.request.release()
                self.chip.close()
            except (gpiod.ChipError, gpiod.LineRequestError) as error:
                logger.error(f'Error with releasing lines: {error}')
