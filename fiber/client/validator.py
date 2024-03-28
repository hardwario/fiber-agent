from fiber.common.consts import (INDICATOR_GREEN, INDICATOR_RED, 
                                 INDICATOR_OFF_TAG, INDICATOR_ON_TAG,
                                 POWER_LED, PROBE_1, PROBE_2, PROBE_3,
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

    @staticmethod
    def validate_indicator(indicator: str, probe: int) -> None:
        if probe == POWER_LED:
            valid_indicators = {INDICATOR_ON_TAG, INDICATOR_OFF_TAG}
        else:
            valid_indicators = {
                INDICATOR_RED,
                INDICATOR_GREEN,
                INDICATOR_ON_TAG,
                INDICATOR_OFF_TAG,
            }
        if indicator not in valid_indicators:
            raise SystemError