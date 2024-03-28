import threading
from queue import Empty
from fiber.common.queue_manager import QueueManager
from loguru import logger
from pydantic import BaseModel, ValidationError


class ServerStopEventError(Exception):
    pass

class ServerError(Exception):
    pass

class ClientRequest(BaseModel):
    uuid: str
    request: str
    body: dict = None


class ServerManager:
    def __init__(self, server_response_queue: QueueManager, client_request_queue: QueueManager, message_for_server_queue: QueueManager) -> None:
        self.server_response_queue = server_response_queue
        self.client_request_queue = client_request_queue
        self.message_for_server_queue = message_for_server_queue
        self._identity = None

    def recv(self, stop_event: threading.Event) -> tuple[str, str, dict | None]:
        while not stop_event.is_set():
            for queue in [self.client_request_queue, self.message_for_server_queue]:
                try:
                    msg = queue.recv_qmsg(stop_event, timeout=0.1, empty_error=True)
                    if not msg:
                        raise ServerStopEventError
                    else:
                        try:
                            ClientRequest(**msg)
                        except ValidationError as e:
                            raise ServerError(e)
                    
                    if "body" not in msg:
                        msg["body"] = None
                    return msg["uuid"], msg["request"], msg["body"]
                except Empty:
                    pass
        raise ServerStopEventError

    def send_err(self, request: str, uuid: str) -> None:
        logger.info(f'Server: Sending error: ("uuid": {uuid}, "response": {request}, "error": False, "body": None)')
        self.server_response_queue.send_qmsg({"uuid": uuid, "response": request, "error": True, "body": None})

    def send_msg(self, request: str, uuid: str, msg, error: bool) -> None:
        logger.debug(f"Server: Sending message: {request}")
        self.server_response_queue.send_qmsg({"uuid": uuid, "response": request, "error": error, "body": msg})
