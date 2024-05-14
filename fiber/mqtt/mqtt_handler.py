import paho.mqtt.client as mqtt
import json

from loguru import logger
from pydantic import ValidationError
from fiber.common.config_manager import ConfigManagerError
from fiber.models.configurations import FiberConfig
from fiber.mqtt.mqtt_bridge import MQTTBridge


class MQTTHandler(MQTTBridge):
    def _on_connect(self, client: mqtt.Client, userdata, flags, rc) -> None:
        super()._on_connect(client, userdata, flags, rc)
        self._acc('/config/get', self.send_config)
        self._acc('/config/set', self.set_config)
        self._acc('/system/mac/get', self.send_mac)
        self._acc('/system/ip/get', self.send_ip)
        self._acc('/system/uptime/get', self.send_uptime)
        self._acc('/system/reboot', self.system_reboot)

    def send_ip(self, payload: None) -> None:
        try:
            topic = '/system/ip'
            ip = self.client_handler.get_ip()
            self.send_json(topic, ip)            
            self.send_ok(topic)
        except SystemError:
            self.send_error(topic)

    def send_mac(self, payload: None) -> None:
        try:
            my_topic = '/system/mac'
            mac = self.client_handler.get_mac()
            self.send_json(my_topic, mac)
            self.send_ok(my_topic)
        except SystemError:
            self.send_error(my_topic)

    def send_uptime(self, payload: None) -> None:
        try:
            topic = '/system/uptime'
            uptime = self.client_handler.get_uptime()
            self.send_json(topic, uptime)
            self.send_ok(topic)
        except SystemError:
            self.send_error(topic)

    def send_config(self, payload: None) -> None:
        try:        
            topic = '/config'
            config = self._fiber_config.dict()
            self.send_json(topic, config)
            self.send_ok(topic)
        except SystemError:
            self.send_error(topic)

    def set_config(self, payload: dict) -> None:
        logger.info('Set configuration')
        try:
            topic = '/config'
            payload_json = json.loads(payload)
            updated_config = FiberConfig(**payload_json)
            self._fiber_config = updated_config
            self.send_ok(topic)
            
        except (ValidationError, TypeError) as e:
            logger.error(f'Payload validation error: {e}')
        except (ConfigManagerError, json.JSONDecodeError):
            self.send_error(topic) 

    def system_reboot(self, payload: None) -> None:
        logger.debug('Reboot request')
        self.client_handler.reboot()

    def send_measurements(self, data: dict) -> None:
        topic = '/measurement'
        self.send_json(topic, data)
