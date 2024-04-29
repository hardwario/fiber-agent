import os
import time
import netifaces
from loguru import logger


class NetworkInterfaceHandler:
    def __init__(self, interface: str) -> None:
        self._serial_number = 1  
        self._interface = interface

    def _get_mac(self) -> None:
        try:
            mac_address = self._wait_for_mac_network_interface()
        except (KeyError, IndexError) as e:
            logger.error(f"No MAC address found, restart the system")
            raise
        return mac_address
    
    def _get_ip(self) -> None: 
        try:
            ip_address = self._wait_for_ip_network_interface()
        except (KeyError, IndexError) as e:
            logger.error(f"No IP address found, restart the system")
            raise
        return ip_address
    
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

    def _wait_for_ip_network_interface(self) -> str:
        while True:
            try:
                addrs = netifaces.ifaddresses(self._interface)
                ip_address = addrs[netifaces.AF_INET][0]['addr']
                return ip_address
            except (KeyError, IndexError):
                logger.debug("Network interface not available yet. Retrying...")
                time.sleep(1)

    def _wait_for_mac_network_interface(self) -> str:
        while True:
            try:
                addrs = netifaces.ifaddresses(self._interface)
                mac_address = addrs[netifaces.AF_LINK][0]['addr']
                return mac_address
            except (KeyError, IndexError):
                logger.debug("Network interface not available yet. Retrying...")
                time.sleep(1)
