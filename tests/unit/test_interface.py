import json
import unittest
from unittest.mock import Mock, patch
import threading
from queue import Queue

from pydantic import ValidationError
from fiber.client.manager import InterfaceManager
from fiber.client.handler import InterfaceHandler
from fiber.models.system import FiberIdBody, RebootBody


class TestInterfaceHandler(unittest.TestCase):
    def setUp(self):
        self.core_stop_event = threading.Event()
        self.system_response_queue = Mock()
        self.interface_request_queue = Mock()
        self.handler = InterfaceHandler(self.core_stop_event, self.system_response_queue, self.interface_request_queue)
        
    @patch('fiber.client.manager.InterfaceManager.send_request')
    def test_set_indicator_state(self, mock_send_request):
        self.handler.set_indicator_state(probe=1, state=True)
        expected_payload = {'output': 1, 'state': True}
        mock_send_request.assert_called_with(operation='set_indicator_state', payload=expected_payload)

    @patch('fiber.client.manager.InterfaceManager.send_request')
    def test_set_indicator_state_invalid(self, mock_send_request):
        with self.assertRaises(ValidationError):
            self.handler.set_indicator_state(probe=9, state=True)
    
    @patch('fiber.client.manager.InterfaceManager.send_request')
    def test_update_sensor_display(self, mock_send_request):
        self.handler.update_sensor_display(probe=2, temperature=36.5)
        expected_payload = {'output': 2, 'temperature': 36.5}
        mock_send_request.assert_called_with(operation='update_sensor_display', payload=expected_payload)

    @patch('fiber.client.manager.InterfaceManager.send_request')
    def test_update_sensor_display(self, mock_send_request):
        with self.assertRaises(ValidationError):
            self.handler.update_sensor_display(probe=9, temperature=36.5)
    
    @patch('fiber.client.manager.InterfaceManager.send_request')
    def test_set_fiber_id(self, mock_send_request):
        fiber_id = 2158000000 
        self.handler.set_fiber_id(fiber_id=fiber_id)
        expected_payload = {'id': fiber_id}
        mock_send_request.assert_called_with(operation='set_id', payload=expected_payload)

    @patch('fiber.client.manager.InterfaceManager.send_request')
    def test_set_fiber_id_invalid(self, mock_send_request):
        fiber_id = 123 
        with self.assertRaises(ValidationError):
            self.handler.set_fiber_id(fiber_id=fiber_id)
    
    @patch('fiber.client.manager.InterfaceManager.send_request')
    def test_reboot(self, mock_send_request):
        self.handler.reboot(delay=10)
        expected_payload = {'delay': 10}
        mock_send_request.assert_called_with(operation='reboot', payload=expected_payload)


class TestInterfaceManager(unittest.TestCase):
    def setUp(self):
        self.core_stop_event = threading.Event()
        self.system_response_queue = Mock()
        self.interface_request_queue = Mock()
        self.manager = InterfaceManager(self.core_stop_event, self.system_response_queue, self.interface_request_queue)
    
    def test_check_response_valid(self):
        received_msg = {'uuid': '1234', 'response': 'OK', 'error': False, 'body': 'test'}
        response = self.manager.check_response(received_msg)
        self.assertEqual(response.body, 'test')
    
    def test_check_response_invalid(self):
        received_msg = {'error': True, 'body': 'test'}
        with self.assertRaises(SystemError):
            self.manager.check_response(received_msg)

    def test_check_response_error(self):
        received_msg = {'uuid': '1234', 'response': 'Error', 'error': True, 'body': 'test'}
        with self.assertRaises(SystemError):
            self.manager.check_response(received_msg)
    
    @patch('fiber.client.manager.InterfaceManager._recv')
    def test_get_response(self, mock_recv):
        mock_recv.return_value = 'response'
        response = self.manager.get_response('get_test')
        self.assertEqual(response, 'response')
        self.interface_request_queue.send_qmsg.assert_called()
        args, kwargs = self.interface_request_queue.send_qmsg.call_args
        self.assertIn('uuid', args[0])
        self.assertEqual(args[0]['request'], 'get_test')
    
    def test_recv_valid(self):
        received_msg = {'uuid': '1234', 'response': 'OK', 'error': False, 'body': 'test'}
        self.system_response_queue.recv_qmsg.return_value = received_msg
        response = self.manager._recv()
        self.assertEqual(response, 'test')

    def test_recv_invalid(self):
        self.system_response_queue.recv_qmsg.return_value = None
        response = self.manager._recv()
        self.assertIsNone(response)

    def test_recv_error(self):
        self.system_response_queue.recv_qmsg.side_effect = json.JSONDecodeError('msg', 'doc', 0)
        with self.assertRaises(SystemError):
            self.manager._recv()
    
    def test_send_request(self):
        operation = 'test_operation'
        payload = {'key': 'value'}
        
        self.manager.send_request(operation, payload)
        self.interface_request_queue.send_qmsg.assert_called()
        args, kwargs = self.interface_request_queue.send_qmsg.call_args
        sent_request_data = args[0]

        self.assertIn('uuid', sent_request_data)
        self.assertEqual(sent_request_data['request'], operation)
        self.assertEqual(sent_request_data['body'], payload)

if __name__ == '__main__':
    unittest.main()
