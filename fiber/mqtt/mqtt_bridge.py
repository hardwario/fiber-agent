import json
import threading
import schedule
import time
import paho.mqtt.client as mqtt
from fiber.client.handler import ClientHandler
from loguru import logger
from fiber.common.config_manager import ConfigManagerError
from fiber.models.configurations import FiberConfig
from fiber.models.system import BeaconBody


class MQTTError(Exception):
    pass


class CallbackError(Exception):
    pass


class MQTTBridge:
    def __init__(self, core_stop_event: threading.Event, client_handler: ClientHandler, fiber_config: FiberConfig) -> None:
        self._topic_callback: dict[str, callable] = {}
        self.client_handler = client_handler
        self._lock = threading.RLock()
        self._core_stop_event = core_stop_event
        self._fiber_id = self.client_handler.get_fiber_id()
        self._fiber_config = fiber_config

        self.mqtt_client = self._create_mqtt_client()

    def close(self) -> None:
        self.mqtt_client.loop_stop()

        self._core_stop_event.set()

        if self.mqtt_thread is not None:
            self.mqtt_thread.join()
            if self.mqtt_thread.is_alive():
                logger.error("Thread did not exit in time")
            else:
                logger.info(f"Thread {self.mqtt_thread.name} exited")

    def _create_mqtt_client(self) -> mqtt.Client | None:
        if self._fiber_config.mqtt.host is None or self._fiber_config.mqtt.port is None:
            return None

        mqtt_client = mqtt.Client()
        mqtt_client.on_connect = self._on_connect
        mqtt_client.on_message = self._on_message
        mqtt_client.on_disconnect = self._on_disconnect

        mqtt_client.user_data_set({"bridge": self, "fiber_id": self._fiber_id})

        try:
            mqtt_client.connect(host=self._fiber_config.mqtt.host, port=int(self._fiber_config.mqtt.port))
            mqtt_client.loop_start()
        except ConnectionError as exc:
            raise MQTTError from exc

        return mqtt_client

    def _acc(self, topic: str, callback: callable) -> None:
        topic = f"fiber/{self._fiber_id}{topic}"
        self._topic_callback[topic] = callback

    @staticmethod
    def _on_disconnect(client, userdata, rc) -> None:
        logger.error("Broken connection")

    def _on_connect(self, client: mqtt.Client, userdata, flags, rc) -> None:
        client.subscribe(f"fiber/{self._fiber_id}/#", 0)
        logger.debug("MQTT: Subscribed communication topics")

    def _on_message(self, client, userdata, msg: mqtt.MQTTMessage) -> None:
        logger.info(f"Received message on topic {msg.topic} with payload {msg.payload}")

        try:
            if msg.topic in self._topic_callback:
                callback = self._topic_callback[msg.topic]
                callback(msg.payload)
            else:
                logger.debug(f"No callback found for topic: {msg.topic}")
        except (CallbackError, json.JSONDecodeError, KeyError, ConfigManagerError):
            self.send_error(msg.topic)

    def start(self) -> None:
        self.mqtt_thread = threading.Thread(target=self._loop)
        self.mqtt_thread.start()

    def _loop(self) -> None:
        schedule.every(1).minute.do(self._send_beacon_data).run()

        logger.info("MQTT: OK")
        while not self._core_stop_event.is_set():
            try:
                schedule.run_pending()
                time.sleep(0.1)
            except MQTTError as e:
                raise SystemError(e)

    def send_ok(self, topic: str) -> None:
        logger.debug(f"Sending OK to {topic}/result")

        if self.mqtt_client:
            with self._lock:
                self.mqtt_client.publish(
                    f"fiber/{self._fiber_id}{topic}/result", json.dumps("ok")
                )

    def send_error(self, topic: str) -> None:
        logger.debug(f"Sending ERROR to {topic}/result")

        if self.mqtt_client:
            with self._lock:
                self.mqtt_client.publish(
                    f"fiber/{self._fiber_id}{topic}/result",
                    json.dumps("error"),
                )

    def send_json(self, topic: str, obj: dict | None, prefix="fiber/") -> None:
        logger.debug(f"Sending JSON object message on {prefix}{self._fiber_id}{topic} with object {obj}")
        if self.mqtt_client:
            try:
                if obj is not None:
                    obj = json.dumps(obj)

                    with self._lock:
                        self.mqtt_client.publish(f"{prefix}{self._fiber_id}{topic}", obj)
                else:
                    logger.warning("No data to send as JSON")
            except json.JSONDecodeError:
                self.send_error(topic)
        else:
            logger.debug("Can't publish: no MQTT broker defined") 
            
    def _send_beacon_data(self) -> None:
        try:
            uptime = self.client_handler.get_uptime()
            ip_address = self.client_handler.get_ip()
            mac = self.client_handler.get_mac()

            beacon_data = BeaconBody(uptime=uptime, ip_address=ip_address, mac_address=mac)
            self.send_json("/beacon", beacon_data.dict())
        except SystemError:
            logger.debug("Problem while receiving uptime, ip_address or mac_address") 
