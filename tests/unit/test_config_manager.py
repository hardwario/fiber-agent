import unittest
from unittest.mock import mock_open, patch, MagicMock
from fiber.common.config_manager import load_config_from_file, save_config, restart_fiber_core_service

class TestConfigFunctions(unittest.TestCase):
    @patch('builtins.open', new_callable=mock_open, read_data='key: value\n')
    def test_load_config_from_file(self, mock_file_open):
        config_path = 'test_config.yaml'
        config_model = MagicMock()
        config = load_config_from_file(config_path, config_model)
        mock_file_open.assert_called_once_with(config_path, 'r')
        config_model.assert_called_once_with(key='value')

    @patch('subprocess.run')
    def test_restart_fiber_core_service(self, mock_subprocess_run):
        restart_fiber_core_service()
        mock_subprocess_run.assert_called_once_with(['systemctl', 'restart', 'fiber-core.service'], check=True)

if __name__ == '__main__':
    unittest.main()