from common.fiber_common.consts import PROBE_1, PROBE_8, POWER_LED, ZEROMQ_ID_CLIENT, INDICATOR_GREEN, INDICATOR_RED
from fiber_common.device_manager import System

# import common.fiber_common as fiber_comm
import sys
import os

import smbus2
import fcntl
import time

class EEPROM:
    def __init__(self, i2c, dev_addr=0x56):
        self._i2c = i2c
        self._dev_addr = dev_addr

    def write(self, mem_addr, data):
        if not isinstance(mem_addr, int) or not isinstance(data, bytes):
            raise TypeError

        fcntl.lockf(self._i2c.fd, fcntl.LOCK_EX)
        time.sleep(0.01)

        write = smbus2.smbus2.i2c_msg.write(self._dev_addr,
                                            bytes([mem_addr]) + data)
        self._i2c.i2c_rdwr(write)
        fcntl.lockf(self._i2c.fd, fcntl.LOCK_UN)

def confirm():
    inp = input('Continue? If yes, press <ENTER>; if not, press <n> + <ENTER>')
    if inp == '':
        return True

    else:
        print('Test failed')
        sys.exit(1)

print(os.getuid())
if os.getuid() != 0:
        print('You must run process as root')
        sys.exit(1)

print('Stoping fiber-system')
os.system('service fiber-system stop')

print('Stoping fiber-setup')
os.system('service fiber-setup stop')

#print('Stoping fiber-client')
#os.system('service fiber-client stop')
#
#for x in range (1, 9):
#    print(f'Stoping fiber-sensor@{x}')
#    os.system(f'service fiber-sensor@{x} stop')
#
#print('Stoping fiber-tower')
#os.system('service fiber-tower stop')
#
#print('Stoping cpgw')
#os.system('service cpgw stop')

print('Init SMBUS')
eep = EEPROM(smbus2.smbus2.SMBus(10))

hsn_input = input('Scan HSN: ')
if hsn_input.strip() == '':
    print('No input provided. Exiting...')
    sys.exit(1)

hsn = int(hsn_input)

if not(2159017983 >= hsn >= 2157969408):
    print('Wrong HSN')

eep.write(0x00, hsn.to_bytes(4, 'little'))
eep.write(0x04, hsn.to_bytes(4, 'little'))
eep.write(0x08, hsn.to_bytes(4, 'little'))

print('Start fiber-system')
# os.system('service fiber-system start')

client = System(hostname='localhost', port=5555, client_id=ZEROMQ_ID_CLIENT)

print('All probe LEDs turns RED')
for probe in range(PROBE_1, PROBE_8+1):
    client.set_indicator(probe, INDICATOR_RED)

confirm()

print('All probe LEDs turns GREEN')
for probe in range(PROBE_1, PROBE_8+1):
    client.set_indicator(probe, INDICATOR_GREEN)

confirm()

print('Power LED is blinking')
client.set_indicator_blink(POWER_LED)

confirm()

print('Check all i2c devices')
os.system("i2cdetect -y 10 | diff /home/fiber/test/i2c.txt -")

for bus in range(1, 9):
    f = open(f'/sys/bus/w1/devices/w1_bus_master{bus}/w1_master_slave_count', 'r')
    if f.readline().strip() != '1':
        print(f'Wrong w1 device count on bus {bus}')
        sys.exit(1)

print('Test success')
