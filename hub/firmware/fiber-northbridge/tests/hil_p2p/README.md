# Two-probe P2P HIL harness

Drives **this northbridge firmware over J-Link RTT** and plays the P2P
**central** against a real STICKER device under test — no Proximos
`control-radio`, no CM4, no PTY bridge. One Python process, two probes.

```
   ┌─ nb_hil.py (this host) ───────────────────────────────────┐
   │  P2P central logic (p2p_proto.py): join tag verify,        │
   │  JoinAccept, session-key KDF, CCM data plane, ACK          │
   └───────┬─────────────────────────────────┬─────────────────┘
      RTT (probe A)                      RTT (probe B, optional)
           │                                 │
   ┌───────▼────────┐   868 MHz LoRa   ┌──────▼──────────────┐
   │ northbridge    │ ◀──────────────▶ │ STICKER under test  │
   │ (bench build)  │   P2P over-air   │ radio-mode p2p      │
   │  = radio modem │                  │  = the real app_p2p │
   └────────────────┘                  └─────────────────────┘
```

The northbridge is a dumb, keyless modem: it only tunes the radio, hands up
received frames (`EVT_RX`) and fires scheduled transmissions (`TX_SCHEDULE`).
**All** P2P protocol logic — the part a real deployment's central service runs —
lives in [`p2p_proto.py`](p2p_proto.py). The central schedules its JoinAccept/ACK
at an **absolute modem-clock timestamp** (`uplink_end + rx1_delay`), so the
northbridge fires the downlink precisely inside the node's RX1 window; the host
only has to deliver the schedule in time, not hit the window itself.

## Files

| File | What |
|---|---|
| `p2p_proto.py` | Pure logic: HDLC + host↔modem protocol + P2P crypto/frames. No hardware. |
| `test_p2p_proto.py` | Offline pytest — verifies `p2p_proto` against sticker-firmware's own known-answer vectors. **Run this first; no hardware.** |
| `nb_hil.py` | RTT transport + the join→telemetry→ACK procedure over two probes. |
| `nb_rtt_bridge.py` | RTT↔PTY byte pipe, for driving this modem from **`control-radio`** instead of from the Python central. No protocol knowledge. |
| `test_nb_rtt_bridge.py` | Offline pytest for the bridge's framing. No hardware. |

### Two ways to drive the modem, and why both exist

`nb_hil.py` plays the central *here*, in Python. `nb_rtt_bridge.py` plays nothing — it hands
`control-radio` a serial device and shuttles bytes. Use the bridge whenever the thing under
test is the **Rust** central; use `nb_hil.py` when you want a P2P exchange with no Proximos
checkout at all.

Keep it that way. `p2p_proto.py` is a second implementation of the central living in the
modem's own repo — legitimate as bench tooling, and offline-verified against the firmware's
KATs, but it must never grow behaviour the Rust central lacks, or the bench stops testing the
thing that ships. The bridge is deliberately incapable of that: it has no keys and no protocol.

### The bridge

```bash
pip install pylink-square
python3 -m pytest test_nb_rtt_bridge.py -v          # framing, no hardware

python3 nb_rtt_bridge.py --nb-sn <SN> \
    --elf ../../../build-nb-rtt/zephyr/zephyr.elf \
    --link /tmp/p2p-modem --verbose
```

It prints the `_SEGGER_RTT` address it read and the `PROXIMOS_P2P_UART` value to use. Then,
in another shell:

```bash
PROXIMOS_P2P_UART=/tmp/p2p-modem control-radio-p2p-host --no-mqtt 2> p2p-host.log
```

Expect `modem GET_INFO ok: …` → `radio armed: …` → `P2P central running`. `no RSP for command
0x01 within 800ms` means the bridge, the probe or the RTT down-buffer is wrong — not the
central. Notes:

- **One RTT client per probe.** Stop `rttt` and `nb_hil.py` first.
- The bridge closes the PTY slave as soon as it has its name: `control-radio` opens the device
  exclusively (tokio-serial), and a slave fd still held here makes that open fail.
- `--verbose` logs every frame and its inject latency. Watch it against the central's 800 ms
  RSP timeout — that budget is the first thing a slow probe eats into.
- `--raw-log PATH` appends everything the modem writes on RTT ch.0 to a file: `P2P_OUT` lines,
  shell echoes, log output and — the reason it exists — a fatal-handler register dump. The bridge
  is the only RTT client while it runs, so without this a crash shows up at the central as a
  link timeout and nothing else. Keep the file outside `/tmp`; the 2026-09-09 bus fault's only
  record was lost to a `/tmp` clean.

## 1. Offline check (no hardware, run in CI)

Proves the harness computes byte-identical tags/keys/CRC to the firmware it
drives (session-key, JoinRequest/JoinAccept CMAC from sticker-firmware
`tests/ccm`; CRC-16/CCITT-FALSE from `tests/hdlc_framing`). If this fails, fix it
before touching the bench.

```bash
pip install pycryptodome pylink-square
cd fiber-northbridge/tests/hil_p2p
python3 -m pytest test_p2p_proto.py -v
```

## 2. Flash the two devices

**Probe A — northbridge, bench build** (RTT transport, `-DBENCH_RTT_BRIDGE=ON`;
this is what routes the host↔modem protocol over RTT — see `docs/firmware.md`):

```bash
west build -p always -b stm32wl5moc fiber-northbridge/app -- -DBENCH_RTT_BRIDGE=ON
west flash --runner jlink --dev-id <NB_SN>
# the 256 B down-buffer (auto-applied) moves _SEGGER_RTT — read it from the .elf:
arm-zephyr-eabi-nm build/zephyr/zephyr.elf | grep _SEGGER_RTT
```

**Probe B — STICKER under test:** flash the main app, set `radio-mode p2p` and
provision its LoRaWAN AppKey (that AppKey is the P2P root secret — pass the same
value to `nb_hil.py --app-key`). See sticker-firmware `doc/p2p.md`.

> ⚠️ **Always pass the J-Link serial (`--dev-id` / `--nb-sn` / `--dut-sn`).**
> Without it a multi-probe bench pops a GUI selector that hangs headless runs and
> can flash the wrong device. Bench probe SNs change day to day — reverify.

## 3. Run the procedure

```bash
python3 nb_hil.py \
    --nb-sn <NB_SN> --rtt-addr 0x<from the .elf above> \
    --dut-sn <DUT_SN> \
    --app-key <32 hex digits> --serial 0x<dut serial> \
    --unix-time $(date +%s)      # optional: exercise the B5 clock-sync ACK tail
```

`--dut-sn` is optional. With it, the runner triggers a fresh join
(`ats radio unjoin`, reboots into the 120 s boot-window join) and reads back
`ats radio status`. Without it, reset/observe the DUT by hand.

### What each step verifies

| Step | Proves |
|---|---|
| JoinRequest seen + CMAC verified | DUT→NB→central uplink path; join tag interop (matches `parse_join_request`) |
| JoinAccept scheduled, `EVT_TX_DONE OK` | central→NB→DUT downlink hits RX1; DUT accepts the JoinAccept CMAC and pairs |
| Data-plane uplink decrypted | session-key KDF interop + CCM data plane (DIR_TX) |
| ACK scheduled, `EVT_TX_DONE OK` | B1 RSSI/SNR in the ACK; with `--unix-time`, B5 clock-sync tail |
| `ats radio status` = PAIRED | end-to-end confirmation on the DUT itself |

A clean run ends `=== RESULT: PASS`. This is the self-contained, Proximos-free
equivalent of the live HIL recorded in proximos-v2 MR !30 (which used the Rust
`control-radio` central over a PTY bridge instead of this Python central).
