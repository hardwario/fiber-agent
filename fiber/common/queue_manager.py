import threading
from queue import Empty, Queue

from loguru import logger


class QueueManager:
    def __init__(self, maxsize: int = 1000) -> None:
        self._q = Queue()
        self.maxsize = maxsize

    def recv_qmsg(self, stop_event: threading.Event, block: bool=True, timeout: float | int | None = None, empty_error: bool=False) -> dict | None:
        while not stop_event.is_set():
            if self._q.qsize() >= self.maxsize:
                logger.warning('Warning: the queue is almost full!')
            try:
                receive = self._q.get(block=block, timeout=timeout)
                return receive
            except Empty:
                if empty_error:
                    raise Empty
                continue
        return None

    def send_qmsg(self, obj: dict) -> None:
        self._q.put(obj)

    def qsize(self) -> int:
        return self._q.qsize()