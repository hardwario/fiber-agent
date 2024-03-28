from ctypes import (Structure, POINTER, c_int, c_uint, 
                    c_ushort, c_byte, c_ubyte, c_ulong)
import decimal


class SerialStructure(Structure):
    _fields_ = [("type", c_int),
                ("line", c_int),
                ("port", c_uint),
                ("irq", c_int),
                ("flags", c_int),
                ("xmit_fifo_size", c_int),
                ("custom_divisor", c_int),
                ("baud_base", c_int),
                ("close_delay", c_ushort),
                ("io_type", c_byte),
                ("reserved_char", c_byte * 1),
                ("hub6", c_uint),
                ("closing_wait", c_ushort),
                ("closing_wait2", c_ushort),
                ("iomem_base", POINTER(c_ubyte)),
                ("iomem_reg_shift", c_ushort),
                ("port_high", c_int),
                ("iomap_base", c_ulong)]


def convert_to_float(x: str, precision: int) -> float:
    context = decimal.Context(prec=precision)
    return float(decimal.Decimal(x, context)) if x else None

recv_start = (
    ("rssi", int),
    ("id", str),
    ("header", int),
    ("sequence", int),
    ("uptime", int)
)

recv_type_lut = {
    1: {'type': 'beacon',
        'items': (
            ("altitude", int),
            ("co2_conc", int),
            ("humidity", lambda x: convert_to_float(x, precision=1)),
            ("illuminance", int),
            ("motion_count", int),
            ("orientation", int),
            ("press_count", int),
            ("pressure", int),
            ("sound_level", int),
            ("temperature", lambda x: convert_to_float(x, precision=2)),
            ("voc_conc", int),
            ("voltage", lambda x: convert_to_float(x, precision=2))
        )},
    2: {'type': 'sound',
        'items': (
            ("min", int),
            ("max", int),
        )}
}

items_v1_0_x = (
    ("rssi", int),
    ("id", str),
    ("sequence", int),
    ("altitude", int),
    ("co2-conc", int),
    ("humidity", lambda x: decimal.Decimal(x, decimal.Context(prec=1))),
    ("illuminance", int),
    ("motion-count", int),
    ("orientation", int),
    ("press-count", int),
    ("pressure", int),
    ("sound-level", int),
    ("temperature", lambda x: decimal.Decimal(x, decimal.Context(prec=2))),
    ("voc-conc", int),
    ("voltage", lambda x: decimal.Decimal(x, decimal.Context(prec=2))),
)