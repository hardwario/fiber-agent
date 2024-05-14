PATH_FIBER_FILE = '/var/fiber/'
PATH_W1_DEVICES = '/sys/bus/w1/devices/w1_bus_master'

POWER_LED = 0
PROBE_1 = 1
PROBE_2 = 2
PROBE_3 = 3
PROBE_4 = 4
PROBE_5 = 5
PROBE_6 = 6
PROBE_7 = 7
PROBE_8 = 8

VALID_PROBES = (
    POWER_LED,
    PROBE_1,
    PROBE_2,
    PROBE_3,
    PROBE_4,
    PROBE_5,
    PROBE_6,
    PROBE_7,
    PROBE_8,
)

PROBE_INDEX = {
    POWER_LED: [0, 1],
    PROBE_1: [2, 3],
    PROBE_2: [4, 5],
    PROBE_3: [6, 7],
    PROBE_4: [8, 9],
    PROBE_5: [10, 11],
    PROBE_6: [12, 13],
    PROBE_7: [14, 15],
    PROBE_8: [16, 17],
}
