import os
import time
import netifaces
from loguru import logger


class NetworkInterfaceHandler:
    def __init__(self, interface: str) -> None:
        self._serial_number = 1
        self.interface_addresses = netifaces.ifaddresses(interface)

    def _get_mac(self) -> None:
        try:
            mac_address = self._wait_for_network_interface_address(netifaces.AF_LINK)
            return mac_address
        except (KeyError, IndexError) as e:
            logger.error(f'No MAC address found, restart the system')
            raise
    
    def _get_ip(self) -> None: 
        try:
            ip_address = self._wait_for_network_interface_address(netifaces.AF_INET)
            return ip_address
        except (KeyError, IndexError) as e:
            logger.error(f'No IP address found, restart the system')
            raise

    def _wait_for_network_interface_address(self, address_family: int) -> str:
        while True:
            try:
                address = self.interface_addresses[address_family][0]['addr']
                return address
            except (KeyError, IndexError):
                logger.debug('Network interface not available yet. Retrying...')
                time.sleep(1)
    
    def _get_uptime(self) -> None:
        with open('/proc/uptime', 'r') as f:
            uptime_seconds = float(f.readline().split()[0])
            return uptime_seconds
        
    def _get_fiber_id(self) -> None:
        fiber_id = self._serial_number
        return fiber_id
    
    def _reboot(self, body):
        if body != None:
            time.sleep(body['delay'])

        os.system('reboot')

    
