from fiber.cooper.gateway import Gateway, GateawayError
from fiber.common.queue_manager import QueueManager

from loguru import logger


class CooperManagerError(Exception):
    def __init__(self, message: str):
        super().__init__(message)


class CooperManager:
    def __init__(self, device: str, cooper_data_queue: QueueManager):
        try:
            self.device = device
            self.cooper_data_queue = cooper_data_queue
            self.gateway = Gateway(self.device)
            self.gateway_serial = None
        except GateawayError as e:
            raise CooperManagerError(f"Error initializing Cooper: {e}")

    def start(self):
        try:
            self.gateway_serial = self.gateway.get_cgsn()
            self.gateway.on_recv = self.on_recv
            self.gateway.start()
        except Exception as e:
            raise CooperManagerError(f"Error at Cooper loop: {e}")
    
    def on_recv(self, payload):
        logger.info(payload)
        logger.debug(f"Cooper msg to queue: {{'press_count': '{payload['press_count']}', 'rssi': {payload['rssi']}, 'humidity': {payload['humidity']}, 'temperature': {payload['temperature']}}}...")
        payload['gw'] = self.gateway_serial
        self.cooper_data_queue.send_qmsg(payload)