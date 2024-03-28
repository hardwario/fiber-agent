from fiber.common.consts import (PROBE_1, PROBE_2, PROBE_3, PROBE_4, 
                                 PROBE_5, PROBE_6, PROBE_7, PROBE_8)

LED_REPEAT_CYCLE_MS = 0.5
GPIO_LED_ENABLE = 14
GPIO_POWER_LED = 21
CHIP_NAME = "/dev/gpiochip0"

POWER_LED_ADDR = 0x21
PROBE_LED_ADDR_1 = 0x14
PROBE_LED_ADDR_2 = 0x17

INDICATOR_ON_ADDR = 0x4F
INDICATOR_OFF_ADDR = 0x00

EEPROM_MEM_ADDR_HSN_A = 0x00
EEPROM_MEM_ADDR_HSN_B = 0x04
EEPROM_MEM_ADDR_HSN_C = 0x08

PROBE_INDICATOR_CONFIG = {
    PROBE_1: {
        "green": {
            "register": 0x0D,
            "address": PROBE_LED_ADDR_1,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
        "red": {
            "register": 0x0E,
            "address": PROBE_LED_ADDR_1,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
    },
    PROBE_2: {
        "green": {
            "register": 0x0F,
            "address": PROBE_LED_ADDR_1,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
        "red": {
            "register": 0x10,
            "address": PROBE_LED_ADDR_1,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
    },
    PROBE_3: {
        "green": {
            "register": 0x11,
            "address": PROBE_LED_ADDR_1,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
        "red": {
            "register": 0x12,
            "address": PROBE_LED_ADDR_1,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
    },
    PROBE_4: {
        "green": {
            "register": 0x13,
            "address": PROBE_LED_ADDR_1,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
        "red": {
            "register": 0x0B,
            "address": PROBE_LED_ADDR_2,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
    },
    PROBE_5: {
        "green": {
            "register": 0x0C,
            "address": PROBE_LED_ADDR_2,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
        "red": {
            "register": 0x0D,
            "address": PROBE_LED_ADDR_2,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
    },
    PROBE_6: {
        "green": {
            "register": 0x0E,
            "address": PROBE_LED_ADDR_2,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
        "red": {
            "register": 0x0F,
            "address": PROBE_LED_ADDR_2,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
    },
    PROBE_7: {
        "green": {
            "register": 0x10,
            "address": PROBE_LED_ADDR_2,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
        "red": {
            "register": 0x11,
            "address": PROBE_LED_ADDR_2,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
    },
    PROBE_8: {
        "green": {
            "register": 0x12,
            "address": PROBE_LED_ADDR_2,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
        "red": {
            "register": 0x13,
            "address": PROBE_LED_ADDR_2,
            "value": INDICATOR_ON_ADDR,
            "disabled": False,
        },
    },
}


