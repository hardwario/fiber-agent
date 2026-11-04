# fiber-agent

The FIBER Hub runtime for stock Raspberry Pi OS Lite 64-bit (Bookworm):
the FIBER Hub software (portal, agent, LoRaWAN, BLE, STM32 firmware), installed with one command.


Layout: `bootstrap.sh` (the one-command installer), `bin/` and `lib/` (the `fiber` CLI),
`tools/` (build, stage, release), `stm32/` (UART flashing), `hub/` (the source of the
portal, the agent and the STM32 firmware).

Developer loop: `make check` (shellcheck + bats).

Bring up a Hub: [docs/REPLICATE.md](docs/REPLICATE.md).
