from io import StringIO
import queue
import unittest
from unittest.mock import MagicMock, patch
import threading
from queue import Empty, Queue
from fiber.common.queue_manager import QueueManager
from loguru import logger

class TestQueueManager(unittest.TestCase):
    def setUp(self):
        self.queue_manager = QueueManager(maxsize=3)

    def test_send_and_recv_message(self):
        message = {'key': 'value'}
        self.queue_manager.send_qmsg(message)
        received_message = self.queue_manager.recv_qmsg(threading.Event())
        self.assertEqual(received_message, message)

    def test_recv_timeout(self):
        with self.assertRaises(Empty):
            self.queue_manager.recv_qmsg(threading.Event(), timeout=0.1, empty_error=True)

    def test_recv_empty_error(self):
        empty_queue = queue.Queue(maxsize=100)
        empty_queue.get = MagicMock(side_effect=Empty)

        self.queue_manager._q = empty_queue

        with self.assertRaises(Empty):
            self.queue_manager.recv_qmsg(threading.Event(), empty_error=True)

    def test_queue_size(self):
        self.assertEqual(self.queue_manager.qsize(), 0)
        self.queue_manager.send_qmsg({'key': 'value'})
        self.assertEqual(self.queue_manager.qsize(), 1)

if __name__ == '__main__':
    unittest.main()