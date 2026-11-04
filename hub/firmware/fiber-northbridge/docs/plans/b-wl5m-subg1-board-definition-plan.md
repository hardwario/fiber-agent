# STM32WL5MOC Zephyr Board Definition Plan

## 1. Overview

Custom Zephyr board definition for a bare **STM32WL5MOCH6TR** module with antenna, connected to a **Raspberry Pi CM4** via UART (ttyAMA3). Goal: **LoRa radio functionality**.

- Console UART: **USART1 on PA9 (TX) / PA10 (RX)** → RPi CM4 ttyAMA3
- Flashing: **J-Link** via SWD
- Reference board: `nucleo_wl55jc` (same SoC, different board)

## 2. Hardware

### STM32WL5MOC Module
- **SoC**: STM32WL55JC (Cortex-M4, 48 MHz, 256 KB flash, 64 KB SRAM)
- **HSE**: 32 MHz crystal integrated in module
- **LSE**: 32.768 kHz crystal integrated in module
- **RF switch**: Integrated; FE_CTRL1 (PC3), FE_CTRL2 (PC4), FE_CTRL3 (PC5) routed internally
- **RF matching**: Integrated, single antenna port
- **PA config**: Low power only — VDDPA wired to VDDPA, VR_PA wired to BIAS_LP (RFO_LP, max +15 dBm)
- **Package**: LGA92, 10x10 mm

### Connections
| Signal | STM32WL5MOC Pin | Connected To |
|---|---|---|
| UART TX | PA9 (USART1_TX) | RPi CM4 ttyAMA3 RX |
| UART RX | PA10 (USART1_RX) | RPi CM4 ttyAMA3 TX |
| SWD CLK | SWCLK | J-Link |
| SWD IO | SWDIO | J-Link |
| Antenna | RF port | External antenna |

### Key Differences from nucleo_wl55jc

| Aspect | nucleo_wl55jc | stm32wl5moc (ours) |
|---|---|---|
| HSE/LSE | External crystals on board | Internal to module |
| RF switch | External, board-level | Internal to module |
| Console | LPUART1 (PA2/PA3) | USART1 (PA9/PA10) |
| LEDs/buttons | 3 LEDs, 3 buttons | None |
| Sensors/flash | None | None |
| Debug | ST-Link on-board | External J-Link |

## 3. File Structure

```
south-zephyr/
├── boards/
│   └── st/
│       └── stm32wl5moc/
│           ├── board.yml                    # Board metadata
│           ├── board.cmake                  # J-Link flash/debug config
│           ├── Kconfig.stm32wl5moc          # Board Kconfig
│           ├── stm32wl5moc_defconfig        # Default kernel config
│           ├── stm32wl5moc.dts              # Device tree
│           └── stm32wl5moc-pinctrl.dtsi     # Pin control
├── app/
│   ├── CMakeLists.txt
│   ├── prj.conf
│   └── src/
│       └── main.c                           # LoRa test app
└── docs/
    └── plans/
        └── b-wl5m-subg1-board-definition-plan.md
```

`BOARD_ROOT` set to project root so Zephyr finds the custom board.

## 4. Implementation Phases

### Phase 1: Minimal Boot (Day 1) — COMPLETED 2026-03-23

1. ~~Create board directory with all board files~~
2. ~~Minimal device tree: clock config + USART1 console only~~
3. ~~Build `hello_world`~~
4. ~~Flash via J-Link, verify serial output on RPi~~

**Result**: HSI→PLL→48MHz clock config works. USART1 PA9/PA10 console verified.

### Phase 2: Sub-GHz Radio / LoRa (Day 2) — COMPLETED 2026-03-23

1. ~~Enable `subghzspi` and `radio@0` in device tree~~
2. ~~Configure RF switch~~ — Required patching sx126x.c driver for 3-GPIO control
3. ~~Build LoRa send/recv samples~~
4. ~~Test TX/RX between two boards~~

**Key finding**: RF switch truth table differs from generic ST BSP. Correct table (from fiber-northbridge): OFF=000, RX=111, TX_LP=001 (PC5 only). Required driver patch for board-level RF switch callback.

*Amended 2026-09-26:* the TX_LP entry above is wrong. ST's HAL for STM32WL5Mxx
(`HAL_RADIO_SetSwitchConfig`) gives PC3/PC4/PC5 = OFF 000, RX 111, **TX_LP 110**,
TX_HP 101. TX_LP=001 left the PA unconnected (~26 dB TX deficit on the P2P bench);
see `docs/problems.md` #2.

**Result**: Two-board LoRa TX/RX working. RSSI weak (-106 dBm at 20cm) — RX boost patch applied, not yet verified.

### Phase 3: LoRa Test App (Day 3) — COMPLETED 2026-03-23

1. ~~LoRa shell app with `lora send` / `lora recv` / `lora config`~~
2. ~~Configurable frequency, SF, BW, TX power~~
3. ~~Controllable from RPi over UART console~~
4. ~~Added `rf_diag` command for GPIO debugging~~

**Result**: Interactive shell working from RPi via `scripts/shell.py`.

### Phase 4: Radio Tuning — IN PROGRESS

1. RX boosted gain (0x08AC=0x97) — patch applied, not verified
2. SMPS drive level management — TODO
3. DC-DC + LDO regulator mode — TODO
4. Image calibration for 868-870 MHz — TODO
5. PA duty cycle tuning per power level — TODO

## 5. Board File Contents

### board.yml
```yaml
board:
  name: stm32wl5moc
  full_name: STM32WL5MOC Module
  vendor: st
  socs:
    - name: stm32wl55xx
```

### Kconfig.stm32wl5moc
```kconfig
config BOARD_STM32WL5MOC
    select SOC_STM32WL55XX
```

### stm32wl5moc_defconfig
```kconfig
CONFIG_SERIAL=y
CONFIG_GPIO=y
CONFIG_CONSOLE=y
CONFIG_UART_CONSOLE=y
CONFIG_ARM_MPU=y
CONFIG_HW_STACK_PROTECTION=y
CONFIG_CLOCK_STM32_HSE_CLOCK=32000000
CONFIG_CLOCK_STM32_SYSCLK_SRC_PLL=y
CONFIG_CLOCK_STM32_PLL_SRC_HSE=y
```

### stm32wl5moc.dts (key nodes)
```dts
/ {
    model = "STM32WL5MOC Module";
    compatible = "st,stm32wl5moc";

    chosen {
        zephyr,console = &usart1;
        zephyr,shell-uart = &usart1;
        zephyr,sram = &sram0;
        zephyr,flash = &flash0;
    };

    aliases {
        lora0 = &lora;
    };
};

&clk_hse { status = "okay"; clock-frequency = <DT_FREQ_M(32)>; };
&clk_lse { status = "okay"; };

&pll {
    div-m = <2>; mul-n = <6>; div-r = <2>; div-q = <2>;
    clocks = <&clk_hse>;
    status = "okay";
};

&rcc { clocks = <&pll>; clock-frequency = <DT_FREQ_M(48)>; };

&usart1 {
    pinctrl-0 = <&usart1_tx_pa9 &usart1_rx_pa10>;
    pinctrl-names = "default";
    current-speed = <115200>;
    status = "okay";
};

&subghzspi {
    status = "okay";
    lora: radio@0 {
        status = "okay";
        power-amplifier-output = "rfo-lp";
        rfo-lp-max-power = <15>;
    };
};

&rng { status = "okay"; };
&rtc {
    clocks = <&rcc STM32_CLOCK_BUS_APB1 0x00000400>,
             <&rcc STM32_SRC_LSE RTC_SEL(1)>;
    status = "okay";
};
```

### board.cmake (J-Link)
```cmake
board_runner_args(jlink "--device=STM32WL55JC" "--speed=4000")
include(${ZEPHYR_BASE}/boards/common/jlink.board.cmake)
```

## 6. Risks

| Level | Risk | Mitigation |
|---|---|---|
| HIGH | RF switch internal to module — Zephyr driver tested with Nucleo's external switch | Test with SDR; check if FE_CTRL pins need explicit GPIO config |
| MEDIUM | HSE oscillator vs bypass mode | Ensure DT does NOT set `hse-bypass`; fallback to HSI |
| LOW | USART1 PA9/PA10 pinctrl — verify these exist in upstream STM32WL pinctrl DTSI | Check `stm32wl55jcix-pinctrl.dtsi` |

## 7. Reference Files

| Purpose | Zephyr Path |
|---|---|
| Reference board | `boards/st/nucleo_wl55jc/` |
| SoC DTSI | `dts/arm/st/wl/stm32wl55Xc.dtsi` |
| Pinctrl | `dts/arm/st/wl/stm32wl55jcix-pinctrl.dtsi` |
| Radio binding | `dts/bindings/lora/st,stm32wl-subghz-radio.yaml` |
| LoRa samples | `samples/drivers/lora/send/` and `recv/` |

## 8. Build & Flash Commands

```bash
export BOARD_ROOT=$PWD/fiber-northbridge   # from firmware/

# Phase 1: Boot test
west build -b stm32wl5moc -s ${ZEPHYR_BASE}/samples/hello_world
west flash --runner jlink

# Phase 2: LoRa test
west build -b stm32wl5moc -s ${ZEPHYR_BASE}/samples/drivers/lora/send
west flash --runner jlink

# Monitor from RPi
minicom -D /dev/ttyAMA3 -b 115200
```

## 9. Success Criteria

| Phase | Criterion |
|---|---|
| Phase 1 | "Hello World" appears on RPi via ttyAMA3 at 115200 baud |
| Phase 2 | LoRa TX produces RF output; RX receives packets |
| Phase 3 | Interactive LoRa shell controllable from RPi terminal |
