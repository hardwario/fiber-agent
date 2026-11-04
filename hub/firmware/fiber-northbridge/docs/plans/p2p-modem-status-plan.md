# P2P Northbridge Modem — Status & Roadmap Plan

Living status of the northbridge P2P **gateway modem** against the cross-repo P2P
design and the host-side implementation. Where [`firmware.md`](../firmware.md)
(MR !3) describes *current behaviour* and [`problems.md`](../problems.md) (MR !3)
logs *open bring-up issues*, this plan maps **what is now HW-verified** and **what
is left**, with particular attention to the P2P-hardening items (S1–S4) surfaced by
the host-side cross-check, so the modem side has a target to grow into.

Authoritative wire spec: proximos-v2 `plan/system/lora/design.md` §3–4. P2P
protocol/central design: proximos-v2 `plan/control/radio/p2p.md` and
`p2p_control_runtime.md`. Host implementation: proximos-v2 **MR !30**
(`control-radio` P2P central runtime).

- **FW version:** 0.1.0 · **Protocol version:** 1 (`P2P_PROTO_VERSION`) · **Board:** STM32WL5MOC

---

## 1. Where the modem sits (unchanged, load-bearing)

The northbridge is a **dumb, keyless, stateless** radio front-end: it brings up
the Sub-GHz radio and speaks the binary host↔modem protocol
(`GET_INFO`/`SET_RADIO_CONFIG`/`RX_START`/`RX_STOP`/`TX_SCHEDULE`/`TX_CANCEL` +
`EVT_BOOT`/`EVT_RX`/`EVT_TX_DONE`/`EVT_LOG`). **The radio frame is an opaque byte
string** — all crypto, addressing and protocol logic live host-side. This property
is what makes most of the hardening below cost the modem nothing (§4).

---

## 2. Bring-up status — now HW-verified

The P2P protocol engine has **run end-to-end on real hardware**, which supersedes
[`problems.md`](../problems.md) §4's "never built or run on real hardware yet".

- **Two-probe P2P HIL** (proximos-v2 MR !30, 2026-08-27/28): this northbridge
  (MR !3) driving a real **STICKER** (`sticker-firmware` PR #404) with a host-side
  `control-radio` central over an RTT↔PTY bridge — full
  `create → node-add → join → telemetry/alarm decrypt → ACK → reboot persistence`,
  **no retransmits**, **RSSI −56…−58 dBm** at bench range.
- The single-radio TX/RX turnaround (`radio_ctl.c` releasing async RX before each
  scheduled TX and re-arming after) is therefore **now exercised against a live
  over-the-air join/ACK round trip**, not only the throwaway bench responder —
  downgrading [`problems.md`](../problems.md) §4's last bullet from "not yet
  exercised".

**Un-draft gate (MR !3):** the acceptance re-run is host-side (through the shipped
`control-radio` runtime, MR !30); nothing on the modem is known-blocking today.

---

## 3. Bench harness (reference)

- **`-DBENCH_RTT_BRIDGE=ON`** carries the HDLC frames over the RTT shell channel and
  auto-pulls `app/bench_rtt_bridge.conf` (`CONFIG_SEGGER_RTT_BUFFER_SIZE_DOWN=256`).
  The 256 B down-buffer is **load-bearing**: at the 16 B default a host
  `p2p_inject` chunks too slowly and the scheduled JoinAccept TX misses its aim
  window, so the join never completes. (Details in [`firmware.md`](../firmware.md).)
- `tests/hil_p2p/` — the two-probe harness; crypto checked offline against
  sticker-firmware's known-answer vectors first.

---

## 4. S1–S4 hardening — modem impact

The host/device P2P hardening set (proximos-v2 `p2p_control_runtime.md` §4;
`sticker-firmware` PR #408 items B1/B4/B5/B6). Because the modem forwards an opaque
frame, **S1–S3 need no modem change** — only S4 does.

| Item | What it is | Modem impact | Status |
|---|---|---|---|
| **S1** — RSSI/SNR in ACK | Host puts the uplink's link-quality into the (extended) ACK body | **None.** `EVT_RX` already carries `rssi(2) \| snr(1)`; the host reads it and builds the ACK. | ✅ proven live (RSSI −56 dBm / SNR 9 dB, MR !30 HIL) |
| **S2** — `0x56` downlink queue | Variable-length command frame delivered in RX1 | **None.** `TX_SCHEDULE` takes an opaque byte string of any length. | ◑ partly proven — `0x56` radiated (`EVT_TX_DONE OK`); the node's `0x55` round-trip not confirmed on the bench run |
| **S3** — Unix time in ACK | Optional 4-byte time tail → 7-byte extended ACK | **None.** Opaque frame; longer body passes through unchanged. | ✅ proven live (`--deliver-time`, DUT RTC set, MR !30 HIL) |
| **S4** — per-device channel/SF | JoinAccept `reserved(4)` = `channel_idx \| sf \| tx_power \| flags` assigns per-node radio | **Future modem work.** `SET_RADIO_CONFIG` is **global** (one config); per-device assignment needs per-TX/per-node reconfig or fast channel switching. | ⛔ not started — all-zero today (single shared channel); **blocked on a regulatory decision** (AU915 dwell / US915 FCC; PR #408 recommends BW500 + a cert-lab review) |

**Takeaway:** the extended ACK (S1) + time tail (S3) + `0x56` (S2) are **transparent
to the modem** — the wire-version-free "self-describing by frame length" ACK
(control-radio MR !30, coordinated with `sticker-firmware` PR #408) rides through
`TX_SCHEDULE` as-is. The only modem-side roadmap item is **S4**.

> ⚠️ **Deploy coordination.** The extended ACK ships as a coordinated pair —
> `control-radio` MR !30 + `sticker-firmware` PR #408 — never against a node that
> still runs only PR #404 (1-byte ACK). The modem is agnostic (opaque frame), so it
> imposes no ordering of its own, but bench rigs must flash the matching heads.

---

## 5. Open modem gaps (from `problems.md` §4, re-scored after HIL)

Still to confirm on the bench, none currently blocking the HIL round trip:

- **`TX_SETUP_LEAD_MS` (15 ms, `radio_ctl.c`)** — the TX-reconfigure lead before a
  scheduled `t_ms`, picked without HIL data. The join succeeds today, but this and
  the RX-window width still want a real sweep (distinct from the central's
  scheduling margin in `system/lora/design.md` §4).
- **`EVT_RX.snr_q_db` precision** — the wire field is "quarter-dB"; the generic
  Zephyr `lora_recv_cb` only exposes whole-dB `int8_t`, so `radio_ctl.c` converts
  ×4. Since the host now uses SNR as a real link-quality field (S1 ACK + portal
  `gateways`), confirm ×4-quantized precision is acceptable for phase 1, or read the
  SX126x SNR register directly.
- **`EVT_TX_DONE.t_actual_ms` precision** — captured after the blocking
  `lora_send()` returns, not at the TxDone IRQ (generic LoRa API gives no hook).
- **`GET_INFO` `rx_count`/`tx_count` semantics** — LoRa packet counts vs. HDLC
  `crc_err`; confirm the reading matches host expectations before relying on it.

---

## 6. Roadmap

1. **Un-draft MR !3** — after the host-side acceptance re-run (MR !30) closes its
   own gate; no known modem blocker.
2. **Resolve the §5 gaps** on the bench (lead-time/RX-window sweep; SNR precision
   call with the host implementer).
3. **Reconcile weak-signal (`problems.md` §1)** — board bring-up measured
   −106…−110 dBm at 20 cm, but the P2P HIL bench read −56…−58 dBm; confirm whether
   the RX-boost patch closed it or the two setups are not comparable, before
   freezing radio-init.
4. **S4 (per-device channel/SF)** — design the `SET_RADIO_CONFIG` extension and the
   JoinAccept `reserved(4)` handling **after** the regulatory decision that decides
   what a channel/SF policy is even allowed to assign.
