import threading
import time

import gpiod
from loguru import logger

gpio_lines=[23, 24, 25]

class ButtonControllerTest:
    def __init__(self, chip_path='/dev/gpiochip0', max_brightness=100):
        self.button_thread = threading.Thread(target=self._loop)
        self._stop_event = threading.Event()

        self.gpio_lines = [23, 24, 25]
        self.chip = gpiod.Chip(chip_path)
        self.last_push = {gpio: 0 for gpio in self.gpio_lines} 
        self.max_brightness = max_brightness
        self.current_brightness = 0
        self._configure_lines()

    def _configure_lines(self):
        config = {
            gpio: gpiod.LineSettings(
                direction=gpiod.line.Direction.INPUT,
                edge_detection=gpiod.line.Edge.RISING,
            ) for gpio in self.gpio_lines
        }
        self.request = self.chip.request_lines(consumer='button_controller', config=config)

    def start(self) -> None:
        logger.info('Starting buttons...')
        self.button_thread.start()

    def quit(self) -> None:
        logger.info('Stopping buttons...')
        self._stop_event.set()
        self.button_thread.join()

    def _loop(self):
        try:
            while not self._stop_event.is_set():
                time.sleep(0.2)
                push_events = self.request.read_edge_events()
                current_time = time.time()

                for event in push_events:
                    line_offset = event.line_offset

                    if current_time - self.last_push[line_offset] > 0.5:
                        self.last_push[line_offset] = current_time

                        if event == 23 and self.current_brightness < self.max_brightness:
                            self.current_brightness += 20
                            logger.info(f'Button 1 pressed, brightness set to {self.current_brightness}')
                        elif event == 24:
                            logger.info('Button 2 pressed, Stop for 30 seconds')
                        elif event == 25 and self.current_brightness > 0:
                            self.current_brightness -= 20
                            logger.info(f'Button 3 pressed, brightness set to {self.current_brightness}')
        except KeyboardInterrupt:
            pass
        finally:
            self.request.release()
            self.chip.close()
    
        
if __name__ == '__main__':
    button_controller = ButtonControllerTest()
    button_controller.start()
    try:
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        button_controller.quit()
        logger.info('Button controller stopped')