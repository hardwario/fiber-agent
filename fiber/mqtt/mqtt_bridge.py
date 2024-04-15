import json
import threading

import paho.mqtt.client as mqtt
from pydantic import ValidationError
from fiber.client.handler import ClientHandler
from loguru import logger

from fiber.common.config_manager import ConfigManager, ConfigManagerError


class MQTTError(Exception):
    pass


class CallbackError(Exception):
    pass


class MQTTBridge:
    def __init__(self, client_handler: ClientHandler, config_path: str) -> None:
        self._topic_callback = {}
        self.client_handler = client_handler
        self.mqtt_client = mqtt.Client()
        self._fiber_config = ConfigManager(config_path).config_data
        self._lock = threading.RLock()

        self._fiber_id = self.client_handler.get_fiber_id()
        self.mqtt_client.on_connect = self._on_connect
        self.mqtt_client.on_message = self._on_message
        self.mqtt_client.on_disconnect = self._on_disconnect

        self.mqtt_client.user_data_set(
            {"bridge": self, "fiber_id": self._fiber_id}
        )

        try:
            if self._fiber_config.mqtt.host is None or \
                self._fiber_config.mqtt.port is None:
                self.mqtt_client = None

            else:
                self.mqtt_client.connect(host=self._fiber_config.mqtt.host,
                                     port=int(self._fiber_config.mqtt.port))

                self.mqtt_client.loop_start()
        except ConnectionError as exc:
            raise MQTTError from exc

    def __del__(self) -> None:
        self.mqtt_client.loop_stop()

    def _acc(self, topic: str, callback):
        topic = f"fiber/{self._fiber_id}{topic}"
        self._topic_callback[topic] = callback

    @staticmethod
    def _on_disconnect(client, userdata, rc) -> None:
        logger.error("Broken connection")

    def _on_connect(self, client: mqtt.Client, userdata, flags, rc) -> None:
        client.subscribe(f"fiber/{self._fiber_id}/#", 0)

        self._acc("/system/reboot", self._callback_reboot)
        self._acc("/config/get", self._callback_config_get)
        self._acc("/config/set", self._callback_config_set)
        self._acc("/system/mac/get", self._callback_mac_get)
        self._acc("/system/ip/get", self._callback_ip_get)
        self._acc("/system/uptime/get", self._callback_uptime_get)


        logger.debug("MQTT: Subscribed communication topics")

    def _on_message(self, client, userdata, msg: mqtt.MQTTMessage) -> None:
        try:
            if msg.topic in self._topic_callback:
                logger.debug(f"Topic of the msg: {msg.topic}: {self._topic_callback[msg.topic]}")
                self._topic_callback[msg.topic](
                    msg.topic, json.loads(msg.payload)
                )
            else:
                logger.debug(f"No callback found for topic: {msg.topic}")
        except (CallbackError, json.JSONDecodeError, KeyError):
            self.send_error(msg.topic)

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
        logger.debug(
            f"Sending JSON object message on {prefix}{self._fiber_id}{topic}"
            f" with object {obj}"
        )

        if self.mqtt_client:
            try:
                if obj is not None:
                    obj = json.dumps(obj)

                    with self._lock:
                        self.mqtt_client.publish(
                            f"{prefix}{self._fiber_id}{topic}", obj
                        )
                else:
                    logger.warning("No data to send as JSON")
            except json.JSONDecodeError:
                self.send_error(topic)
        else:
            logger.debug("Can't publish: no MQTT broker defined")   

    def send_config(self) -> None:
        logger.debug("Send configuration")
        topic = "/config"
        config = self._fiber_config.dict()
        self.send_json(topic, config)

    def send_ip(self) -> None:
        logger.debug("Send IP address")
        topic = "/system/ip"
        ip = self.client_handler.get_ip()
        self.send_json(topic, ip)

    def send_mac(self) -> None:
        logger.debug("Send MAC address")
        topic = "/system/mac"
        mac = self.client_handler.get_mac()
        self.send_json(topic, mac)

    def send_uptime(self) -> None:
        logger.debug("Send uptime")
        topic = "/system/uptime"
        uptime = self.client_handler.get_uptime()
        self.send_json(topic, uptime)

    def send_measurements(self, data: dict) -> None:
        logger.debug(f"Send measurements: {data}")
        self.send_json("/measurement", data)

    def send_beacon(self) -> None:
        try:
            beacon_data = self._prepare_beacon_data()
            self.send_json("/beacon", beacon_data)
        except SystemError:
            logger.debug("Problem while receiving uptime, ip_address or mac_address")

    def _prepare_beacon_data(self) -> dict:
        uptime = self.client_handler.get_uptime()
        ip_address = self.client_handler.get_ip()
        mac = self.client_handler.get_mac()
        
        logger.debug(f'SYSTEM INFO: ip - {ip_address}, mac - {mac}')
        return {"uptime": uptime, "ip_address": ip_address, "mac_address": mac}

    def _callback_reboot(self, topic, payload) -> None:
        logger.debug("Reboot request")
        self.client_handler.reboot()

    def _callback_config_set(self, msg: mqtt.MQTTMessage) -> None:
        logger.info("Set configuration")
        try:
            payload_json = json.loads(msg.payload)
            try:
                updated_config = self._fiber_config(**payload_json)
                self._fiber_config = updated_config
            except ValidationError as e:
                logger.error(f"Payload validation error: {e}")
        except (ConfigManagerError, json.JSONDecodeError):
            raise ValueError
        
    def _callback_ip_get(self, topic: str, payload) -> None:
        try:
            self.send_ip()
            self.send_ok(topic)
        except SystemError:
            self.send_error(topic)

    def _callback_mac_get(self, topic: str, payload) -> None:
        try:
            self.send_mac()
            self.send_ok(topic)
        except SystemError:
            self.send_error(topic)

    def _callback_uptime_get(self, topic: str, payload) -> None:
        try:
            self.send_uptime()
            self.send_ok(topic)
        except SystemError:
            self.send_error(topic)

    def _callback_config_get(self, topic: str, payload) -> None:
        try:        
            self.send_config()
            self.send_ok(topic)
        except SystemError:
            self.send_error(topic)