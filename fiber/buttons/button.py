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
        self.last_push = {gpio: 0 for gpio in GPIO_LINES}

        self.st7920_display = spi_display.display
        self.sensor_widget = spi_display.sensor_widget
        self.current_brightness = spi_display.start_brightness
        self.max_brightness = spi_display.max_brightness

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
        logger.info('Before stop')
        self._stop_event.set()

        if self._button_thread.is_alive():
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
                time.sleep(0.2)
                check_push = self.request.wait_edge_events(0)
                if check_push:
                    push_events = self.request.read_edge_events()
                    logger.info(f'Push events: {len(push_events)}')

                    current_time = time.time()
                    for event in push_events:
                        if self._stop_event.is_set():
                            break

                        line_offset = event.line_offset

                        if current_time - self.last_push[line_offset] > 0.5:
                            self.last_push[line_offset] = current_time

                            if line_offset == 23 and self.current_brightness < self.max_brightness:
                                self.current_brightness += 20
                                self.st7920_display.set_brightness(
                                    self.current_brightness)
                            elif line_offset == 24:
                                self.sensor_widget.freeze_page()
                            elif line_offset == 25 and self.current_brightness > 0:
                                self.current_brightness -= 20
                                self.st7920_display.set_brightness(
                                    self.current_brightness)

                            self.st7920_display.set_buzzer(False)
                            time.sleep(0.02)
                            self.st7920_display.set_buzzer(True)
        except Exception as e:
            logger.error(f'Error in buttons loop: {e}')
        finally:
            try:
                self.request.release()
                self.chip.close()
            except Exception as e:
                logger.error(f'Error with releasing lines: {e}')
