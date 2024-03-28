import threading
import time

from loguru import logger


class AlreadyRegisteredError(Exception):
    pass


class ThreadNotFoundError(Exception):
    pass


class Pool:
    def __init__(self, timeout: int = 5) -> None:
        self._thread_event = {}
        self._timeout = timeout
        self._lock = threading.RLock()

    def manage_thread(self, register_request: bool, thread: threading.Thread | None, event: threading.Event | None) -> None:
        self._validate_thread_event(thread, event)

        if register_request:
            self._register(thread, event)
        else:
            self._unregister(thread)

    @staticmethod
    def _validate_thread_event(thread: threading.Thread | None, event: threading.Thread | None) -> None:
        if not isinstance(thread, threading.Thread) or not isinstance(event, threading.Event):
            raise TypeError(
                "Thread must be an instance of threading.Thread "
                "and event must be an instance of threading.Event"
            )

    def _register(self, thread: threading.Thread, event: threading.Event) -> None:
        with self._lock:
            if thread in self._thread_event:
                raise AlreadyRegisteredError

            self._thread_event[thread] = event
            logger.debug(f"Adding - {thread}: {event}")

    def _unregister(self, thread: threading.Thread) -> None:
        try:
            with self._lock:
                self._thread_event.pop(thread)
        except KeyError:
            pass

    def exit_all_thread(self) -> None:
        logger.info(f"Active theads: {len(self._thread_event)}")
        logger.info(f"Closing threads...")
        logger.debug(self._thread_event)
        for k, v in self._thread_event.items():
            logger.debug(self._thread_event[k])
        with self._lock:
            for event in self._thread_event.values():
                event.set()
            
            for thread in list(self._thread_event):
                logger.info(f"Deleting thread {thread.name}...")
                thread.join(self._timeout)
                if thread.is_alive():
                    logger.error(f"Thread {thread.name} did not exit in time")
                self._thread_event.pop(thread, None)
            logger.info(f"Active theads: {len(self._thread_event)}")
    
    def _check_thread_presence(self, thread_name: str, timeout: int = 30) -> bool:
        for _ in range(timeout):
            if any(thread_name in thread.name for thread in self._thread_event.keys()):
                return True
            time.sleep(1)
        logger.error(f"Time out for locating '{thread_name}' thread.")
        return False

pool = Pool()