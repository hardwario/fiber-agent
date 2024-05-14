
from uuid import uuid1, UUID
import threading
from pydantic import ValidationError
import json
from loguru import logger

from fiber.models.request import Request
from fiber.models.response import Response
from fiber.common.queue_manager import QueueManager


class ClientManager:
    def __init__(self, core_stop_event: threading.Event, server_response_queue: QueueManager, client_request_queue: QueueManager) -> None:
        self.server_response_queue = server_response_queue
        self.client_request_queue = client_request_queue
        self._core_stop_event = core_stop_event

    def check_response(self, received_msg: dict) -> Response:
        try:
            msg = Response(**received_msg)
        except ValidationError as e:
            raise SystemError(e)

        if msg.error is True:
            logger.error('Server response error')
            raise SystemError
        
        return msg

    def _recv(self) -> int | float | str:
        try:
            logger.debug('Client RECV: Trying to get message from server')
            received_msg = self.server_response_queue.recv_qmsg(stop_event=self._core_stop_event)
            if received_msg is not None:
                msg = self.check_response(received_msg)
                return msg.body
            return None
        except (json.JSONDecodeError, KeyError):
            logger.error('System recieve error')
            raise SystemError

    def get_response(self, operation: str) -> int | float | str:
        request_data = Request(uuid=str(uuid1()), request=operation)
        logger.debug(f'Client GET: Sending request: \n{dict(request_data)}')
        self.client_request_queue.send_qmsg(dict(request_data))
        response = self._recv()
        return response

    def send_request(self, operation: str, payload: dict) -> None:
        request_data = Request(uuid=str(uuid1()), request=operation, body=payload)
        logger.debug(f'Client SEND: Sending request: {dict(request_data)}')
        self.client_request_queue.send_qmsg(dict(request_data))