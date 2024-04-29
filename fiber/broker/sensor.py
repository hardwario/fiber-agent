import time
import threading
from queue import Empty

from loguru import logger
from pydantic import ValidationError

from fiber.client.handler import ClientHandler
from fiber.models.configurations import FiberConfig
from fiber.broker.local_storage import LocalStorage
from fiber.common.queue_manager import QueueManager
from fiber.mqtt.mqtt_handler import MQTTHandler
from fiber.models.sensor import SensorOutput


class SensorBrokerError(Exception):
    pass


class AlreadyRunningThread(SensorBrokerError):
    pass


class Timeout(Exception):
    pass


class AfterReportInterval(Exception):
    pass


class SensorBroker:
    def __init__(self, client_handler: ClientHandler, fiber_config: FiberConfig, sensor_temperature_queue: QueueManager) -> None:
        self._sensor_thread = threading.Thread(target=self._loop)
        self._stop_event = threading.Event()

        self.fiber_config = fiber_config
        
        if fiber_config.mqtt.enabled:
            self._mqtt = MQTTHandler(self._stop_event, client_handler, fiber_config)
            self._mqtt.start()
        else:
            self._mqtt = None
 
        self._storage = (
            LocalStorage(self.fiber_config.storage.name)
            if self.fiber_config.storage.enabled
            else None
        )

        self.sensor_temperature_queue = sensor_temperature_queue
        self._lock = threading.RLock()
        self.load_intervals()
        self._sensor_data = SensorData(int(time.time()), self._sampling_interval, self._report_interval)

    def load_intervals(self) -> None:
        with self._lock:
            self._sampling_interval = self.fiber_config.sensor.sampling_interval_seconds
            self._report_interval = self.fiber_config.sensor.report_interval_seconds

    def close(self) -> None:
        self._stop_event.set()

        if self._sensor_thread is not None:
            self._sensor_thread.join()
            if self._sensor_thread.is_alive():
                logger.error("Thread did not exit in time")
            else:
                logger.info(f"Thread {self._sensor_thread.name} exited")

    def start(self) -> None:
        self.load_intervals()
        self._sensor_data.reset()
        self._sensor_thread.start()

    def _loop(self) -> None:
        logger.info("Broker Sensor: OK")

        while not self._stop_event.is_set():
            try:
                recv = self._recv()
                if recv is not None:
                    with self._lock:
                        self._handle_received_data(recv)
            except Timeout:
                self._handle_timeout()

        if self._mqtt:
            self._mqtt.close()

    def _recv(self) -> SensorOutput | None:
        try:
            recv = self.sensor_temperature_queue.recv_qmsg(
                self._stop_event, timeout=0.1, empty_error=True
            )
            if recv is not None:
                validated_recv = SensorOutput(**recv)

                return validated_recv
        except Empty:
            raise Timeout
        except (KeyError, ValidationError) as exc:
            raise SensorBrokerError from exc

    def _handle_received_data(self, recv: SensorOutput) -> None:
        try:
            self._sensor_data.recv(recv)
        except AfterReportInterval:
            self.send_report()
            self._sensor_data.reset()
            self._sensor_data.recv(recv)

    def _handle_timeout(self) -> None:
        if self._sensor_data.ready_to_send:
            self.send_report()
            self._sensor_data.reset()

    def send_report(self) -> None:
        with self._lock:
            if self._sensor_data.report:
                if self._mqtt:
                    self._mqtt.send_measurements(self._sensor_data.report)
                if self._storage:
                    self._storage.add_report(int(time.time()), self._sensor_data.report)


class Measurement:
    def __init__(self, timestamp: int) -> None:
        self._values: list[int | float] = []
        self._timestamp = timestamp

    def add_sample(self, value: int | float) -> None:
        if not isinstance(value, (int, float)):
            raise TypeError
        self._values.append(value)

    @property
    def average(self) -> int | float | None:
        if not self._values:
            return None
        return round(sum(self._values) / len(self._values), 2)

    @property
    def min(self) -> int | float | None:
        if not self._values:
            return None
        return min(self._values)

    @property
    def max(self) -> int | float | None:
        if not self._values:
            return None
        return max(self._values)

    @property
    def median(self) -> int | float | None:
        if not self._values:
            return None
        
        n = len(self._values)
        self._values.sort()
        if n % 2 == 0:
            m1 = self._values[n//2]
            m2 = self._values[n//2 - 1]
            return (m1 + m2)/2
        else:
            return self._values[n//2]

    @property
    def samples(self) -> int:
        return len(self._values)

    @property
    def timestamp(self) -> int:
        return self._timestamp


class Report:
    def __init__(self, timestamp_start: int, sampling_interval: int) -> None:
        self._samples: list[Measurement] = []
        self._sampling_interval = sampling_interval
        self._sample_ts_start = timestamp_start
        self._sample_ts_end = self._sample_ts_start + sampling_interval

        self._samples.append(Measurement(self._sample_ts_start))

    def add_measurement(self, timestamp: int | None, value: float | None) -> None:
        if not isinstance(value, float) or not isinstance(timestamp, int):
            raise TypeError

        if self.last_sample is None or timestamp > self._sample_ts_end:
            self._create_new_sample()

        if self.last_sample is not None:
            self.last_sample.add_sample(value)

    def _create_new_sample(self) -> None:
        self._sample_ts_start = self._sample_ts_end
        self._sample_ts_end += self._sampling_interval
        self._samples.append(Measurement(self._sample_ts_start))

    @property
    def last_sample(self) -> Measurement | None:
        return self._samples[-1] if self._samples else None

    @property
    def report(self) -> list[dict[str, int | float]]:
        return [
            {
                "timestamp": sample.timestamp,
                "value": {
                    "average": sample.average,
                    "median": sample.median,
                    "minimum": sample.min,
                    "maximum": sample.max,
                },
                "count": sample.samples,
            }
            for sample in self._samples
        ]


class SensorData:
    def __init__(self, start: int, sampling_interval: int, report_interval: int) -> None:
        self._sampling_interval = sampling_interval
        self._report_interval = report_interval
        self._ts_start = start
        self._ts_end = self._ts_start + report_interval
        self._data: dict[int, dict[int, Report]] = {}

    def recv(self, measurement: SensorOutput) -> None:
        if measurement.channel not in self._data:
            self._data[measurement.channel] = {}

        if measurement.timestamp <= self._ts_end:
            report = self._data[measurement.channel].get(measurement.thermometer)
            if report is None:
                report = Report(self._ts_start, self._sampling_interval)
                self._data[measurement.channel][measurement.thermometer] = report
            report.add_measurement(measurement.timestamp, measurement.temperature)
        else:
            raise AfterReportInterval

    @property
    def report(self) -> dict:
        ret = {}

        for channel, thermometers in self._data.items():
            ret[channel] = {therm: therm_data.report for therm, therm_data in thermometers.items()}

        return ret

    def reset(
        self,
        new_sampling_interval: int | None = None,
        new_report_interval: int | None = None,
    ) -> None:
        sampling_interval = (
            self._sampling_interval
            if new_sampling_interval is None
            else new_sampling_interval
        )

        report_interval = (
            self._report_interval
            if new_report_interval is None
            else new_report_interval
        )

        start = self._ts_end

        self.__init__(start, sampling_interval, report_interval)

    @property
    def ready_to_send(self) -> bool:
        return int(time.time()) > self._ts_end
