from fiber.common.consts import (POWER_LED, PROBE_1, PROBE_2, PROBE_3,
                                PROBE_4, PROBE_5, PROBE_6, PROBE_7, PROBE_8)


class ClientDataValidator:
    @staticmethod
    def validate_probe(probe: int) -> None:
        valid_probes = {
            POWER_LED,
            PROBE_1,
            PROBE_2,
            PROBE_3,
            PROBE_4,
            PROBE_5,
            PROBE_6,
            PROBE_7,
            PROBE_8,
        }
        if probe not in valid_probes:
            raise SystemError
