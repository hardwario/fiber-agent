from fiber.hal.consts import (EEPROM_MEM_ADDR_HSN_A, EEPROM_MEM_ADDR_HSN_B, EEPROM_MEM_ADDR_HSN_C)
from fiber.hal.devices import eeprom as mem
from fiber.hal.devices.eeprom import EEPROM


A = 0
B = 1
C = 2


class SerialNumberReadError(Exception):
    pass


class SerialNumber:
    def __init__(self, eeprom: EEPROM) -> None:
        if not isinstance(eeprom, mem.EEPROM):
            raise TypeError

        self._eeprom = eeprom

        try:
             a, b, c = (int.from_bytes(self._eeprom.read(EEPROM_MEM_ADDR_HSN_A, 4), byteorder="little"),
                        int.from_bytes(self._eeprom.read(EEPROM_MEM_ADDR_HSN_B, 4), byteorder="little"),
                        int.from_bytes(self._eeprom.read(EEPROM_MEM_ADDR_HSN_C, 4), byteorder="little"))
        except OSError as e:
            raise SerialNumberReadError

        result = (~a & b & c) | (a & ~b & c) | (a & b & ~c) | (a & b & c)

        if result != a:
            self._eeprom.write(EEPROM_MEM_ADDR_HSN_A, result.to_bytes(4, "little"))
        if result != b:
            self._eeprom.write(EEPROM_MEM_ADDR_HSN_B, result.to_bytes(4, "little"))
        if result != c:
            self._eeprom.write(EEPROM_MEM_ADDR_HSN_C, result.to_bytes(4, "little"))


        if result == 4294967295:
            raise SerialNumberReadError("result == 4294967295")

        self._id = result

    @property
    def id(self) -> int:
        return self._id

    @id.setter
    def id(self, hsn: int) -> None:
        hsn_bytes = hsn.to_bytes(4, "little")
        self._eeprom.write(EEPROM_MEM_ADDR_HSN_A, hsn_bytes)
        self._eeprom.write(EEPROM_MEM_ADDR_HSN_B, hsn_bytes)
        self._eeprom.write(EEPROM_MEM_ADDR_HSN_C, hsn_bytes)

        self._id = hsn
