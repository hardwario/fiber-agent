import threading
from fiber.common.thread_manager import pool
from fiber.cooper.consts import SerialStructure, recv_start, recv_type_lut, items_v1_0_x
from loguru import logger
import time
from threading import Event, Lock
import fcntl
import serial


class GateawayError(Exception):
    def __init__(self, message: str):
        super().__init__(message)


class CommandError(Exception):
    def __init__(self, command: str, message: str = "Command did not work"):
        self.command = command
        self.message = f"{message}: {command}"
        super().__init__(self.message)


class Gateway:
    def __init__(self, device: str) -> None:
        self._device = device
        self._initialize_serial_connection()
        self.on_line = None
        self.on_recv = None
        self._thread = None
        self._event = Event()
        self._command_mutex = Lock()
        self._response = None
        self.is_run = False
        self._configure_device()
        
    def _initialize_serial_connection(self):
        try:
            self._ser = serial.Serial(port=self._device, baudrate=115200, timeout=3)
        except serial.SerialException as e:
            raise GateawayError(f"Could not initialize serial connection: {e}")

        self._ser.flush()
        self._ser.reset_input_buffer()
        self._ser.reset_output_buffer()
        time.sleep(0.5)

    def _configure_device(self):
        try:
            self._ser.write(b'\x1b') 
            self._lock()
            self._speed_up()
            self._command('')
            cgmr = self.get_cgmr()
            self._old_recv = cgmr.startswith(("1.0.", "v1.0."))
            self._recv_type_lut = {header: {'type': recv_type_lut[header]['type'],
                                         'items': [(item[0], item[1]) for item in recv_type_lut[header]['items']]}
                               for header in recv_type_lut}
        except Exception as e:
            raise GateawayError(f"Device configuration failed: {e}")

    def __del__(self) -> None:
        try:
            if self._ser and self._ser.is_open:  
                self._unlock()
                self._ser.close()
        except Exception as e:
            logger.error(f"Error closing serial port: {e}")
        finally:
            self._ser = None

    def start(self) -> None:
        """Run in thread"""
        self._stop_event = threading.Event()
        self._thread = threading.Thread(target=self.cooper_main_loop)
        pool.manage_thread(True, self._thread, self._stop_event)
        self._thread.start()
    
    def cooper_main_loop(self) -> None:
        logger.info("Cooper: OK")

        self.is_run = True
        while not self._stop_event.is_set():    
            try:       
                self._loop()
            except serial.SerialException as e:
                return
    
    def _reconnect_serial(self):
        try:
            self._ser.open()
            logger.info("Reconnecting to the cooper dongle...")
        except serial.SerialException as e:
            logger.error(f"Failed to reconnect: {e}")
            time.sleep(5)
            return False
        return True
    
    def _process_line(self, line: bytes) -> None:
        line = line.decode().strip()
        logger.debug(f"Read line: {line}")

        if self.on_line:
            self.on_line(line)

        if self.on_recv and line.startswith("$RECV:"):
            payload = self._extract_payload(line[7:].split(','))
            self.on_recv(payload)

        elif self._response is not None:
            if line in {'OK', 'ERROR'}:
                logger.debug(f'Event set, response: {self._response}: {line}')
                self._event.set()
            else:
                logger.debug(f'Response append line: {line}')
                self._response.append(line)

    def _extract_payload(self, values: list) -> dict:
        payload = {}
        if self._old_recv:
            payload = {item[0]: (None if value == '' else item[1](value)) for item, value in zip(items_v1_0_x, values)}
        else:
            for i, (item, value) in enumerate(zip(recv_start, values)):
                payload[item[0]] = None if value == '' else item[1](value)

            recv_type = self._recv_type_lut.get(payload['header'], None)

            if recv_type:
                del payload['header']
                payload['type'] = recv_type['type']
                for i, (item, value) in enumerate(zip(recv_type['items'], values[5:])):
                    payload[item[0]] = None if value == '' else item[1](value)
        return payload

    def _loop(self) -> None:
        if not self._ser.is_open and not self._reconnect_serial():
            return 
        
        try:
            logger.debug(f"Waiting for cooper line...")
            line = self._ser.readline()
            if line and line[0] not in ('{', '#'):
                self._process_line(line)
        except serial.SerialException as e:
            logger.info("Disconnecting from the cooper dongle (device disconnected or multiple access on port?)...")
            self._handle_serial_exception()

    def _handle_serial_exception(self) -> None:
        try:
            self._ser.close()
        except Exception as e:
            logger.error(f"Error closing serial port on exception: {e}")
        time.sleep(5)

    def _command(self, command: str) -> list:
        with self._command_mutex:
            command = f'AT{command}\r\n'
            self._event.clear()
            self._response = []
            self._ser.write(command.encode('ascii'))

            if self.is_run:
                if not self._event.wait(timeout=10):
                    logger.error(f"Timeout waiting for response to command: {command}")
            else:
                while not self._event.is_set():
                    self._loop()

            response = self._response
            self._response = None

            return response

    def command(self, command: str, repeat: int=5) -> list:
        for attempt in range(repeat):
            response = self._command(command)
            if response:
                return response
            time.sleep(0.5)
            continue
        raise CommandError(command, f"All attempts failed")

    def get_device_info(self, command: str) -> str:
        response = self.command(command)
        if not response or ':' not in response[0]:
            raise GateawayError(f"Invalid response for {command}")
        return response[0].split(':')[1].strip()

    def get_cgsn(self) -> str:
        return self.get_device_info("+CGSN")

    def get_cgmr(self) -> str:
        return self.get_device_info("+CGMR")

    def _lock(self) -> None:
        if not fcntl or not self._ser:
            return
        try:
            fcntl.flock(self._ser.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except IOError as e:
            raise GateawayError(f'Could not lock device {self._device}')

    def _unlock(self) -> None:
        if not fcntl or not self._ser:
            return
        try:
            fcntl.flock(self._ser.fileno(), fcntl.LOCK_UN)
        except IOError as e:
            raise GateawayError(f'Could not unlock device {self._device}: {e}')

    def _speed_up(self) -> None:
        if not self._ser or not self._ser.is_open: 
            return
        
        TIOCGSERIAL = 0x541E
        TIOCSSERIAL = 0x541F
        ASYNC_LOW_LATENCY = 0x2000

        buf = SerialStructure()

        try:
            with open(self._ser.fileno(), 'wb', closefd=False) as fd:
                fcntl.ioctl(fd, TIOCGSERIAL, buf)
                buf.flags |= ASYNC_LOW_LATENCY
                fcntl.ioctl(fd, TIOCSSERIAL, buf)
        except (IOError, ValueError, serial.SerialException, AttributeError) as e:
            logger.error(f"Failed to apply speed-up settings: {e}")
