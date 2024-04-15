from uuid import uuid1
import threading
from pydantic import BaseModel, ValidationError
import json
from loguru import logger

from fiber.common.queue_manager import QueueManager


class ServerResponse(BaseModel):
    uuid: str = None  
    response: str = None
    error: bool
    body: dict | int | float | str = None


class ClientManager:
    def __init__(
        self,
        server_response_queue: QueueManager,
        client_request_queue: QueueManager,
        message_for_server_queue: QueueManager,
        stop_event: threading.Event,
    ) -> None:
        self.server_response_queue = server_response_queue
        self.client_request_queue = client_request_queue
        self.message_for_server_queue = message_for_server_queue
        self._stop_event = stop_event

    def check_response(self, resp: dict) -> None:
        try:
            ServerResponse(**resp)
        except ValidationError as e:
            raise SystemError(e)

        if resp["error"] is True:
            logger.error("Server response error")
            raise SystemError

    def _recv(self) -> int | float | str:
        try:
            logger.debug("Client RECV: Trying to get message from server")
            recv = self.server_response_queue.recv_qmsg(self._stop_event)
            if recv is not None:
                self.check_response(recv)
                return recv["body"]
            return None
        except (json.JSONDecodeError, KeyError):
            logger.error("System recieve error")
            raise SystemError

    def get_response(self, request_type: str) -> int | float | str:
        request_data = {"uuid": str(uuid1()), "request": request_type}
        logger.debug(f"Client GET: Sending request: \n{request_data}")
        self.client_request_queue.send_qmsg(request_data)
        response = self._recv()
        return response

    def send_request(self, request_type: str, body: dict) -> None:
        request_data = {
            "uuid": str(uuid1()),
            "request": request_type,
            "body": body,
        }
        logger.debug(f"Client SEND: Sending request: {request_data}")
        self.message_for_server_queue.send_qmsg(request_data)
