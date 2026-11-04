# fiber-southbridge (Zephyr)

Zephyr port of the `fiber-southbridge` firmware (STM32C031C6).

## Layout

```
fiber-southbridge/
├── app/                          Zephyr application
│   ├── CMakeLists.txt
│   ├── prj.conf
│   └── src/                      main.c + modules
├── bridge/                       Bench-only UART<->RTT console firmware
├── boards/st/fiber_southbridge/  Custom board (STM32C031C6, HSI/4=12 MHz)
├── tests/                        native_sim ztest suites (twister)
├── tools/                        J-Link scripts, rtt.sh, flash_southbridge.py
└── west.yml                      West manifest (zephyr pin, shared with fiber-northbridge)
```

## Build

The west workspace is `firmware/` (see [`../README.md`](../README.md) for the
one-time setup). From `firmware/`:

```sh
west config manifest.path fiber-southbridge && west update
west build -b fiber_southbridge -d build-south -p auto fiber-southbridge/app
west flash -d build-south --runner jlink
```

The app's `CMakeLists.txt` prepends `../boards` to `BOARD_ROOT`, so the
`fiber_southbridge` board is discovered automatically.

Last verified build (Zephyr 4.3.99, SDK 1.0.0):

```
FLASH: 31104 B / 32 KB (94.92%)
RAM:    7376 B / 12 KB (60.03%)
```

## Diagnostic shell via J-Link RTT

A Zephyr shell runs on Segger RTT (zero UART pins consumed). The same J-Link
used for flashing carries the shell traffic.

The repo ships a convenience wrapper around [`rttt`][rttt] (HARDWARIO's RTT
terminal — `pip install rttt`):

[rttt]: https://pypi.org/project/rttt/

```sh
# After `west flash`:
./tools/rtt.sh
```

You should see:

```
south:~$
```

`rttt` talks to the J-Link directly (no need for a separate `JLinkGDBServer`)
and provides line editing + history (file under `~/.rttt_history`).

Alternative clients work too:

- `JLinkRTTClient` — requires `JLinkGDBServer` running in another terminal:
  ```sh
  JLinkGDBServer -device STM32C031C6 -if SWD -speed 4000 -nogui &
  stty -echo; JLinkRTTClient; stty echo
  ```
  The `stty -echo` trick is **not needed** here because the firmware sets
  `CONFIG_SHELL_ECHO_STATUS=n` — local terminal echo is the only echo.
- `JLinkRTTViewer` — the SEGGER GUI; auto-handles connection.

Commands (these mutate the same state as the USART1 protocol):

| Command                          | Effect                                         |
|----------------------------------|------------------------------------------------|
| `status`                         | Print firmware version + uptime + LED state    |
| `led <0-7> <O\|G\|R\|Y> <S\|L\|F>` | Composed line LED                              |
| `pwr <O\|G\|Y\|L> <S\|L\|F>`     | Composed power LED                             |
| `bright <0-100>`                 | Global brightness                              |
| `gpio <NAME> <on\|off\|toggle>`  | Drive any pin directly (P0, LED3G, PWRLEDG…)   |
| `help`                           | List all shell commands                        |

The shell and USART1 protocol are independent — running `led 3 G F` from
RTT and `LED 3 G F\n` from `/dev/ttyAMA*` both update the same internal
LED state.

## Protocol (UART USART1, 115200 8N1)

Identical to the legacy CubeIDE firmware. Lines terminated by `\n` (`\r`
ignored). Every command answers `OK\r\n` or `ERR: ...\r\n`.

| Command                         | Effect                                          |
|---------------------------------|-------------------------------------------------|
| `P0 ON\|OFF\|TOGGLE`             | Drive output P0 (idem P1..P7)                   |
| `LED3G ON\|OFF\|TOGGLE`          | Direct drive of a named LED pin                 |
| `LED <0-7> <O\|G\|R\|Y> <S\|L\|F>` | Composed line LED: color + steady/slow/fast     |
| `PWR <O\|G\|Y\|L> <S\|L\|F>`     | Composed power LED                              |
| `BRIGHT <0-100>`                | Global brightness for composed LEDs (debounce 50 ms) |
| `VIN READ` / `VBAT READ`        | ADC read, emits formatted `<NAME>_raw= ...`     |
| `SYNC`                          | Reset blink phase (multi-board sync)            |
| `HELP`                          | Print command reference (`HELP: ...`)           |
| `VERSION`                       | Firmware version string                         |
| `UPTIME`                        | Milliseconds since boot                         |
| `STATUS`                        | Dump brightness + LED/PWR colors and patterns   |

### Diagnostic command examples

```
> HELP
HELP: P<0-7> ON|OFF|TOGGLE       drive output pin
HELP: LED<i><G|R> ON|OFF|TOGGLE  drive a single LED pin directly
HELP: PWRLEDG|PWRLEDY ON|OFF|TOGGLE
HELP: LED <0-7> <O|G|R|Y> <S|L|F>  composed line LED
HELP: PWR <O|G|Y|L> <S|L|F>        composed power LED
HELP: BRIGHT <0-100>             global LED brightness
HELP: VIN READ | VBAT READ        ADC sample
HELP: SYNC                       reset blink phase
HELP: STATUS | VERSION | UPTIME  diagnostics

> STATUS
STATUS: brightness=100
STATUS: led0=O,S
STATUS: led1=O,S
...
STATUS: pwr=Y,S

> VERSION
VERSION: south-zephyr 0.1.0

> UPTIME
UPTIME: 18432ms
```

## Differences from the legacy firmware

- **Blink patterns now actually blink.** In the legacy firmware, `blink_tick`
  was declared but never advanced, making `L` and `F` patterns identical
  to `S` (steady). This port advances `blink_tick` at every 10 kHz PWM
  cadence and composes the on/off mask through `blink_on_now`.
- Dead bitmask state (`led_green_state`, `led_red_state`, `pwr_led_g`,
  `pwr_led_y`) was removed — the legacy code maintained it but never read
  from it after the redesign to `led_states[]`.
- Otherwise behavior-equivalent: same pinout, same clock (HSI/4 = 12 MHz),
  same UART speed, same command set, same response strings.

## `bridge/` — a serial console for the Hub's Linux (bench only)

A second, throwaway application that replaces the protocol firmware with a
transparent byte pipe between USART1 and RTT channel 0.

It exists because the Hub's carrier board does not break out the CM4's console
pins (GPIO14/15) and its USB OTG port is in host mode, so USART1 — the wire
this MCU already sits on — is the only way to reach the CM4. Flash this, point
the Hub's Linux console at `ttyAMA4`, and the J-Link on the SWD header becomes
the missing serial port: boot log in, login prompt out.

It **replaces** `app/`; it does not coexist with it. A Hub running the bridge
has no sensor-power pins, LEDs or ADC until `app/` is flashed back.

```sh
west build -b fiber_southbridge -p auto bridge -d build-bridge
west flash -d build-bridge --runner jlink

# The tools are not on PATH by default on a nix box. segger-jlink is unfree
# and pulls an insecure qt4, hence both flags:
export JLINK_BIN=$(NIXPKGS_ALLOW_UNFREE=1 nix build --impure --no-link \
  --print-out-paths --expr 'with import <nixpkgs> { config = {
     allowUnfree = true; segger-jlink.acceptLicense = true;
     permittedInsecurePackages = [ "segger-jlink-qt4-952" ]; }; };
   segger-jlink')/bin
nix-shell -p socat --run "bash bridge/console.sh"   # Ctrl-] to detach
```

`console.sh` holds the J-Link connection open, waits for its RTT telnet server
and attaches your terminal raw. Three things it does that are not obvious, each
of which otherwise looks like dead hardware:

- **It keeps stdin open.** `JLinkExe` exits at its `J-Link>` prompt on EOF,
  taking the RTT server with it.
- **It never probes the port by connecting.** The RTT server accepts exactly
  one client, so a readiness check that opens the socket consumes it and the
  real client gets `ERROR: Connection refused - There already is an active
  connection.`
- **It resets the MCU and jumps to the app entry first.** This board has
  `NBOOT_SEL=0` with BOOT0 on a CM4 GPIO, so a power-cycle can leave the STM32
  in the ST system bootloader; the symptom is `RTT control block not found` and
  a silent console. The vector is read from the built binary, so a rebuild
  cannot leave a stale address behind.

To restore the protocol firmware: `west build -b fiber_southbridge -p auto app
&& west flash --runner jlink`, or flash the released
`southbridge-1.0.0.bin` (shipped in meta-fiber's `wlprog` recipe) — the CM4
can also do it over `ttyAMA4` with `tools/flash_southbridge.py`, no probe
needed.

The Linux half of this — enabling `uart4` and putting the kernel console on
`ttyAMA4` — lives in the Hub image; see [`system/README.md`](../../system/README.md)
section "Getting a console on a Hub".

## Files of interest

- `boards/st/fiber_southbridge/fiber_southbridge.dts` — pinout
- `app/src/led_pwm.c` — soft-PWM + blink composer
- `app/src/cmd_dispatch.c` — protocol parser
- `bridge/src/main.c` — UART↔RTT byte pipe (bench console firmware)
- `docs/plans/zephyr-port-fiber-southbridge-plan.md` — design doc
