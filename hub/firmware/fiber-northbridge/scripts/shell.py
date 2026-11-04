#!/usr/bin/env python3
"""Interactive serial shell for STM32WL5MOC Zephyr console."""

import sys
import os
import select
import termios
import tty
import serial

PORT = os.environ.get("SERIAL_PORT", "/dev/ttyAMA3")
BAUD = int(os.environ.get("SERIAL_BAUD", "115200"))


def main():
    ser = serial.Serial(PORT, BAUD, timeout=0)
    old_settings = termios.tcgetattr(sys.stdin)

    print(f"Connected to {PORT} @ {BAUD}. Press Ctrl-] to exit.")

    try:
        tty.setraw(sys.stdin.fileno())

        while True:
            readable, _, _ = select.select([ser, sys.stdin], [], [], 0.05)

            for source in readable:
                if source is ser:
                    data = ser.read(1024)
                    if data:
                        os.write(sys.stdout.fileno(), data)

                elif source is sys.stdin:
                    ch = os.read(sys.stdin.fileno(), 1)
                    if ch == b"\x1d":  # Ctrl-]
                        return
                    ser.write(ch)

    except KeyboardInterrupt:
        pass
    finally:
        termios.tcsetattr(sys.stdin, termios.TCSADRAIN, old_settings)
        ser.close()
        print("\nDisconnected.")


if __name__ == "__main__":
    main()
