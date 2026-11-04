#!/usr/bin/env python3
"""Mock MQTT publisher for the ProXimos v2 portal dev loop.

Simulates one FIBER Hub (8 wired 1-Wire probes, 2 LoRaWAN Node/STICKER
devices) publishing exactly the topics the v2 portal's M1 milestone
consumes: status, system/info, sensors/aggregated, config/state,
alarms/events, power/standby, export/probe_1m/{line}. Payload shapes match
the v1 protocol byte-for-byte (see plan/v1/viewer/mqtt.md) — that's the
wire contract, not incidental detail.

M4-E extension: the mock is also the E2E test bed for the signed-command
flow. It owns a deterministic device-CA Ed25519 identity (--seed), answers
pairing on proximos/{host}/pair/request (fixed --pairing-code, default ABC123,
or the code-free local path when the request sets "local": true),
and runs the full challenge/confirm loop on commands/config/request +
commands/config/confirm with real signature verification — an independent
cross-check of the signer's canonicalization (7-field request / 5-field
confirm pre-image, sorted-key compact JSON, per mqtt.md). A verification
failure is logged loudly with the expected message bytes: treat it as a
canonicalization FINDING, not noise.

M5-D extension: Node (STICKER) + Beacon (EYE) behavior. Publishes
lorawan/sensors aggregate telemetry (--sticker-interval-s, default 20 s)
and eye/sensors snapshots (--eye-interval-s, default 15 s); serves every Node
command (get_sticker_config/info/full_config/history, sticker_force_send) from
the ONE signed path — P3 deleted the flat-dialect commands/* topics, as P4 did
on the device; handles the signed set_sticker_config + Beacon commands via
CommandSim; keeps a per-
Node history ring buffer (--history-depth-s, default 3600) replayed as
…/history frames, with out_of_range refusals on the …/command topic.

B3 extension: a mock-only control topic, `{prefix}/{host}/mock/control`
(driven by dev/node.sh), silences or resumes ONE Node's uplinks and picks
how it answers get_sticker_history — ok / silent / history_unavailable.
That is what makes devices.backend.md §6's Accept: clauses reachable in the
dev loop: per-Node offline→online, cadence retries to exhaustion with the
"cannot download history" alert, and a terminal refusal. This topic does
not exist on a real Hub.

Demo cluster: one process is one Hub and `--hub` picks which (see PROFILES
below). `server` is the unit the portal runs on and reproduces the
single-Hub fleet literally; `cold-store` and `truck` are client Hubs with
their own hostname, gateway, pairing code and faults. Three of them on one
broker is the cluster the portal has always modelled (`fiber/+/...`, one
DeviceState per hostname) and the dev loop never had: a Beacon heard by
several Hubs, P2P Nodes on the v2 radio family, a Hub with no concentrator.
`make multi-up` starts all three; `dev/hub.sh` drives one.

Usage:
    python mock_publisher.py --host localhost --port 1883
    python mock_publisher.py --host localhost --port 1883 --hub cold-store
    python mock_publisher.py --host localhost --port 1883 --pair-invite
"""

from __future__ import annotations

import sys

# Line-buffer stdout so prints (pairing code, SIGCHECK findings) surface
# immediately in `docker logs` instead of sitting in a block buffer.
sys.stdout.reconfigure(line_buffering=True)

import argparse
import base64
import hashlib
import json
import math
import re
import os
import random
import secrets
import signal
import threading
import time
import uuid
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

# `--selftest` compares strings and verifies signatures against the checked-in
# vectors; it talks to no broker, so a missing paho must not stop it. Everything
# else does need a broker, and `main` says so before it tries.
try:
    import paho.mqtt.client as mqtt
except ImportError:
    mqtt = None

try:
    from cryptography.exceptions import InvalidSignature
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import (
        Ed25519PrivateKey,
        Ed25519PublicKey,
    )
    from cryptography.hazmat.primitives.asymmetric.x25519 import (
        X25519PrivateKey,
        X25519PublicKey,
    )
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    from cryptography.hazmat.primitives.kdf.hkdf import HKDF
    from cryptography.hazmat.primitives.kdf.pbkdf2 import PBKDF2HMAC
except ImportError:
    print("cryptography not installed. Run: pip install cryptography")
    sys.exit(1)


# ============================================================================
# Constants
# ============================================================================

TOPIC_PREFIX = "proximos"     # plan/portal/mqtt/design.md §9: one root, both halves

DEFAULT_AGGREGATION_S = 2
DEFAULT_SYSTEM_INFO_S = 30
DEFAULT_PROBE_1M_S = 60
DEFAULT_STICKER_INTERVAL_S = 20
DEFAULT_EYE_INTERVAL_S = 10   # advertisement cadence (devices.beacon.md §1); NOT the store interval
DEFAULT_HISTORY_DEPTH_S = 3600
DEFAULT_RADIO_GATEWAYS_S = 60   # proximos-control's health tick (plan/portal/devices.hub.md §12 A2)

# Fixed topic, no prefix and no hostname level — one cluster-wide report per broker
# (control_radio::northbound::GATEWAYS_TOPIC).
RADIO_GATEWAYS_TOPIC = "radio/gateways"
RADIO_API_STATUSES = ("ok", "auth_failed", "unreachable")
# Root of the v2 radio northbound family (control_radio::northbound::ROOT). Owned by
# the producer, which is why it hangs off no configurable prefix the way `fiber/` does.
RADIO_ROOT = "proximos"

CONFIG_PAGE_SIZE = 6            # device-config keys per config page (forces 2 pages)
HISTORY_RECORDS_PER_FRAME = 20  # per mqtt.md §history: records ≤20 per frame
DISCOVERED_TAG_DELAY_S = 30

DEFAULT_CRITICAL_LOW = -40.0
DEFAULT_WARNING_LOW = 0.0
DEFAULT_WARNING_HIGH = 30.0
DEFAULT_CRITICAL_HIGH = 50.0



# cluster-client-add's sealed password (plan/control/cluster.md §7, amended
# 2026-10-02): the agent never puts the password on config/response in
# clear. Third implementation of fiber-protocol/vectors/sealed_reply.json
# (which this very code generated), beside fiber_protocol::sealed_reply and
# control-core's cluster_clients::seal.
SEAL_LABEL = b"proximos/cluster-client-add/password-seal/v1"


def _b64url(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def _unb64url(text: str) -> bytes:
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


def seal_password(password: str, reply_key: str, request_id: str, hostname: str,
                  agent_secret: Optional[bytes] = None, nonce: Optional[bytes] = None) -> dict:
    """X25519(ephemeral, reply_key) -> HKDF-SHA256(salt = epk || reply_key,
    info = label 0 request_id 0 hostname) -> AES-256-GCM, no AAD.
    `{"epk", "nonce", "ct"}`, base64url without padding."""
    rpk = _unb64url(reply_key)
    if len(rpk) != 32:
        raise ValueError("reply_key is not an X25519 public key")
    esk = (X25519PrivateKey.from_private_bytes(agent_secret) if agent_secret
           else X25519PrivateKey.generate())
    epk = esk.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    shared = esk.exchange(X25519PublicKey.from_public_bytes(rpk))  # raises on an all-zero result
    info = SEAL_LABEL + b"\x00" + request_id.encode() + b"\x00" + hostname.encode()
    key = HKDF(algorithm=hashes.SHA256(), length=32, salt=epk + rpk, info=info).derive(shared)
    nonce = nonce or secrets.token_bytes(12)
    ct = AESGCM(key).encrypt(nonce, password.encode(), None)
    return {"epk": _b64url(epk), "nonce": _b64url(nonce), "ct": _b64url(ct)}


# ============================================================================
# Hub profiles — the demo cluster
# ============================================================================
#
# One process is one Hub. `local_hostname` in dev/config.yaml names the
# server, the unit the portal runs on, which pairs without a code; every
# other hostname on the broker is a client Hub that pairs remotely with its
# own code. The portal needed no change for this — it subscribes `fiber/+/…`
# and keys one DeviceState per hostname — so the whole cluster is a matter of
# starting this file more than once.
#
# Identity is derived from `index` so two instances can never collide on a
# DevEUI, a MAC, a serial or a gateway EUI. Index 0 reproduces the
# single-Hub fleet byte for byte: dev/node.sh, dev/beacon.sh, dev/alarm.sh
# and e2e_client.py all address it by hard-coded identifier.


@dataclass(frozen=True)
class HubProfile:
    """One Hub's fixture: how big its fleet is and which faults it carries.

    Every field here exists because it puts the portal in a state the other
    two profiles do not — a missing concentrator, a battery under the alert
    threshold, a Beacon that two Hubs hear. Tuning knobs that change nothing
    on screen belong on the command line, not in this table.
    """
    key: str
    index: int
    label: str
    #: `system/info.lorawan.cluster.role`. Untyped on the wire and stored
    #: verbatim by the portal, so it is a plausible blob, not a contract.
    cluster_role: str
    #: Wired 1-Wire probes this Hub has reported on. The portal pre-creates
    #: all 8 lines regardless (models.rs), so fewer than 8 is the ordinary
    #: shape of a partly-wired unit, not a degraded one.
    probes: int
    #: Probes parked above `critical_high` instead of walking there by luck:
    #: a Hub that is alarming right now is what /hubs?oor=alarming and the
    #: dashboard's alarming tile need in order to link to anything.
    alarming_probes: int = 0
    #: LoRaWAN Nodes with the plain three-field profile.
    plain_stickers: int = 0
    #: One more Node reporting the WHOLE continuous+counter registry.
    rich_sticker: bool = False
    #: Nodes on the P2P radio, published on the v2 northbound family. The
    #: only place the transport badge (M8) can read anything but "lorawan".
    p2p_nodes: int = 0
    #: Registered Beacons: (name, location, battery_mv, rssi) each.
    eye_tags: tuple = ()
    #: A discovered-only stranger appears on the air 30 s after startup.
    discovered_tag: bool = False
    #: Beacons enrolled on ANOTHER Hub that this one also hears, as
    #: (mac, rssi). They ride this Hub's eye/sensors as strangers and never
    #: its config/state — which is exactly what "overheard" is on the wire —
    #: and they are the input `render::beacon_hubs` ranks. A single-Hub loop
    #: cannot produce one.
    also_hears: tuple = ()
    #: False = no concentrator attached. The portal's UI-4 "waiting / no
    #: radio" state, a degraded-green fact about the unit, never an error.
    gateway_present: bool = True
    #: `system/info.lorawan.mode`, the rendered `radio.mode`. One transport
    #: per Hub, matching the fleet above: a Hub without a concentrator is
    #: `off`, since `lorawan` there is the concentrator-missing fault.
    radio_mode: str = "lorawan"
    ethernet: bool = True
    wifi: bool = False
    dc_connected: bool = True
    battery_percentage: int = 87
    #: Kept alongside the percentage rather than derived: a Li-ion curve is
    #: not linear, and a mock that reports 14 % at 3850 mV contradicts itself
    #: on the same card.
    battery_voltage_mv: int = 3850
    battery_status: str = "normal"
    storage_used_percent: int = 20
    #: radio/gateways is cluster-wide and retained on ONE topic. Two
    #: publishers do not merge, they overwrite, so the server owns the whole
    #: table and the clients publish nothing.
    publishes_gateways: bool = False
    pairing_code: str = "ABC123"
    seed: str = "mock-ca-seed"
    #: A real Hub hostname (decision #17, `proximos-<8 hex>`) instead of the
    #: `fiber-mock-NNN` fixture name. Every Client has one: the portal's Add
    #: hub refuses any other shape, and a real Client is registered there
    #: before it can reach the broker at all (plan/portal/devices.hub.md
    #: §12 M, C5; `make demo-seed` registers cold-store and truck).
    hostname_override: Optional[str] = None

    @property
    def hostname(self) -> str:
        if self.hostname_override:
            return self.hostname_override
        # Lowercase, the one spelling decision #21 gives a hostname: the portal
        # publishes commands under it, and MQTT topics are case-sensitive.
        return f"fiber-mock-{self.index + 1:03d}"

    @property
    def gateway_eui(self) -> str:
        """This Hub's own concentrator: the same EUI in system/info.lorawan
        and in its radio/gateways row, which is how the portal tells the
        local gateway from another Hub's (render::radio_card_ctx)."""
        return f"70b3d54900aa{self.index + 1:04x}"

    @property
    def gateway_id(self) -> str:
        """The gateway_id an uplink is credited to — a ChirpStack-side name,
        not the EUI."""
        return f"mock-gw-{self.index}"

    def node_eui(self, slot: int) -> str:
        """16 hex (EUI-64), lowercase per mqtt.md convention."""
        return f"70b3d5{self.index:02x}0000{slot:04x}"

    def serial(self, slot: int) -> str:
        return f"SN-{self.index:03d}-{slot:03d}"

    @property
    def cpu_serial(self) -> str:
        """The Hub's own CPU serial, `system/info.hardware.serial`: 16 hex
        like a CM4's `/proc/device-tree/serial-number`."""
        return f"10000000aa{self.index + 1:06x}"

    def eye_mac(self, slot: int) -> str:
        return f"EE:E0:00:00:{self.index:02X}:{slot:02X}"

    @property
    def ip(self) -> str:
        return f"192.168.1.{50 + self.index}"


#: The Beacon the whole cluster can hear. Enrolled on the server, overheard
#: by both clients — the one MAC that makes the Beacon detail page's "heard
#: by" list a list.
SHARED_EYE_MAC = "EE:E0:00:00:00:01"

PROFILES: dict[str, HubProfile] = {
    # The unit the portal runs on. Its fleet is unchanged from the
    # single-Hub loop, down to the identifiers.
    "server": HubProfile(
        key="server", index=0, label="Mock Device 1", cluster_role="server",
        probes=8, plain_stickers=2, rich_sticker=True,
        eye_tags=(
            ("Freezer", "Cold Room A", 2900, -70),
            ("Fridge", "Kitchen", 2650, -78),
            ("Loading dock", "Dock", 2400, -88),
        ),
        discovered_tag=True,
        publishes_gateways=True,
        # Demo: the server runs both transports (radio.mode dual).
        p2p_nodes=2, radio_mode="dual",
    ),
    # A healthy client Hub on WiFi, and the only place a P2P Node exists in
    # the dev loop. A P2P Hub and nothing else: `radio.mode` is one transport
    # per Hub (phase 1 does not build `dual`), so a Hub carrying LoRaWAN and
    # P2P Nodes at once would show the portal a fleet that cannot exist.
    "cold-store": HubProfile(
        key="cold-store", index=1, label="Cold Store North", cluster_role="client",
        probes=4, p2p_nodes=3, radio_mode="p2p",
        eye_tags=(("Chiller", "Cold Room B", 2880, -64),),
        also_hears=((SHARED_EYE_MAC, -84),),
        ethernet=False, wifi=True,
        pairing_code="DEF456", seed="mock-ca-seed-cold-store",
        hostname_override="proximos-c01d0002",
    ),
    # The degraded one: no concentrator, running off a battery under the
    # 20 % alert threshold, storage past the 90 % one, and a probe sitting in
    # CRITICAL. Every fault axis the portal has, on one unit.
    "truck": HubProfile(
        key="truck", index=2, label="Truck 12", cluster_role="client",
        probes=2, alarming_probes=1,
        eye_tags=(("Trailer", "Truck 12", 2380, -92),),
        also_hears=((SHARED_EYE_MAC, -95),),
        gateway_present=False, radio_mode="off", ethernet=False, wifi=True,
        dc_connected=False, battery_percentage=14, battery_voltage_mv=3480,
        battery_status="low",
        storage_used_percent=93,
        pairing_code="GHI789", seed="mock-ca-seed-truck",
        hostname_override="proximos-7a0c0003",
    ),
    # The Client Hub of milestone C5 (plan/control/cluster.md §8–§9): a LoRaWAN
    # gateway Hub that is NOT on the broker until it has "joined". Register it
    # with Add hub on the server's portal (hostname proximos-c5c1e000, gateway
    # EUI 70b3d54900aa0004), then start it (`make joiner-up`): its first
    # status is what turns the registry row from pending to complete. Not in
    # `--profile multi`, because a Client that publishes before it is
    # registered is the one thing a real join cannot produce.
    "joiner": HubProfile(
        key="joiner", index=3, label="Warehouse B gateway", cluster_role="client",
        probes=2, hostname_override="proximos-c5c1e000",
        ethernet=True, plain_stickers=2,
        pairing_code="JKM234", seed="mock-ca-seed-joiner",
    ),
}

#: `cluster-client-add` / `-remove` effects the server mock reports
#: (plan/control/cluster.md §7, C2's contract), so the portal's
#: "registration incomplete" path can be driven without a broken Hub. Env vars
#: for the same reason as MOCK_RADIO_API_STATUS: docker compose sets them
#: without a command rewrite.
CLUSTER_EFFECTS = ("ok", "failed")


# ============================================================================
# Dataclasses
# ============================================================================

@dataclass
class Sensor:
    line: int
    name: str
    location: Optional[str]
    enabled: bool = True
    has_override: bool = False
    critical_low: float = DEFAULT_CRITICAL_LOW
    low_alarm: float = DEFAULT_WARNING_LOW  # firmware backward-compat field
    warning_low: float = DEFAULT_WARNING_LOW
    warning_high: float = DEFAULT_WARNING_HIGH
    high_alarm: float = DEFAULT_WARNING_HIGH  # firmware backward-compat field
    critical_high: float = DEFAULT_CRITICAL_HIGH
    current_value: float = 22.0
    alarm_state: str = "NORMAL"  # NORMAL | WARNING | CRITICAL


@dataclass
class FieldThreshold:
    field: str
    critical_low: Optional[float] = None
    warning_low: Optional[float] = None
    warning_high: Optional[float] = None
    critical_high: Optional[float] = None


def default_sticker_device_config() -> dict:
    """STICKER settable device-config keys (STICKER_SETTABLE-ish subset).
    This dict is both the config store and the validation set for signed
    set_sticker_config — unknown keys are rejected with an ERROR response."""
    return {
        "application.interval_sample": 30,
        "application.interval_report": 300,
        "application.history_enable": True,
        "sensors.cap_temperature": True,
        "sensors.cap_humidity": True,
        "sensors.cap_hall": True,
        "alarms.alarm_0_enable": True,
        "alarms.alarm_0_field": "temperature",
        "alarms.alarm_0_lo": -10.0,
        "alarms.alarm_0_hi": 12.0,
        # The packed 17-byte rule slots the agent accepts (fiber-protocol
        # fields/sticker_config.rs), so the portal's PI-A alarm unit is not
        # refused as an unknown key. All-zero = slot not present.
        **{f"alarms.alarm_{n}": "00" * 17 for n in range(16)},
    }


@dataclass(frozen=True)
class NodeReport:
    """What a Node says about itself on STICKER firmware v2.0.0 (!179,
    plan/control/radio/node-firmware-v2.0.0.md): the retained `device_info`
    (boot `Info`) and `config` (boot `ConfigDump`) documents of the v2 radio
    family, `proximos/{host}/nodes/{dev_eui}/{device_info,config}`.

    The GP pins are three enum caps: `cap_gpio_mode` none | inputs | pir |
    buzzer, and `cap_gpio_input_a/_b` none | digital | analog | pulse. A
    digital or pulse input counts (`input_*_count`), an analog one reports
    its voltage (`input_*_voltage`, fields 30/31); `pir` counts motion.

    `fw` below (2, 0, 0) is what the Portal refuses config writes to
    ("FW < 2.0.0"). `fault` adds the crash record (`fault_reason`, `_name`,
    `_pc`, `_lr`) of the first boot after a fatal error (!180).
    `null_fields` are reported as the firmware's NA sentinel, which the Hub
    decodes to null — a `no_data` event, the row shows a dash.
    """
    fw: tuple = (2, 0, 0)
    gpio_mode: str = "none"
    input_a: str = "none"
    input_b: str = "none"
    barometer: bool = False
    light_sensor: bool = False
    w1_slots: tuple = ("empty", "empty", "empty", "empty")
    fault: bool = False
    null_fields: tuple = ()

    @property
    def fw_text(self) -> str:
        return ".".join(str(n) for n in self.fw)


#: STICKER v2.0.0 reports per (profile, EUI slot); a Node not listed reports
#: the default NodeReport (2.0.0, GP pins off). One per GP-pin variant, one
#: old firmware, one crash record, and NA fields, spread over both carriers.
NODE_REPORTS: dict[tuple, NodeReport] = {
    # Sticker 2: firmware 1.5.0 — the Portal refuses config writes to it.
    ("server", 1): NodeReport(fw=(1, 5, 0)),
    # Sticker 3 — full: inputs, A pulse (input_a_count), B analog
    # (input_b_voltage), barometer, light, two Dallas 1-Wire probes.
    ("server", 0x10): NodeReport(gpio_mode="inputs", input_a="pulse", input_b="analog",
                                 barometer=True, light_sensor=True,
                                 w1_slots=("dallas", "dallas", "empty", "empty")),
    # P2P 1: PIR (motion_count), last boot was a crash, barometer fitted but
    # reporting NA (pressure/altitude null).
    ("cold-store", 0x20): NodeReport(gpio_mode="pir", barometer=True, fault=True,
                                     null_fields=("pressure", "altitude")),
    # P2P 2: inputs, A analog (voltage, now and then NA), B digital (count).
    ("cold-store", 0x21): NodeReport(gpio_mode="inputs", input_a="analog", input_b="digital"),
    # P2P 3: buzzer on the GP pins, a light sensor that always reads NA.
    ("cold-store", 0x22): NodeReport(gpio_mode="buzzer", light_sensor=True,
                                     null_fields=("illuminance",)),
}


@dataclass
class Sticker:
    """A LoRaWAN Node (STICKER): config/state subset + M5-D live telemetry,
    device-config store, and history ring buffer."""
    dev_eui: str
    name: str
    serial_number: Optional[str]
    location: Optional[str]
    # False = this Node stops uplinking entirely: no entry in
    # lorawan/sensors, so its `last_seen` freezes and the portal's per-Node
    # offline detector (devices.backend.md §5) trips for this one Node while
    # the rest of the fleet reports normally. Flip it with dev/node.sh.
    enabled: bool = True
    # get_sticker_history behaviour, for B3's two failure paths (§6.2/§6.3):
    #   "ok"                  — replay the ring buffer (the default)
    #   "silent"              — answer nothing at all, which is what drives
    #                           the cadence retries to exhaustion and the
    #                           "cannot download history" alert
    #   "history_unavailable" — a terminal refusal; the job fails at once
    #                           with device_refused and NO alert (a refusal
    #                           is an answer, exhaustion is silence)
    history_mode: str = "ok"
    field_thresholds: list[FieldThreshold] = field(default_factory=list)
    #: Report the WHOLE continuous+counter registry
    #: (crates/fiber-protocol/src/fields/lorawan.rs), not just the three
    #: fields a plain STICKER sends. One of these in the fleet is what makes
    #: the Node detail page show its external probes, machine probes and
    #: environmental group with real values instead of 22 dashes.
    #:
    #: Only `fields` and `counters` are extended, both plain name→number maps
    #: keyed by the registry. The eight `event`-kind fields stay out: their
    #: wire shape is not in mqtt.md (lorawan.rs: "absent from the agent's
    #: registry"), and this file is the executable spec for those payloads —
    #: inventing one here would be a lie with a long half-life.
    rich: bool = False
    # --- M5-D runtime state (not part of config/state) ---
    temperature: float = 5.0
    humidity: float = 55.0
    voltage: float = 3.0
    fcnt: int = 0
    hall_left_count: int = 0
    # Rich-profile runtime state, walked like temperature/humidity above.
    ext_temperature_1: float = 4.0
    ext_temperature_2: float = 6.5
    machine_probe_temperature_1: float = 21.0
    machine_probe_temperature_2: float = 23.5
    machine_probe_humidity_1: float = 48.0
    machine_probe_humidity_2: float = 52.0
    illuminance: float = 120.0
    pressure: float = 1013.25  # hPa, as the Node reports it
    altitude: float = 235.0
    motion_count: int = 0
    hall_right_count: int = 0
    input_a_count: int = 0
    input_b_count: int = 0
    seq: int = 0  # device-side command/ack sequence counter
    field_alarm_states: dict = field(default_factory=dict)
    device_config: dict = field(default_factory=default_sticker_device_config)
    history: list = field(default_factory=list)  # ring buffer of {time, fields, counters}
    report: NodeReport = field(default_factory=NodeReport)
    input_b_voltage: float = 4.2


@dataclass
class EyeTag:
    """A Beacon (EYE) tag. Registered tags appear in config/state.eye_tags;
    discovered-only tags appear only in eye/sensors with discovered=true."""
    mac: str
    name: Optional[str]
    location: Optional[str] = None
    discovered: bool = False
    #: mock-only (dev/beacon.sh eye_silence): False drops this tag out of
    #: eye/sensors entirely, which is how a BLE broadcast actually goes
    #: quiet — there is no per-tag "enabled" on the wire.
    enabled: bool = True
    #: B6. `stale` is on the wire and means "listed, but the radio has
    #: stopped hearing it"; `present` is mock-only and means "not in the
    #: snapshot at all". They are different things, and reconcile-by-absence
    #: turns on the difference: a stale tag is KEPT (disconnected), an absent
    #: discovered-only one is PRUNED. `enabled` above predates both and is
    #: kept as dev/beacon.sh's silence switch.
    stale: bool = False
    present: bool = True
    #: One of the wire's five (control-ble host.rs): pending | provisioning |
    #: provisioned | failed | read_only. `None` = the Hub said nothing, which
    #: is what a pre-B6 Hub does.
    provisioning: Optional[str] = None
    temperature_c: float = 5.0
    humidity_pct: float = 45.0
    battery_mv: int = 2900
    rssi: int = -70
    #: `moving` / `movement_count` are already keys of the eye/sensors wire
    #: shape (mqtt.md) and two of the EYE registry's four fields
    #: (crates/fiber-protocol/src/fields/eye.rs) — they were simply published
    #: as null, so nothing downstream ever had movement to show.
    moving: bool = False
    movement_count: int = 0
    field_thresholds: list[FieldThreshold] = field(default_factory=list)


@dataclass
class P2pNode:
    """A Node on the P2P radio, published on the v2 northbound family
    (`proximos/{host}/nodes/{dev_eui}/{info,reading}`) and nowhere else.

    It is deliberately absent from `config/state.lorawan_sensors`: that key
    is the LoRaWAN inventory, and listing a P2P Node there would be a lie on
    the wire. So it carries no name — the retained `info` document states the
    transport, the readings carry the serial, and the operator-facing alias
    lives in the portal's own DB (p2p_nodes.md §3), which is why the portal
    shows these rows by serial with a `p2p` badge.
    """
    dev_eui: str
    serial_number: str
    temperature: float = 5.0
    humidity: float = 55.0
    voltage: float = 3.0
    hall_left_count: int = 0
    enabled: bool = True
    #: The session the central assigned at the join. `None` until the first
    #: reading: a registered Node has an `info` document and no session,
    #: which is the "registered — waiting for join" row (devices.node.md §5).
    dev_addr: int | None = None
    joined_at: str | None = None
    report: NodeReport = field(default_factory=NodeReport)
    motion_count: int = 0
    input_a_voltage: float = 2.5
    input_b_count: int = 0
    pressure: float = 1013.25
    altitude: float = 235.0


@dataclass
class DeviceState:
    hostname: str
    label: str
    #: B6: the Hub dropped overheard strangers from the snapshot
    #: (`eye/sensors.discovered_capped`). Mock-only switch behind
    #: `dev/beacon.sh capped`; on a real Hub it is `discovered_max` or the
    #: payload ladder. Never published as true for an empty `tags` array.
    eye_discovered_capped: bool = False
    firmware_version: str = "v0.99.0-mock"
    sample_interval_ms: int = 1000
    aggregation_interval_ms: int = DEFAULT_AGGREGATION_S * 1000
    report_interval_ms: int = 60_000
    system_info_interval_s: int = DEFAULT_SYSTEM_INFO_S
    led_brightness: int = 75
    screen_brightness: int = 80
    screen_timeout_secs: int = 60
    buzzer_volume: int = 50
    eye_enabled: bool = True
    eye_auto_provision: bool = False
    eye_auto_discover: bool = False
    display_lines: list[dict] = field(default_factory=list)
    sensors: list[Sensor] = field(default_factory=list)
    stickers: list[Sticker] = field(default_factory=list)
    eye_tags: list[EyeTag] = field(default_factory=list)
    start_ts: float = field(default_factory=time.time)
    standby: bool = False
    powered_off: bool = False  # set by power_off command; SIGHUP powers back on
    battery_voltage_mv: int = 3850
    battery_percentage: int = 87
    battery_status: str = "normal"
    dc_connected: bool = True
    dc_voltage_mv: int = 12000
    wifi_connected: bool = False
    ethernet_connected: bool = True
    # --- demo cluster: what used to be a module constant, now per Hub -----
    index: int = 0
    gateway_eui: str = ""
    gateway_id: str = "mock-gw-0"
    gateway_present: bool = True
    radio_mode: str = "lorawan"
    cpu_serial: str = "10000000aa000001"
    ip: str = "192.168.1.50"
    cluster_role: str = "server"
    cluster_peers: list[str] = field(default_factory=list)
    storage_used_percent: int = 20
    discovered_mac: str = "EE:E0:00:00:00:99"
    p2p_nodes: list[P2pNode] = field(default_factory=list)
    #: mock-only (dev/hub.sh offline): the Hub stops publishing altogether
    #: after a retained `offline` status — what a unit losing its uplink
    #: looks like, without killing the container.
    offline_forced: bool = False
    #: Server-only (dev/hub.sh gateway-stale): hostnames whose row in the
    #: cluster-wide radio/gateways report is backdated past the portal's
    #: 300 s window, which is how a concentrator that stopped reporting
    #: renders.
    stale_gateway_hosts: set = field(default_factory=set)
    #: Every Hub whose gateway belongs in the cluster-wide report. Non-empty
    #: on the server alone — it is the one publisher of that topic.
    gateway_members: list = field(default_factory=list)
    radio_api_status: str = "ok"


def build_device(profile: HubProfile) -> DeviceState:
    """One Hub's whole simulated inventory, derived from its profile.

    Every identifier hangs off `profile.index`, so two processes on one
    broker cannot collide — and index 0 reproduces the single-Hub fleet
    literally, which is what dev/node.sh, dev/beacon.sh and e2e_client.py
    address by hard-coded identifier.
    """
    sensors = [
        Sensor(
            line=line,
            name=f"Probe {line + 1}",
            location=f"Zone {chr(ord('A') + line)}",
            current_value=22.0 + random.uniform(-2, 2),
        )
        for line in range(profile.probes)
    ]
    # Parked above critical_high. `alarm_state` stays NORMAL on purpose: the
    # first aggregated publish then fires a real NORMAL -> CRITICAL
    # transition, so the alarm has a row and a timestamp rather than
    # appearing as a state nobody watched happen.
    for s in sensors[: profile.alarming_probes]:
        s.current_value = DEFAULT_CRITICAL_HIGH + 5.0

    def cold_room_thresholds() -> list[FieldThreshold]:
        return [
            FieldThreshold(field="temperature", critical_low=-10.0, warning_low=0.0, warning_high=8.0, critical_high=12.0),
            FieldThreshold(field="humidity", critical_low=20.0, warning_low=30.0, warning_high=75.0, critical_high=90.0),
        ]

    stickers = [
        Sticker(
            dev_eui=profile.node_eui(i),
            name=f"Sticker {i + 1}",
            serial_number=profile.serial(i),
            location=f"Cold Room {chr(ord('A') + i)}",
            field_thresholds=cold_room_thresholds(),
            temperature=random.uniform(3.0, 7.0),
            humidity=random.uniform(45.0, 65.0),
            report=NODE_REPORTS.get((profile.key, i), NodeReport()),
        )
        for i in range(profile.plain_stickers)
    ]
    # One Node reporting the WHOLE registry, so the fleet exercises the
    # external probes, the machine probes, the environmental group and every
    # counter — the plain ones above leave 22 of the 25 registry rows dashed.
    # Slot 0x10 in the EUI, 010 in the serial: both are v1 literals, kept as
    # they were so the imported identifiers survive the move to profiles.
    if profile.rich_sticker:
        stickers.append(Sticker(
            dev_eui=profile.node_eui(0x10),
            name="Sticker 3 — full",
            serial_number=profile.serial(10),
            location="Machine Hall",
            rich=True,
            report=NODE_REPORTS.get((profile.key, 0x10), NodeReport()),
            field_thresholds=cold_room_thresholds() + [
                FieldThreshold(field="machine_probe_temperature_1", critical_low=10.0, warning_low=16.0, warning_high=32.0, critical_high=40.0),
                FieldThreshold(field="pressure", critical_low=990.0, warning_low=1000.0, warning_high=1025.0, critical_high=1035.0),
            ],
            temperature=random.uniform(3.0, 7.0),
            humidity=random.uniform(45.0, 65.0),
        ))

    p2p_nodes = [
        P2pNode(
            dev_eui=profile.node_eui(0x20 + i),
            serial_number=profile.serial(20 + i),
            temperature=random.uniform(3.0, 7.0),
            humidity=random.uniform(45.0, 65.0),
            report=NODE_REPORTS.get((profile.key, 0x20 + i), NodeReport()),
        )
        for i in range(profile.p2p_nodes)
    ]

    # Seeded as `provisioned`: the fleet's steady state, so the B6 pill is
    # absent until something changes it (dev/beacon.sh provisioning).
    eye_tags = [
        EyeTag(mac=profile.eye_mac(n + 1), name=name, location=location,
               provisioning="provisioned", battery_mv=battery_mv, rssi=rssi)
        for n, (name, location, battery_mv, rssi) in enumerate(profile.eye_tags)
    ]
    # Overheard, not enrolled: a stranger in eye/sensors and absent from
    # config/state, which is all a Hub can honestly say about a Beacon
    # somebody else registered.
    eye_tags += [
        EyeTag(mac=mac, name=None, discovered=True, rssi=rssi)
        for mac, rssi in profile.also_hears
    ]

    return DeviceState(
        hostname=profile.hostname,
        label=profile.label,
        sensors=sensors,
        stickers=stickers,
        eye_tags=eye_tags,
        p2p_nodes=p2p_nodes,
        index=profile.index,
        gateway_eui=profile.gateway_eui,
        gateway_id=profile.gateway_id,
        gateway_present=profile.gateway_present,
        radio_mode=profile.radio_mode,
        cpu_serial=profile.cpu_serial,
        ip=profile.ip,
        cluster_role=profile.cluster_role,
        cluster_peers=[p.hostname for p in PROFILES.values() if p.key != profile.key],
        storage_used_percent=profile.storage_used_percent,
        discovered_mac=profile.eye_mac(0x99),
        wifi_connected=profile.wifi,
        ethernet_connected=profile.ethernet,
        dc_connected=profile.dc_connected,
        # A disconnected input reads 0 mV, not the nominal 12 V it would
        # carry if it were plugged in.
        dc_voltage_mv=12000 if profile.dc_connected else 0,
        battery_percentage=profile.battery_percentage,
        battery_voltage_mv=profile.battery_voltage_mv,
        battery_status=profile.battery_status,
    )


# ============================================================================
# Publishers — payload shapes match plan/v1/viewer/mqtt.md verbatim
# ============================================================================

def _ts() -> str:
    return datetime.now(timezone.utc).isoformat()


def _ts_ago(seconds: int) -> str:
    return datetime.fromtimestamp(time.time() - seconds, timezone.utc).isoformat()


def publish_status(client, dev: DeviceState, prefix: str, online: bool):
    topic = f"{prefix}/{dev.hostname}/status"
    payload = {"status": "online" if online else "offline", "timestamp": _ts()}
    client.publish(topic, json.dumps(payload), qos=1, retain=True)


def _compute_alarm_state(s: Sensor) -> str:
    v = s.current_value
    if v <= s.critical_low or v >= s.critical_high:
        return "CRITICAL"
    if v <= s.warning_low or v >= s.warning_high:
        return "WARNING"
    return "NORMAL"


def publish_alarm_event(client, dev: DeviceState, prefix: str, s: Sensor, from_state: str, to_state: str):
    topic = f"{prefix}/{dev.hostname}/alarms/events"
    payload = {
        "timestamp": _ts(),
        "line": s.line,
        "name": s.name,
        "from_state": from_state,
        "to_state": to_state,
        "temperature_celsius": s.current_value,
        "event_type": "alarm_transition",
    }
    client.publish(topic, json.dumps(payload), qos=1, retain=False)


def publish_sensors_aggregated(client, dev: DeviceState, prefix: str):
    topic = f"{prefix}/{dev.hostname}/sensors/aggregated"
    period_end = int(time.time())
    period_start = period_end - (dev.aggregation_interval_ms // 1000)
    sensors_data = []
    for s in dev.sensors:
        s.current_value += random.uniform(-0.3, 0.3)
        new_state = _compute_alarm_state(s)
        if new_state != s.alarm_state:
            publish_alarm_event(client, dev, prefix, s, s.alarm_state, new_state)
            s.alarm_state = new_state
        sensors_data.append({
            "line": s.line,
            "name": s.name,
            "location": s.location,
            "sample_count": 2,
            "disconnected_count": 0,
            "temperature": {
                "min_celsius": s.current_value - 0.1,
                "max_celsius": s.current_value + 0.1,
                "avg_celsius": s.current_value,
            },
            "alarm_counts": {
                "normal": 2 if s.alarm_state == "NORMAL" else 0,
                "warning": 2 if s.alarm_state == "WARNING" else 0,
                "critical": 2 if s.alarm_state == "CRITICAL" else 0,
                "disconnected": 0,
                "reconnecting": 0,
            },
            "dominant_alarm_state": s.alarm_state,
            "alarm_triggered_at": None,
        })
    payload = {
        "timestamp": _ts(),
        "period_start_ts": period_start,
        "period_end_ts": period_end,
        "duration_sec": period_end - period_start,
        "sensors": sensors_data,
    }
    client.publish(topic, json.dumps(payload), qos=0, retain=False)


def publish_config_state(client, dev: DeviceState, prefix: str):
    topic = f"{prefix}/{dev.hostname}/config/state"
    sensors_data = [
        {
            "line": s.line, "name": s.name, "location": s.location,
            "enabled": s.enabled, "has_override": s.has_override,
            "thresholds": {
                "critical_low_celsius": s.critical_low,
                "low_alarm_celsius": s.low_alarm,
                "warning_low_celsius": s.warning_low,
                "warning_high_celsius": s.warning_high,
                "high_alarm_celsius": s.high_alarm,
                "critical_high_celsius": s.critical_high,
            },
        }
        for s in dev.sensors
    ]
    lorawan_data = [
        {
            "dev_eui": s.dev_eui, "name": s.name,
            "serial_number": s.serial_number, "location": s.location,
            "enabled": s.enabled,
            "field_thresholds": [asdict(t) for t in s.field_thresholds],
        }
        for s in dev.stickers
    ]
    payload = {
        "timestamp": _ts(),
        "led_brightness": dev.led_brightness,
        "screen_brightness": dev.screen_brightness,
        "screen_timeout_secs": dev.screen_timeout_secs,
        "buzzer_volume": dev.buzzer_volume,
        "eye": {
            "enabled": dev.eye_enabled,
            "auto_provision": dev.eye_auto_provision,
            "auto_discover": dev.eye_auto_discover,
        },
        "system_info_interval_s": dev.system_info_interval_s,
        "device_label": dev.label,
        "display_lines": dev.display_lines,
        "sensors": sensors_data,
        "lorawan_sensors": lorawan_data,
        "eye_tags": [
            {"mac": t.mac, "name": t.name, "location": t.location}
            for t in dev.eye_tags if not t.discovered
        ],
        "intervals": {
            "sample_interval_ms": dev.sample_interval_ms,
            "aggregation_interval_ms": dev.aggregation_interval_ms,
            "report_interval_ms": dev.report_interval_ms,
        },
    }
    client.publish(topic, json.dumps(payload), qos=1, retain=True)


def publish_system_info(client, dev: DeviceState, prefix: str):
    topic = f"{prefix}/{dev.hostname}/system/info"
    uptime_s = int(time.time() - dev.start_ts)
    d, rem = divmod(uptime_s, 86400)
    h, rem = divmod(rem, 3600)
    m, s = divmod(rem, 60)
    if d > 0:
        uptime_human = f"{d}d {h}h {m}m {s}s"
    elif h > 0:
        uptime_human = f"{h}h {m}m {s}s"
    elif m > 0:
        uptime_human = f"{m}m {s}s"
    else:
        uptime_human = f"{s}s"
    total_bytes = 4_000_000_000
    payload = {
        "timestamp": _ts(),
        "hostname": dev.hostname,
        "device_label": dev.label,
        "firmware_version": dev.firmware_version,
        "uptime_seconds": uptime_s,
        "uptime_human": uptime_human,
        "power": {
            "battery": {
                "voltage_mv": dev.battery_voltage_mv,
                "percentage": dev.battery_percentage,
                "status": dev.battery_status,
            },
            "dc": {"voltage_mv": dev.dc_voltage_mv, "connected": dev.dc_connected},
            "last_dc_loss": None,
        },
        "network": {
            "wifi": {"connected": dev.wifi_connected, "signal_dbm": -60 if dev.wifi_connected else 0, "ip": dev.ip if dev.wifi_connected else None},
            "ethernet": {"connected": dev.ethernet_connected, "ip": f"192.168.0.{30 + dev.index}" if dev.ethernet_connected else None},
            "has_internet": dev.wifi_connected or dev.ethernet_connected,
        },
        "storage": {
            "data_partition": {
                "total_bytes": total_bytes,
                "available_bytes": total_bytes - total_bytes * dev.storage_used_percent // 100,
                "used_percent": dev.storage_used_percent,
            },
        },
        "lorawan": {
            # The rendered radio.mode, which the Hub's description card labels.
            "mode": dev.radio_mode,
            # False means no concentrator attached: the portal then says nothing about
            # services that have no radio to run (UI-4), rather than reporting them down.
            "gateway_present": dev.gateway_present,
            "concentratord_running": dev.gateway_present,
            "chirpstack_running": dev.gateway_present,
            "sensor_count": len(dev.stickers),
            "gateway_eui": dev.gateway_eui if dev.gateway_present else None,
            # Untyped on the wire (mqtt.md pins no shape) and stored verbatim by the portal;
            # this is a plausible cluster blob, not a contract.
            "cluster": {"role": dev.cluster_role, "peers": dev.cluster_peers},
        },
        # GitLab #58 — the `setup:` key the agent republishes so the portal can show it and
        # keep no copy. The mock's Hubs are all set up, with the role their profile already
        # carries; the "has not said" case (an agent predating the key) is reached by deleting
        # this block, which is the state the Hub page must render as "not reported" rather
        # than as "not completed".
        "setup": {"provisioned": True, "role": dev.cluster_role},
        # The device-tree identity the agent reads (devices.hub.md §3.1, amendment
        # 2026-09-28). Every mock Hub is a CM4; deleting a field, or the block, is the
        # "could not read it" case the card renders as "—".
        "hardware": {
            "model": "Raspberry Pi Compute Module 4",
            "revision": "1.1",
            "serial": dev.cpu_serial,
        },
        # southbridge_update.md §3.7: the SB firmware, the one the image bundles and the
        # update state. A bundled image plus an idle update enables the card's button.
        "southbridge": {
            "version": "south-zephyr 1.0.0 g5a1c2e90",
            "bundled": "south-zephyr 1.1.0 g73f39ed5",
            "update": {"state": "idle", "attempts": 0},
        },
    }
    client.publish(topic, json.dumps(payload), qos=1, retain=True)
    publish_radio_status(client, dev)


def publish_radio_status(client, dev: DeviceState):
    """radio/lorawan and radio/northbridge — the retained radio status documents
    proximos-control publishes (plan/control/radio/northbridge_diagnostics.md
    §3/§4a), shaped by `radio.mode`: a transport the mode does not run says
    `disabled`. Published with system/info, so both follow its cadence."""
    now = datetime.now(timezone.utc)
    ts = now.strftime("%Y-%m-%dT%H:%M:%SZ")
    lorawan_on = dev.radio_mode in ("lorawan", "dual")
    p2p_on = dev.radio_mode in ("p2p", "dual")
    server = dev.cluster_role == "server"
    if not lorawan_on:
        lorawan = {"timestamp": ts, "state": "disabled"}
    elif not dev.gateway_present:
        lorawan = {
            "timestamp": ts, "state": "error",
            "card": {"present": False, "device": "/dev/rak5146", "usb_id": None},
            "concentratord": {"unit": "failed", "result": "exit-code", "restarts": 3},
            "forwarder": {"unit": "active", "conn": "offline"},
            "last_error": {"message": "card: /dev/rak5146 missing", "at": ts},
        }
    else:
        rx = 800 + 37 * dev.index
        lorawan = {
            "timestamp": ts, "state": "alive",
            "card": {"present": True, "device": "/dev/rak5146", "usb_id": "0483:5740"},
            "concentratord": {
                "unit": "active", "result": "success", "restarts": 0, "version": "4.7.1",
                "gateway_eui": dev.gateway_eui or None, "last_stats": ts,
                "stats": {"rx_received": rx, "rx_ok": rx - 31, "tx_received": 96, "tx_emitted": 96},
            },
            "forwarder": {"unit": "active", "conn": "online"},
            "last_error": None,
        }
        # A client Hub forwards to the server's ChirpStack and runs none itself.
        if server:
            lorawan["chirpstack"] = {"unit": "active", "api": dev.radio_api_status, "version": "4.19.1"}
    if not p2p_on:
        nb = {"timestamp": ts, "state": "disabled"}
    else:
        boot = datetime.fromtimestamp(now.timestamp() - 3600 * (2 + dev.index), timezone.utc)
        nb = {
            "timestamp": ts, "state": "alive", "last_seen": ts,
            "fw_version": "0.3.0", "git_sha": "c05b8d3e", "proto_version": 0,
            "build_flags": [], "uart": "/dev/ttyAMA3",
            "uptime_s": int(now.timestamp() - boot.timestamp()),
            "boot": {"at": boot.strftime("%Y-%m-%dT%H:%M:%SZ"), "reset_cause": ["pin"]},
            "radio": {"state": "gateway", "freq_hz": 869525000, "sf": 7, "tx_power_dbm": 14},
            "gateway": {
                "net_id": 0x6D248A62 + dev.index, "peers": len(dev.p2p_nodes), "peers_max": 64,
                "queue_used": 0, "queue_max": 32,
            },
            "counters": {
                "rx": 412, "tx": 128, "rx_fresh": 398, "rx_dup": 14, "crc_err": 3,
                "link_err": 0, "rx_rearm_fail": 0, "tx_radio_err": 0, "tx_refused": 0,
                "rx_mic_fail": 0,
            },
            "err_flags": [], "last_error": None,
            "update": {"state": "idle", "bundled": "0.3.0+c05b8d3e", "attempts": 0,
                       "last_error": None, "note": None},
        }
    for area, doc in (("lorawan", lorawan), ("northbridge", nb)):
        client.publish(f"{RADIO_ROOT}/{dev.hostname}/radio/{area}", json.dumps(doc), qos=1, retain=True)


def publish_radio_gateways(client, dev: DeviceState, api_status: str, members: list[HubProfile]):
    """radio/gateways — the cluster-wide retained gateway report proximos-control
    publishes on its 60 s health tick (plan/portal/devices.hub.md §12 A2). No prefix,
    no hostname level: one report per broker, rows carry their own hostname.

    Which is why only the server profile publishes it. Two publishers on one
    retained topic do not merge, they overwrite — whichever wrote last would
    delete every other Hub's row — so the server owns the table and lists the
    whole cluster, as the real producer does. A Hub with no concentrator has
    no row at all; the portal's card renders that as "waiting / no radio"
    rather than as a gateway that is down.
    """
    rows = []
    for p in members:
        if not p.gateway_present:
            continue
        # The portal recomputes `status` on a 300 s window from last_seen, so
        # backdating a row past it is what a concentrator that stopped
        # reporting looks like (dev/hub.sh gateway-stale).
        stale = p.hostname in dev.stale_gateway_hosts
        rows.append({
            "gateway_eui": p.gateway_eui,
            "name": f"{p.label} concentrator",
            "hostname": p.hostname,
            "ip": p.ip,
            # Computed by the producer at publish time; the portal recomputes it on the
            # 300 s window from last_seen, which is fresh on every tick here.
            "status": "offline" if stale else "online",
            "last_seen": _ts_ago(900) if stale else _ts(),
            "rssi": -67 - 8 * p.index,
            "snr": round(9.5 - 1.5 * p.index, 1),
            "radio": "lorawan",
        })
    payload = {"timestamp": _ts(), "api_status": api_status, "gateways": rows}
    client.publish(RADIO_GATEWAYS_TOPIC, json.dumps(payload), qos=1, retain=True)


def publish_standby(client, dev: DeviceState, prefix: str):
    """Retained; per mqtt.md this uniquely never touches online/last_seen —
    published once here since the mock has no standby transitions to model."""
    topic = f"{prefix}/{dev.hostname}/power/standby"
    payload = {
        "timestamp": _ts(),
        "standby": dev.standby,
        "reason": None,
        "requested_by": "",
        "entered_at": None,
        "vin_mv": dev.dc_voltage_mv,
    }
    client.publish(topic, json.dumps(payload), qos=1, retain=True)


def publish_probe_1m(client, dev: DeviceState, prefix: str):
    """export/probe_1m/{line} — the authoritative 30-day hot-tier minute
    aggregate. Not retained (it's a ledger stream, not device state)."""
    minute_ts = int(time.time() // 60 * 60)
    for s in dev.sensors:
        topic = f"{prefix}/{dev.hostname}/export/probe_1m/{s.line}"
        # Export streams use the save-and-feed envelope (mqtt.md):
        # {message_id (required), exported_at, stream, stream_version, data}.
        payload = {
            "message_id": f"{dev.hostname}-probe_1m-{s.line}-{minute_ts}",
            "exported_at": int(time.time()),
            "stream": "probe_1m",
            "stream_version": 1,
            "data": {
                "minute_ts": minute_ts,
                "sensor_line": s.line,
                "min_c": round(s.current_value - 0.15, 2),
                "avg_c": round(s.current_value, 2),
                "max_c": round(s.current_value + 0.15, 2),
                "sample_count": 60,
                "disconnect_count": 0,
                "worst_alarm": s.alarm_state,
                "data_hmac": None,
            },
        }
        client.publish(topic, json.dumps(payload), qos=1, retain=False)


# ============================================================================
# Node (STICKER) + Beacon (EYE) simulation (M5-D)
# ============================================================================

_ALARM_RANK = {"NORMAL": 0, "WARNING": 1, "CRITICAL": 2}


def _field_state(value: float, t: FieldThreshold) -> str:
    if (t.critical_low is not None and value <= t.critical_low) or (
        t.critical_high is not None and value >= t.critical_high
    ):
        return "CRITICAL"
    if (t.warning_low is not None and value <= t.warning_low) or (
        t.warning_high is not None and value >= t.warning_high
    ):
        return "WARNING"
    return "NORMAL"


def _worst(states) -> str:
    return max(states, key=lambda s: _ALARM_RANK.get(s, 0), default="NORMAL")


def _node_topic(prefix: str, dev: DeviceState, dev_eui: str, leaf: str) -> str:
    return f"{prefix}/{dev.hostname}/lorawan/sensors/{dev_eui}/{leaf}"


def seed_sticker_history(dev: DeviceState, depth_s: int, interval_s: float):
    """Pre-seed each Node's ring buffer with backdated records so
    get_sticker_history has a full --history-depth-s window from t=0."""
    now = int(time.time())
    step = max(1, int(interval_s))
    for s in dev.stickers:
        temp, hum = s.temperature, s.humidity
        for ts in range(now - depth_s, now, step):
            temp = min(10.0, max(2.0, temp + random.uniform(-0.4, 0.4)))
            hum = min(70.0, max(40.0, hum + random.uniform(-1.5, 1.5)))
            s.history.append({
                "time": ts,
                "fields": {"temperature": round(temp, 2), "humidity": round(hum, 1)},
                "counters": {},
            })


def _walk(value: float, step: float, lo: float, hi: float) -> float:
    """One tick of a bounded random walk — the rich Node's field generator."""
    return min(hi, max(lo, value + random.uniform(-step, step)))


def publish_lorawan_sensors(client, dev: DeviceState, prefix: str, history_depth_s: int):
    """Aggregate Node telemetry (~20-30 s per mqtt.md §lorawan/sensors).
    last_seen changes on every publish — that's what gates portal persistence
    (lorawan_record_on_uplink_only). Also appends to the history ring."""
    topic = f"{prefix}/{dev.hostname}/lorawan/sensors"
    now = time.time()
    sensors_data = []
    for s in dev.stickers:
        # A disabled Sticker is simply absent from the frame — not present
        # with a stale `last_seen`, which the portal would keep crediting.
        # This is the seam B3's *Accept:* needs: one Node silent while the
        # others report (devices.backend.md §16.5).
        if not s.enabled:
            continue
        # Random walks; rare spike above critical_high (12.0) so events fire.
        s.temperature = min(10.0, max(2.0, s.temperature + random.uniform(-0.4, 0.4)))
        if random.random() < 0.05:
            s.temperature = 12.0 + random.uniform(0.0, 2.0)
        s.humidity = min(70.0, max(40.0, s.humidity + random.uniform(-1.5, 1.5)))
        s.voltage = round(3.0 + random.uniform(-0.05, 0.05), 3)
        s.fcnt += 1
        if random.random() < 0.2:
            s.hall_left_count += 1

        fields = {
            "temperature": round(s.temperature, 2),
            "humidity": round(s.humidity, 1),
            "voltage": s.voltage,
        }
        counters = {"fCnt": s.fcnt, "hall_left_count": s.hall_left_count}
        if s.rich:
            s.ext_temperature_1 = _walk(s.ext_temperature_1, 0.3, -20.0, 30.0)
            s.ext_temperature_2 = _walk(s.ext_temperature_2, 0.3, -20.0, 30.0)
            s.machine_probe_temperature_1 = _walk(s.machine_probe_temperature_1, 0.5, 15.0, 45.0)
            s.machine_probe_temperature_2 = _walk(s.machine_probe_temperature_2, 0.5, 15.0, 45.0)
            s.machine_probe_humidity_1 = _walk(s.machine_probe_humidity_1, 1.0, 30.0, 70.0)
            s.machine_probe_humidity_2 = _walk(s.machine_probe_humidity_2, 1.0, 30.0, 70.0)
            s.illuminance = _walk(s.illuminance, 25.0, 0.0, 900.0)
            s.pressure = _walk(s.pressure, 0.4, 980.0, 1040.0)
            s.altitude = _walk(s.altitude, 0.5, 200.0, 270.0)
            fields.update({
                "ext_temperature_1": round(s.ext_temperature_1, 2),
                "ext_temperature_2": round(s.ext_temperature_2, 2),
                "machine_probe_temperature_1": round(s.machine_probe_temperature_1, 2),
                "machine_probe_temperature_2": round(s.machine_probe_temperature_2, 2),
                "machine_probe_humidity_1": round(s.machine_probe_humidity_1, 1),
                "machine_probe_humidity_2": round(s.machine_probe_humidity_2, 1),
                "illuminance": round(s.illuminance, 1),
                "pressure": round(s.pressure, 1),
                "altitude": round(s.altitude, 1),
                # v2.0.0 names: 1-Wire slots by number, the analog input B.
                "w1_1_temperature": round(s.ext_temperature_1, 2),
                "w1_2_temperature": round(s.ext_temperature_2, 2),
            })
            if s.report.input_b == "analog":
                s.input_b_voltage = _walk(s.input_b_voltage, 0.3, 0.0, 10.0)
                fields["input_b_voltage"] = round(s.input_b_voltage, 2)
            # Counters only ever climb — that is what makes them counters,
            # and what makes them unthresholdable (lorawan.rs).
            if random.random() < 0.30:
                s.motion_count += 1
            if random.random() < 0.15:
                s.hall_right_count += 1
            if random.random() < 0.10:
                s.input_a_count += 1
            if random.random() < 0.05:
                s.input_b_count += 1
            counters.update({
                "motion_count": s.motion_count,
                "hall_right_count": s.hall_right_count,
                "input_a_count": s.input_a_count,
                "input_b_count": s.input_b_count,
            })
        states: dict[str, str] = {}
        events: list[dict] = []
        for t in s.field_thresholds:
            v = fields.get(t.field)
            if v is None:
                continue
            new_state = _field_state(v, t)
            if new_state == "CRITICAL" and s.field_alarm_states.get(t.field) != "CRITICAL":
                # The fPort-3 alarm as control-radio's decoder publishes it
                # (decode/reading.rs: the decoder's `type` travels as `side`,
                # the edge as `event`). The v1 `edge`/`alarm_type` shape left
                # the Node's last-alarm card untranslated (S37-5).
                is_high = t.critical_high is not None and v >= t.critical_high
                events.append({
                    "type": "alarm",
                    "ts": _ts(),
                    "slot": 0,
                    "quantity": t.field,
                    "event": "activate",
                    "side": "high" if is_high else "low",
                    "value": v,
                })
            states[t.field] = new_state
        s.field_alarm_states = states

        rssi = -60 + random.randint(-10, 10)
        snr = round(8 + random.uniform(-3.0, 3.0), 1)

        # History ring buffer: one record per telemetry tick, pruned to depth.
        s.history.append({
            "time": int(now),
            "fields": {"temperature": fields["temperature"], "humidity": fields["humidity"]},
            "counters": {},
        })
        cutoff = now - history_depth_s
        while s.history and s.history[0]["time"] < cutoff:
            s.history.pop(0)

        sensors_data.append({
            "dev_eui": s.dev_eui,
            "name": s.name,
            "serial_number": s.serial_number,
            "location": s.location,
            "fields": fields,
            "field_alarm_states": states,
            "field_thresholds": [asdict(t) for t in s.field_thresholds],  # list ⇒ authoritative
            "counters": counters,
            "events": events,
            "rssi": rssi,
            "snr": snr,
            "gateways": [{"gateway_id": dev.gateway_id, "rssi": rssi, "snr": snr}],
            "fcnt": s.fcnt,
            "dr": 5,
            "downlink_gateway_id": dev.gateway_id,
            "last_seen": _ts(),
            "alarm_state": _worst(states.values()),
        })
    payload = {"timestamp": _ts(), "sensors": sensors_data}
    client.publish(topic, json.dumps(payload), qos=0, retain=False)


def publish_sticker_command_result(client, dev: DeviceState, prefix: str, s: Sticker,
                                   command: str, result: str, detail: Optional[str] = None):
    """…/{dev_eui}/command — Node command result (results incl. 'requested';
    'out_of_range' is how the portal learns a refused history request)."""
    s.seq += 1
    payload = {
        "dev_eui": s.dev_eui,
        "command": command,
        "seq": s.seq,
        "result": result,
        "expect": None,
        "detail": detail,
        "fault_key": None,
        "ts": int(time.time()),
    }
    client.publish(_node_topic(prefix, dev, s.dev_eui, "command"), json.dumps(payload), qos=1, retain=False)


def publish_sticker_config_tombstone(client, dev: DeviceState, prefix: str, s: Sticker):
    """Empty retained tombstone on the config topic first, per mqtt.md
    (portal treats the failed JSON decode as warning-only)."""
    client.publish(_node_topic(prefix, dev, s.dev_eui, "config"), b"", qos=1, retain=True)


def publish_sticker_config_pages(client, dev: DeviceState, prefix: str, s: Sticker,
                                 keys: Optional[list] = None):
    """Paged fPort-85 device-config readback. Optional `keys` selection:
    known keys are returned, unknown ones land in `missing` (read_status
    'partial'). Pages are not retained — the retained slot holds the
    tombstone published at startup."""
    cfg = s.device_config
    if keys:
        selected = {k: cfg[k] for k in keys if k in cfg}
        missing = [k for k in keys if k not in cfg]
    else:
        selected, missing = dict(cfg), []
    items = list(selected.items())
    pages = [items[i:i + CONFIG_PAGE_SIZE] for i in range(0, len(items), CONFIG_PAGE_SIZE)] or [[]]
    page_count = len(pages)
    topic = _node_topic(prefix, dev, s.dev_eui, "config")
    for i, page in enumerate(pages):
        payload = {
            "dev_eui": s.dev_eui,
            "config": dict(page),
            "page_index": i,
            "page_count": page_count,
            "synced_at": _ts(),  # RFC 3339, as the agent (publisher.rs `timestamp`)
            "last_ack": {"seq": s.seq, "result": "ok"},
            "read_status": "partial" if missing else "complete",
            "missing": missing,
            "error": None,
            "chunks_done": page_count,
            "chunks_total": page_count,
        }
        client.publish(topic, json.dumps(payload), qos=1, retain=False)


def publish_sticker_info(client, dev: DeviceState, prefix: str, s: Sticker):
    """…/{dev_eui}/info — retained projected info object. An empty payload
    would be the removal tombstone (not modeled: the mock never removes)."""
    s.seq += 1
    payload = {
        "dev_eui": s.dev_eui,
        "fw_major": s.report.fw[0],
        "fw_minor": s.report.fw[1],
        "fw_patch": s.report.fw[2],
        "serial_number": s.serial_number,
        "uptime_s": int(time.time() - dev.start_ts),
        "battery_mv": int(s.voltage * 1000),
        "device_status": "ok",
        "seq": s.seq,
        "active_alarms": [],
    }
    client.publish(_node_topic(prefix, dev, s.dev_eui, "info"), json.dumps(payload), qos=1, retain=True)


def publish_sticker_full_config(client, dev: DeviceState, prefix: str, s: Sticker):
    """…/{dev_eui}/full-config — config-page shape plus last_seq; includes
    read-only keys on top of the settable store, never secret keys."""
    full = dict(s.device_config)
    full.update({
        "info.fw_version": s.report.fw_text,
        "info.hw_revision": "R3",
        "info.dev_eui": s.dev_eui,
    })
    payload = {
        "dev_eui": s.dev_eui,
        "config": full,
        "page_index": 0,
        "page_count": 1,
        "synced_at": _ts(),  # RFC 3339, as the agent (publisher.rs `timestamp`)
        "last_ack": {"seq": s.seq, "result": "ok"},
        "read_status": "complete",
        "missing": [],
        "error": None,
        "chunks_done": 1,
        "chunks_total": 1,
        "last_seq": s.seq,
    }
    client.publish(_node_topic(prefix, dev, s.dev_eui, "full-config"), json.dumps(payload), qos=1, retain=False)


def publish_sticker_history(client, dev: DeviceState, prefix: str, s: Sticker,
                            t_from, t_to, history_depth_s: int):
    """…/{dev_eui}/history — ring-buffer replay frames (≤20 records each),
    then a command result. Window entirely outside the buffer ⇒ result
    'out_of_range' and NO frames (mqtt.md: refusals travel via the command
    topic only)."""
    if s.history_mode == "silent":
        # No frames AND no command result: the portal has nothing to fail
        # the job on, so the reconciler waits out `retry_cadence_ticks x
        # interval_report`, re-sends twice more, and raises the §6.3
        # "cannot download history" alert on the third failure.
        print(f"[{dev.hostname}] get_sticker_history for {s.dev_eui}: silent (mock)")
        return
    if s.history_mode == "history_unavailable":
        publish_sticker_command_result(
            client, dev, prefix, s, "get_sticker_history", "history_unavailable",
            detail="mock: history storage unavailable on this Node",
        )
        return
    now = int(time.time())
    lo = int(t_from) if t_from is not None else now - history_depth_s
    hi = int(t_to) if t_to is not None else now
    records = [r for r in s.history if lo <= r["time"] <= hi]
    if not records:
        publish_sticker_command_result(
            client, dev, prefix, s, "get_sticker_history", "out_of_range",
            detail=f"window [{lo},{hi}] outside ring buffer",
        )
        return
    frames = [records[i:i + HISTORY_RECORDS_PER_FRAME]
              for i in range(0, len(records), HISTORY_RECORDS_PER_FRAME)]
    topic = _node_topic(prefix, dev, s.dev_eui, "history")
    for i, frame in enumerate(frames):
        payload = {
            "dev_eui": s.dev_eui,
            "frame_index": i,
            "frame_count": len(frames),
            "records": frame,
        }
        client.publish(topic, json.dumps(payload), qos=1, retain=False)
    publish_sticker_command_result(
        client, dev, prefix, s, "get_sticker_history", "ok",
        detail=f"{len(records)} records in {len(frames)} frames",
    )


def publish_eye_sensors(client, dev: DeviceState, prefix: str):
    """eye/sensors — BLE beacon snapshot per mqtt.md §eye/sensors. Registered
    tag plus any discovered-only tags; field_alarm_states computed from the
    tag's thresholds (empty until set_eye_field_threshold sets some)."""
    topic = f"{prefix}/{dev.hostname}/eye/sensors"
    now = int(time.time())
    tags = []
    for t in dev.eye_tags:
        if not t.enabled:
            continue  # silenced by dev/beacon.sh — a quiet broadcast is an absent tag
        if not t.present:
            # B6: out of the snapshot entirely. For a discovered-only tag
            # this is the portal's ONLY instruction to prune it
            # (reconcile-by-absence); an enrolled tag simply stops being
            # reported and the offline detector notices.
            continue
        t.temperature_c = 5.0 + random.uniform(-1.0, 1.0)
        t.humidity_pct = 45.0 + random.uniform(-5.0, 5.0)
        # A tag is mostly still; when it moves, the counter climbs.
        t.moving = random.random() < 0.15
        if t.moving:
            t.movement_count += 1
        states: dict[str, str] = {}
        values = {"temperature": t.temperature_c, "humidity": t.humidity_pct, "battery": t.battery_mv}
        for th in t.field_thresholds:
            v = values.get(th.field)
            if v is not None:
                states[th.field] = _field_state(v, th)
        tags.append({
            "gateway": dev.hostname,
            "mac": t.mac,
            "name": t.name,
            "temperature_c": round(t.temperature_c, 2),
            "humidity_pct": int(t.humidity_pct),
            "battery_mv": t.battery_mv,
            # The tag asserts this itself, which is exactly why `battery` is
            # not thresholdable portal-side (fields/eye.rs).
            "low_battery": t.battery_mv < 2500,
            "magnet_present": None,
            "magnet_detected": None,
            "moving": t.moving,
            "movement_count": t.movement_count,
            "pitch_deg": None,
            "roll_deg": None,
            "rssi": t.rssi + random.randint(-3, 3),
            "last_seen_ts": now,
            "stale": t.stale,
            "provisioning": t.provisioning,
            "discovered": t.discovered,
            "field_alarm_states": states,
            "alarm_state": _worst(states.values()),
        })
    # B6: `discovered_capped` rides the envelope, never the tags. An empty
    # array is the empty snapshot — authoritative precisely because it is
    # empty — so it is never reported as capped.
    payload = {
        "timestamp": _ts(),
        "discovered_capped": bool(tags) and dev.eye_discovered_capped,
        "tags": tags,
    }
    client.publish(topic, json.dumps(payload), qos=0, retain=False)


# ============================================================================
# P2P Nodes — the v2 radio northbound family (plan/control/radio/plan.md §9.4)
# ============================================================================
#
# `proximos/{hostname}/nodes/{dev_eui}/{info,reading}`, published per node by
# proximos-control. Not `--prefix`: the producer owns the fixed `proximos/`
# root (control_radio::northbound::ROOT), which is why the portal subscribes
# it from the producer's own builder rather than from its configured prefix.
#
# One document per node per uplink, never v1's full array — that shape
# overflowed rumqttc's 10 240 B packet cap with a single well-buffered Node
# and tore the whole MQTT client down.


#: One P2P network per Hub; the central hands the same NetID to every session.
P2P_NET_ID = 7


#: `--beacon-history`: `archive` answers a `beacon-history` request the way
#: control/ble does (beacon_history.md §2.2-2.3), `silent` accepts it and
#: publishes nothing.
BEACON_HISTORY_MODE = "archive"
BEACON_ARCHIVE_INTERVAL_S = 900
BEACON_ARCHIVE_DEPTH_S = 3 * 86400


def publish_beacon_archive(client, dev: DeviceState, prefix: str, tag: "EyeTag", params: dict):
    """Walk a Beacon's archive for one `beacon-history` job: `requested`,
    `receiving` x/y, `records` in batches of at most 240, then `done`. The
    archive is synthetic: a slow daily wave around the tag's temperature,
    one record per 15 min, three days deep."""
    topic = f"{prefix}/{dev.hostname}/eye/history"
    job = str(params.get("job") or f"bh-mock-{int(time.time())}")
    now = int(time.time())
    to = int(params.get("to") or now)
    start = int(params.get("from") or (now - BEACON_ARCHIVE_DEPTH_S))
    start = max(start, now - BEACON_ARCHIVE_DEPTH_S)
    first = start - start % BEACON_ARCHIVE_INTERVAL_S + BEACON_ARCHIVE_INTERVAL_S
    stamps = list(range(first, min(to, now) + 1, BEACON_ARCHIVE_INTERVAL_S))
    base = {"job": job, "mac": tag.mac, "from": start, "to": to}

    def send(doc):
        client.publish(topic, json.dumps({**base, **doc}), qos=1, retain=False)

    send({"state": "requested", "received": 0, "expected": None, "reason": None})
    time.sleep(1.0)
    send({"state": "receiving", "received": 0, "expected": len(stamps), "reason": None})
    for seq, at in enumerate(range(0, len(stamps), 240)):
        chunk = stamps[at:at + 240]
        records = [[ts, round(tag.temperature_c + 1.5 * math.sin(ts / 86400 * 2 * math.pi)
                              + random.uniform(-0.2, 0.2), 2)] for ts in chunk]
        send({"state": "records", "seq": seq, "interval_s": BEACON_ARCHIVE_INTERVAL_S,
              "records": records})
        send({"state": "receiving", "received": at + len(chunk), "expected": len(stamps),
              "reason": None})
        time.sleep(0.5)
    send({"state": "done", "received": len(stamps), "expected": len(stamps), "reason": None,
          "sessions": 1, "rejected_chunks": 0, "future_dropped": 0, "crc_failed": 0})
    print(f"[{dev.hostname}] beacon-history: {tag.mac} job {job} — {len(stamps)} records")


def _radio_node_topic(dev: DeviceState, dev_eui: str, area: str) -> str:
    return f"{RADIO_ROOT}/{dev.hostname}/nodes/{dev_eui}/{area}"


def publish_node_reports(client, dev: DeviceState, dev_eui: str, serial: str,
                         report: NodeReport, battery_mv: int):
    """`…/nodes/{dev_eui}/device_info` and `…/config` — the Node's own boot
    `Info` and `ConfigDump`, retained (node_reported_state.md §4), in
    control-radio's `DeviceInfoDocument`/`ConfigDocument` shapes. Published
    for LoRaWAN and P2P Nodes alike: the carrier is not on this topic."""
    now = _ts()
    info = {
        "fw_major": report.fw[0],
        "fw_minor": report.fw[1],
        "fw_patch": report.fw[2],
        "serial_number": serial,
        "uptime_s": int(time.time() - dev.start_ts) + 3600,
        "battery": battery_mv,
        "device_status": 0,
        "reset_cause": 1 << 1 if report.fault else 1 << 3,  # software | power_on
        "unix_time": int(time.time()),
        "active_alarms": [],
        "w1_slot_state": ["ok" if t != "empty" else "none" for t in report.w1_slots],
    }
    if report.fault:
        # A bus fault in app_compose, as ttn.js decodes it (Info fields 20..22).
        info.update({
            "fault_reason": 25,
            "fault_reason_name": "K_ERR_ARM_BUS_PRECISE_DATA_BUS",
            "fault_pc": "0x0002f3a4",
            "fault_lr": "0x0002f381",
        })
    client.publish(_radio_node_topic(dev, dev_eui, "device_info"), json.dumps({
        "dev_eui": dev_eui, "received_at": now, "device_info": info,
    }), qos=1, retain=True)
    config = {
        "application.interval_sample": 30,
        "application.interval_report": 300,
        "sensors.cap_temperature": True,
        "sensors.cap_humidity": True,
        "sensors.cap_hall_left": True,
        "sensors.cap_barometer": report.barometer,
        "sensors.cap_light_sensor": report.light_sensor,
        "sensors.cap_w1_sensors": any(t != "empty" for t in report.w1_slots),
        "sensors.cap_gpio_mode": report.gpio_mode,
        "sensors.cap_gpio_input_a": report.input_a,
        "sensors.cap_gpio_input_b": report.input_b,
    }
    client.publish(_radio_node_topic(dev, dev_eui, "config"), json.dumps({
        "dev_eui": dev_eui, "received_at": now, "seq": 0, "page_count": 1,
        "config": config, "w1_slot_type": list(report.w1_slots), "body_hex": "",
    }), qos=1, retain=True)


def publish_radio_node_info(client, dev: DeviceState, n: P2pNode):
    """`…/nodes/{dev_eui}/info` — the retained per-node inventory.

    Its `transport` field is the only place the radio path is stated (GitLab
    #73: the topic no longer carries a transport level), and the portal
    defaults a Node to `lorawan` until this document lands. Retained, so it
    is replayed to the portal on subscribe ahead of any live reading.

    The shape is control-radio's (`p2p/session.rs::info_core` plus what the
    join handler in `p2p/runtime.rs` adds): `pairing_status` is `registered`
    from `node-add` and `joined` once a session exists, and only the joined
    document carries `dev_addr`/`net_id`/`session_state`/`joined_at`.
    """
    payload = {
        "transport": "p2p",
        "dev_eui": n.dev_eui,
        "serial_number": n.serial_number,
        "profile": "default",
        "pairing_status": "joined" if n.joined_at else "registered",
    }
    if n.joined_at:
        payload.update({
            "dev_addr": n.dev_addr,
            "net_id": P2P_NET_ID,
            "session_state": "joined",
            "proto_version": 1,
            "joined_at": n.joined_at,
        })
    client.publish(_radio_node_topic(dev, n.dev_eui, "info"),
                   json.dumps(payload), qos=1, retain=True)


def _v2_reading_extras(n: P2pNode, ts: str):
    """The fields, counters and `no_data` events a v2.0.0 Node adds to a
    reading for what its NodeReport switches on (decode/reading.rs: a null
    is a `no_data` event naming the field and no entry in `fields`)."""
    r = n.report
    fields: dict = {}
    counters: dict = {}
    nulls = set(r.null_fields)
    if r.gpio_mode == "pir":
        if random.random() < 0.3:
            n.motion_count += 1
        counters["motion_count"] = n.motion_count
    if r.gpio_mode == "inputs" and r.input_a == "analog":
        n.input_a_voltage = _walk(n.input_a_voltage, 0.2, 0.0, 10.0)
        # Now and then the ADC reads NA: the row goes to a dash and back.
        if random.random() < 0.25:
            nulls.add("input_a_voltage")
        else:
            fields["input_a_voltage"] = round(n.input_a_voltage, 2)
    if r.gpio_mode == "inputs" and r.input_b in ("digital", "pulse"):
        if random.random() < 0.2:
            n.input_b_count += 1
        counters["input_b_count"] = n.input_b_count
    if r.barometer and "pressure" not in nulls:
        n.pressure = _walk(n.pressure, 0.4, 980.0, 1040.0)
        n.altitude = _walk(n.altitude, 0.5, 200.0, 270.0)
        fields["pressure"] = round(n.pressure, 1)
        fields["altitude"] = round(n.altitude, 1)
    events = [{"type": "no_data", "ts": ts, "field": f} for f in sorted(nulls)]
    return fields, counters, events


def publish_radio_node_readings(client, dev: DeviceState):
    """`…/nodes/{dev_eui}/reading` — one decoded uplink each. QoS 1, not
    retained: a retained reading would hand every new subscriber one stale
    measurement as if it were live."""
    for i, n in enumerate(dev.p2p_nodes):
        if not n.enabled:
            continue
        # The first frame is the join: the central commits a session and
        # republishes the retained `info` as the joined document before the
        # reading goes out (p2p/runtime.rs), so a subscriber never sees a
        # reading from a node whose `info` still says `registered`.
        if not n.joined_at:
            n.dev_addr = i + 1
            n.joined_at = _ts()
            publish_radio_node_info(client, dev, n)
        n.temperature = _walk(n.temperature, 0.4, 2.0, 10.0)
        n.humidity = _walk(n.humidity, 1.5, 40.0, 70.0)
        n.voltage = round(3.0 + random.uniform(-0.05, 0.05), 3)
        if random.random() < 0.2:
            n.hall_left_count += 1
        received_at = _ts()
        fields, counters, events = _v2_reading_extras(n, received_at)
        rssi = -70 + random.randint(-12, 12)
        snr = round(6 + random.uniform(-3.0, 3.0), 2)
        payload = {
            # The documented derivation for an uplink with no ChirpStack
            # deduplication_id, which a P2P uplink never has
            # (decode::reading::message_id).
            "message_id": f"{n.dev_eui}:{received_at}",
            "dev_eui": n.dev_eui,
            "serial": n.serial_number,
            "fields": {
                "temperature": round(n.temperature, 2),
                "humidity": round(n.humidity, 1),
                "voltage": n.voltage,
                **fields,
            },
            "counters": {"hall_left_count": n.hall_left_count, **counters},
            "events": events,
            # P2P reaches this Hub's own radio and no other, so a
            # single-element list (northbound.rs).
            "gateways": [{"gateway_id": dev.gateway_id, "rssi": rssi, "snr": snr}],
            "rssi": rssi,
            "snr": snr,
            "received_at": received_at,
        }
        client.publish(_radio_node_topic(dev, n.dev_eui, "reading"),
                       json.dumps(payload), qos=1, retain=False)


# ============================================================================
def publish_pair_invite(client, dev: DeviceState, prefix: str, code: str, origin: str = "ble"):
    """One `pair/invite` — the device asking the portal on this machine to pair with it.

    GitLab #61, plan/control/ble/peripheral.md §6.5. On a real Hub this is what the agent
    publishes after FB1A hands it the code the operator typed into the phone over BLE; the
    portal then does what an admin clicking "Pair" would.

    QoS 1, **retain=False**, and published exactly once. The retain flag is the point: an
    invitation is a one-shot signal, and the portal drops a retained one on arrival
    (`mqtt::dispatch`), so this mock must not be the thing that makes that path look fine.
    """
    topic = f"{prefix}/{dev.hostname}/pair/invite"
    payload = {"code": code, "origin": origin}
    client.publish(topic, json.dumps(payload), qos=1, retain=False)
    print(f"[{dev.hostname}] pair/invite sent (code {code}, origin {origin}, retain=false)")
    # The portal checks the code against the front-panel alphabet before it touches the device,
    # and these demo codes predate that alphabet: ABC123 and GHI789 carry a 1 and an I, the two
    # characters it drops. On the server Hub it makes no difference (the local path uses the
    # fixed password); on a client Hub the invite is refused, which is itself the path worth
    # seeing — say so rather than leaving it looking like a bug.
    if any(c not in PAIRING_CODE_ALPHABET for c in code.upper()) or len(code) != 6:
        print(f"[{dev.hostname}] note: {code!r} is not from the front-panel alphabet "
              f"({PAIRING_CODE_ALPHABET}) — a remote Hub's invite will be refused as "
              f"invalid_code. Pass --pairing-code with six characters from it to pair for real.")


# Signed-command simulator (M4-E) — device CA, pairing, challenge/confirm
# ============================================================================

PAIR_KDF_ITERATIONS = 480_000  # PBKDF2-HMAC-SHA256, per mqtt.md §Pairing

# PBKDF2 password for a pair/request that sets "local": true — the portal is
# running on this device, so there is no LCD for its operator to read a code
# off. Public by construction; the protection is that the broker is
# loopback-only and that the pairing window on the device was opened by hand.
#
# Hand-mirrored in three places, each with a literal tripwire test on the
# Rust sides — change all of them together:
#   portal/crates/fiber-protocol/src/pairing.rs::LOCAL_PAIRING_PASSWORD
#   portal/crates/control-core/src/libs/pairing/crypto.rs::LOCAL_PAIRING_PASSWORD
#   here
LOCAL_PAIRING_PASSWORD = "proximos-local-pairing-v1"

# The pairing code's alphabet, as the front panel generates it and as FB1A and the portal both
# check it: no O, 0, I, 1 or L, because an operator reads it off one screen and types it into
# another. Mirrored from control-core/src/libs/pairing/code.rs.
PAIRING_CODE_ALPHABET = "23456789ABCDEFGHJKMNPQRSTUVWXYZ"

# ---------------------------------------------------------------------------
# The fixture (plan/portal/mqtt/design.md §10)
# ---------------------------------------------------------------------------
#
# One file, three implementations: `fiber-protocol` builds the pre-image and
# compares the string, `control-core` feeds the wire JSON through its own
# parser and compares the same string, and this mock loads both documents at
# start and asserts its `_canonical` reproduces every vector before it serves
# anything (`--selftest`). Read by relative path, exactly as `control-core`'s
# `v2_tests.rs` does — there is no copy of the verb table here, because a copy
# is a thing that drifts.
#
# The mock used to be *looser* than the device on time (±600 s against 60) and
# *tighter* on challenge life (60 s against 300), so a run could pass here and
# fail on a Hub, or the reverse. It now takes the device's numbers.
VECTORS_DIR = (Path(__file__).resolve().parent / ".." / ".." /
               "crates" / "fiber-protocol" / "vectors")


def _load_vectors(name: str) -> dict:
    path = (VECTORS_DIR / name).resolve()
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except OSError as e:
        sys.exit(
            f"cannot read {path}: {e}\n"
            f"The vectors are checked in next to the portal's contract crate; the mock "
            f"reads them rather than keeping its own copy of the verb table."
        )


CATALOGUE = _load_vectors("catalogue.json")
ENVELOPE_VECTORS = _load_vectors("envelope_v2.json")

ENVELOPE_VERSION = ENVELOPE_VECTORS["envelope_version"]
#: `command_type` -> `{"permission", "level", "single_shot"}`.
COMMAND_SPECS = {c["command_type"]: c for c in CATALOGUE["commands"]}
#: level name -> the `level:<name>` pseudo-permission a certificate must carry
#: for that level (design.md §4.3).
LEVEL_PERMISSIONS = CATALOGUE["level_permissions"]

# The device's numbers (design.md §8, `control-core`'s constants).
TIMESTAMP_WINDOW_S = 60        # ±60 s
CHALLENGE_TTL_S = 300          # and `expires_at` = request timestamp + 300
MAX_CHALLENGES = 10            # concurrent, `manage` and above only


def _rust_float(value: float) -> str:
    """`serde_json`'s rendering of an `f64`, from Python's.

    Both produce the shortest string that round-trips, and on five of the
    fixture's six float pins they already agree byte for byte. They part on
    **negative exponents**: Python pads the exponent to two digits (`1e-07`)
    and Rust does not (`1e-7`). Positive exponents are two digits either way
    (`1e+16`), which is why only one pin caught it.

    Found by `--selftest` on 2026-09-15, against a fixture that was written to
    catch exactly this class of thing. design.md §10 named one known way the
    three implementations could disagree — `ensure_ascii` — and this is the
    second."""
    text = repr(value)
    if "e" not in text:
        return text
    mantissa, _, exponent = text.partition("e")
    sign = ""
    if exponent[:1] in ("+", "-"):
        sign, exponent = exponent[0], exponent[1:]
    return f"{mantissa}e{sign}{exponent.lstrip('0') or '0'}"


def _canonical(obj) -> bytes:
    """The signing canonicalization, treated as spec: sorted-key compact JSON,
    recursive, matching `fiber_protocol::canonical_json` and the device's
    `BTreeMap` byte for byte. That agreement is the contract (design.md §3.2),
    and `--selftest` is where this file proves it against the fixture rather
    than asserting it in a comment.

    Two things Python's defaults get wrong for this job:

    - `ensure_ascii=True` would escape a Czech `reason` into `\\uXXXX` where
      `serde_json` emits the character — a different pre-image and a signature
      that verifies nowhere. The fixture's `non_ascii_reason` case exists for
      this and fails the moment the argument is dropped.
    - float exponents, per [`_rust_float`].

    Floats are swapped for unique placeholder strings before the dump and
    substituted back after, so the rendering is controlled without
    re-implementing JSON's string escaping — which is the part Python already
    gets right and the part hand-rolling would break."""
    floats: dict[str, str] = {}

    def swap(value):
        if isinstance(value, float):
            token = f"__float_{uuid.uuid4().hex}__"
            floats[token] = _rust_float(value)
            return token
        if isinstance(value, dict):
            return {k: swap(v) for k, v in value.items()}
        if isinstance(value, list):
            return [swap(v) for v in value]
        return value

    text = json.dumps(swap(obj), sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    for token, rendered in floats.items():
        text = text.replace(f'"{token}"', rendered)
    return text.encode()


def _b64(data: bytes) -> str:
    return base64.b64encode(data).decode()


def _json_text(value) -> str:
    """`value` as `serde_json::Value`'s `Display` renders it — compact, no
    ASCII escaping — so an error message that quotes a wire value is
    byte-identical on both halves."""
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def _rfc3339(ts: int) -> str:
    """A unix timestamp as the `YYYY-MM-DDTHH:MM:SSZ` string the certificate
    schema carries (`crypto/certificate.rs`, `vectors/certificate.json`)."""
    return datetime.fromtimestamp(int(ts), tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _parse_rfc3339(value) -> Optional[float]:
    """The inverse, fail-closed: `None` for anything that is not a parseable
    RFC3339 string — which is how the device reads it (`is_expired()` is true
    for an unparseable `expires_at`)."""
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


class Refused(Exception):
    """A refusal the device would publish on `{prefix}/{host}/errors`.

    `error` and `message` are the device's own, not the mock's invention —
    `control-core` renders `AuthError`'s `Display` around `CryptoError`'s
    `Debug` and publishes the pair through `publish_error`, so the strings
    below are what a portal actually reads off a Hub. The mock matching them
    is the whole point of P3: a command that fails here must fail there, with
    the same words, or the mock is not evidence of anything."""

    def __init__(self, error: str, message: str, command: Optional[str] = None):
        super().__init__(f"{error}: {message}")
        self.error = error
        self.message = message
        # The `command` the errors document names, when it is not the dialect
        # that arrived. design.md §3.2 fixes `unsupported_envelope` to
        # `config_request` on both dialects — an unsupported envelope is one
        # whose selector the device does not trust — and the device's
        # `classify_parse_error` does exactly that.
        self.command = command

    @staticmethod
    def unsupported_envelope(detail: str = "") -> "Refused":
        return Refused(
            "unsupported_envelope",
            f"v={ENVELOPE_VERSION} required" + (f", got {detail}" if detail else ""),
            command="config_request",
        )

    @staticmethod
    def invalid_command(detail: str) -> "Refused":
        return Refused("authorization_failed", f"Invalid command: {detail}")

    @staticmethod
    def unknown_command(command_type) -> "Refused":
        return Refused.invalid_command(f"Unknown command type: {command_type}")

    @staticmethod
    def signature(detail: str) -> "Refused":
        return Refused("authorization_failed", f"Signature verification failed: {detail}")


def request_preimage(payload: dict) -> dict:
    """The nine signed keys of a `config_request` (design.md §3.2).

    `level` sorts between `command_type` and `nonce`, `v` last after
    `timestamp` — but nothing here depends on that: `_canonical` sorts
    structurally, which is the property that makes the position free and the
    three implementations agree."""
    return {
        "command_type": payload.get("command_type"),
        "level": payload.get("level"),
        "nonce": payload.get("nonce"),
        "params": payload.get("params", {}),
        "reason": payload.get("reason", ""),
        "request_id": payload.get("request_id"),
        "signer_id": payload.get("signer_id"),
        "timestamp": payload.get("timestamp"),
        "v": payload.get("v"),
    }


def confirm_preimage(payload: dict) -> dict:
    """The seven signed keys of a `config_confirm`. `command_type`, `params`
    and `reason` ride along unsigned and informational, as in v1."""
    return {
        "challenge_id": payload.get("challenge_id"),
        "confirmation": payload.get("confirmation"),
        "level": payload.get("level"),
        "nonce": payload.get("nonce"),
        "signer_id": payload.get("signer_id"),
        "timestamp": payload.get("timestamp"),
        "v": payload.get("v"),
    }


class CommandSim:
    """Device-side half of the signed-command protocol.

    Holds the device CA (deterministic from --seed so restarts keep the
    identity), answers pairing, verifies config_request/config_confirm
    signatures, and applies phase-1 commands to the shared DeviceState.
    Runs entirely in paho's network thread (handlers are quick; slow
    transitions like restart go to daemon timers)."""

    def __init__(self, client, dev: DeviceState, prefix: str, seed_phrase: str, pairing_code: str,
                 history_depth_s: int = DEFAULT_HISTORY_DEPTH_S,
                 inject_duplicate_challenge: bool = False):
        self.client = client
        self.inject_duplicate_challenge = inject_duplicate_challenge
        self.dev = dev
        self.prefix = prefix
        self.pairing_code = pairing_code
        self.history_depth_s = history_depth_s
        self.ca_seed = hashlib.sha256(seed_phrase.encode()).digest()  # 32 B raw Ed25519 seed
        self.ca_key = Ed25519PrivateKey.from_private_bytes(self.ca_seed)
        self.ca_public_hex = self.ca_key.public_key().public_bytes(
            serialization.Encoding.Raw, serialization.PublicFormat.Raw
        ).hex()
        # lowercased signer_id -> nonces seen. Per signer, as design.md §8
        # says ("unchanged"); note the device's store is **global** — one
        # `used_nonces(nonce PRIMARY KEY)` table (`crypto/nonce.rs`) — so a
        # nonce reused across two signers passes here and fails on a Hub.
        # The portal never does that (`generate_nonce` is 128 random bits),
        # so the looseness is recorded rather than closed.
        self.used_nonces: dict[str, set] = {}
        self.challenges: dict[str, dict] = {}   # challenge_id -> pending command
        self.lock = threading.Lock()
        # Config sync stub (plan/portal/config_sync.md §5): the last applied
        # generation and hash. Starts at 0, like a Hub with a fresh /data.
        self.sync_gen = 0
        self.sync_hash = ""
        self.boot_id = f"mock-{random.randint(0, 1 << 32):08x}"

    # ----- topics -----

    def _t(self, suffix: str) -> str:
        return f"{self.prefix}/{self.dev.hostname}/{suffix}"

    # The flat dialect is gone (design.md §1, §14 P4). All eight ex-`PLAIN`
    # verbs — `get_sticker_config/info/full_config/history`,
    # `sticker_force_send`, `silence_buzzer` and the rest — are served from
    # the one signed path, so there is nothing left to subscribe to under
    # `commands/<verb>`. The device loses its parsers in P4; the mock must
    # not go on accepting what the device will refuse.
    def subscriptions(self) -> list[str]:
        # Both spellings. The portal publishes every command under the
        # lowercase hostname (decision #21: `relay.rs` lowercases the topic).
        # The fixture names are lowercase too, so the pair collapses to one
        # topic; the second spelling stays for a `hostname_override` that is
        # not lowercase.
        topics = []
        for suffix in ("pair/request", "commands/config/request",
                       "commands/config/confirm", "mock/control",
                       "config/sync/hint", "config/sync/resp"):
            for topic in (self._t(suffix), f"{self.prefix}/{self.dev.hostname.lower()}/{suffix}"):
                if topic not in topics:
                    topics.append(topic)
        return topics

    def route(self, topic: str, raw: bytes):
        try:
            payload = json.loads(raw.decode())
        except Exception as e:
            print(f"[{self.dev.hostname}] {topic}: invalid JSON ({e})")
            return
        try:
            if topic.endswith("pair/request"):
                self.handle_pair_request(payload)
            elif topic.endswith("commands/config/request"):
                self.handle_config_request(payload)
            elif topic.endswith("commands/config/confirm"):
                self.handle_config_confirm(payload)
            elif topic.endswith("mock/control"):
                self.handle_mock_control(payload)
            elif topic.endswith("config/sync/hint"):
                self.sync_request()
            elif topic.endswith("config/sync/resp"):
                self.handle_sync_response(payload)
        except Exception as e:
            print(f"[{self.dev.hostname}] handler for {topic} crashed: {e!r}")

    # ----- config sync stub (config_sync.md §5) -----

    def sync_request(self):
        """Ask the portal for the desired document (boot, reconnect, hint)."""
        body = {"gen": self.sync_gen, "hash": self.sync_hash, "boot_id": self.boot_id}
        self.client.publish(self._t("config/sync/req"), json.dumps(body), qos=1)

    def handle_sync_response(self, payload: dict):
        """A stub applier: it neither verifies nor applies the items, it
        takes the document's gen/hash and acks every key as applied."""
        if payload.get("status") != "document":
            return
        self.sync_gen = payload.get("gen", 0)
        self.sync_hash = payload.get("hash", "")
        results = [{"key": i.get("key"), "ok": True} for i in payload.get("items", [])]
        ack = {"gen": self.sync_gen, "hash": self.sync_hash, "results": results}
        self.client.publish(self._t("config/sync/ack"), json.dumps(ack), qos=1)
        print(f"[{self.dev.hostname}] config sync: gen {self.sync_gen}, {len(results)} items acked")

    # ----- mock-only control channel (dev loop, not part of mqtt.md) -----

    def handle_mock_control(self, payload: dict):
        """`{prefix}/{host}/mock/control` — a back door for the dev loop, so
        a scenario the fleet reaches on its own only by luck can be reached
        on demand. Same spirit as dev/alarm.sh forcing a probe transition;
        `dev/node.sh` is the front end. Nothing here exists on a real Hub,
        and the portal neither publishes nor subscribes to this topic.

        `{"action": "silence"|"resume", "dev_eui": ...}` stops/starts one
        Node's uplinks (B3's per-Node offline→online trigger); `{"action":
        "history", "dev_eui": ..., "mode": "ok"|"silent"|
        "history_unavailable"}` picks how it answers get_sticker_history.

        B4 adds `{"action": "join", "dev_eui": ...}`: the first uplink of a
        Node the portal has just registered. `add_lorawan_sticker` leaves a
        new Sticker in `config/state` and out of every telemetry frame,
        which is what devices.node.md §5's "registered — waiting for join"
        looks like on the wire; this is the event that ends it.

        B5 adds the Beacon half, keyed by `mac` instead of `dev_eui`:
        `eye_silence`/`eye_resume` drop a tag out of `eye/sensors` and put
        it back (a BLE broadcast has no "disabled" flag — going quiet *is*
        vanishing from the snapshot), and `eye_rssi` moves one between the
        signal bands `ui.signal_quality` maps. `dev/beacon.sh` is the front
        end.

        B6 adds the six the reconcile rules need:
        `eye_vanish`/`eye_appear` take one tag out of the snapshot and put
        it back — for a discovered-only tag that is exactly the prune
        instruction; `eye_stale` flips the WIRE flag while the tag stays
        listed, which must NOT prune it; `eye_empty` publishes `tags: []`,
        the empty snapshot that prunes every stranger and keeps every
        enrolled Beacon; `eye_capped` flips `discovered_capped`, under which
        nothing is pruned at all; and `eye_provisioning` drives one tag
        through the five wire states.
        """
        action = str(payload.get("action") or "")
        if action == "list":
            for s in self.dev.stickers:
                print(f"[{self.dev.hostname}] mock/control: {s.dev_eui} "
                      f"enabled={s.enabled} history_mode={s.history_mode}")
            for t in self.dev.eye_tags:
                print(f"[{self.dev.hostname}] mock/control: {t.mac} "
                      f"enabled={t.enabled} rssi={t.rssi}")
            return
        if action in (
            "eye_silence", "eye_resume", "eye_rssi",
            "eye_vanish", "eye_appear", "eye_stale", "eye_empty",
            "eye_capped", "eye_provisioning",
        ):
            self.handle_mock_eye_control(action, payload)
            return
        if action in ("hub_offline", "hub_online", "hub_standby", "gateway_stale"):
            self.handle_mock_hub_control(action, payload)
            return
        s = self._sticker_by_eui(payload.get("dev_eui"))
        if s is None:
            print(f"[{self.dev.hostname}] mock/control: unknown dev_eui "
                  f"{payload.get('dev_eui')!r} — ignored")
            return
        if action == "join":
            # B4: the first uplink of a Node registered through the portal.
            # `add_lorawan_sticker` leaves it `enabled=False` — in the
            # inventory, absent from every telemetry frame, which is exactly
            # what "registered — waiting for join" is. This is the join.
            if s.enabled:
                print(f"[{self.dev.hostname}] mock/control: {s.dev_eui} has already joined")
                return
            s.enabled = True
            print(f"[{self.dev.hostname}] mock/control: {s.dev_eui} joined — first uplink")
            publish_config_state(self.client, self.dev, self.prefix)
            publish_lorawan_sensors(self.client, self.dev, self.prefix, self.history_depth_s)
            return
        if action in ("silence", "resume"):
            s.enabled = action == "resume"
            print(f"[{self.dev.hostname}] mock/control: {s.dev_eui} "
                  f"{'resumed' if s.enabled else 'silenced'}")
            # config/state carries `enabled` per Node, so the portal sees
            # the flip on its own terms too, not only as missing telemetry.
            publish_config_state(self.client, self.dev, self.prefix)
            if s.enabled:
                # One immediate uplink, so the recovery edge (and B3's
                # auto-backfill) lands now instead of on the next tick.
                publish_lorawan_sensors(self.client, self.dev, self.prefix,
                                        self.history_depth_s)
        elif action == "history":
            mode = str(payload.get("mode") or "ok")
            if mode not in ("ok", "silent", "history_unavailable"):
                print(f"[{self.dev.hostname}] mock/control: bad history mode {mode!r}")
                return
            s.history_mode = mode
            print(f"[{self.dev.hostname}] mock/control: {s.dev_eui} history_mode={mode}")
        else:
            print(f"[{self.dev.hostname}] mock/control: unknown action {action!r}")

    def handle_mock_hub_control(self, action: str, payload: dict):
        """The Hub half of `mock/control` (demo cluster). What a whole unit
        going quiet, parking in standby, or losing a concentrator's heartbeat
        looks like from the portal — states a three-Hub fleet reaches on its
        own only by accident. `dev/hub.sh` is the front end."""
        dev = self.dev
        if action in ("hub_offline", "hub_online"):
            dev.offline_forced = action == "hub_offline"
            if dev.offline_forced:
                # The same retained `offline` the LWT would publish, without
                # killing the container: the portal marks the Hub down and
                # every list it appears on says so.
                publish_status(self.client, dev, self.prefix, online=False)
                print(f"[{dev.hostname}] mock/control: hub offline")
            else:
                publish_status(self.client, dev, self.prefix, online=True)
                publish_config_state(self.client, dev, self.prefix)
                publish_system_info(self.client, dev, self.prefix)
                print(f"[{dev.hostname}] mock/control: hub back online")
            return
        if action == "hub_standby":
            dev.standby = not dev.standby
            publish_standby(self.client, dev, self.prefix)
            print(f"[{dev.hostname}] mock/control: standby={dev.standby}")
            return
        # gateway_stale — only the server publishes radio/gateways, so only
        # the server can age a row in it.
        host = str(payload.get("hostname") or "").upper() or dev.hostname
        if host in dev.stale_gateway_hosts:
            dev.stale_gateway_hosts.discard(host)
        else:
            dev.stale_gateway_hosts.add(host)
        print(f"[{dev.hostname}] mock/control: stale gateways = "
              f"{sorted(dev.stale_gateway_hosts) or 'none'}")
        if dev.gateway_members:
            publish_radio_gateways(self.client, dev, dev.radio_api_status, dev.gateway_members)

    def handle_mock_eye_control(self, action: str, payload: dict):
        """The Beacon half of `mock/control` (B5). Defaults to the first
        registered tag so `dev/beacon.sh silence` needs no MAC."""
        # Two of the six are about the SNAPSHOT, not about one tag.
        if action == "eye_empty":
            # `tags: []` — every discovered-only beacon pruned, every
            # enrolled one kept. Published once and then undone, because the
            # telemetry loop's next frame is the real fleet again.
            for t in self.dev.eye_tags:
                t.present = False
            publish_eye_sensors(self.client, self.dev, self.prefix)
            for t in self.dev.eye_tags:
                t.present = True
            print(f"[{self.dev.hostname}] mock/control: published an empty eye/sensors snapshot")
            return
        if action == "eye_capped":
            self.dev.eye_discovered_capped = not self.dev.eye_discovered_capped
            print(f"[{self.dev.hostname}] mock/control: "
                  f"discovered_capped={self.dev.eye_discovered_capped}")
            publish_eye_sensors(self.client, self.dev, self.prefix)
            return

        mac = str(payload.get("mac") or "").upper()
        tag = next((t for t in self.dev.eye_tags if t.mac == mac), None)
        if tag is None and not mac:
            # `eye_vanish`/`eye_appear`/`eye_stale` are about strangers as
            # often as about enrolled tags, so with no MAC they default to
            # the first DISCOVERED tag; everything else keeps B5's default of
            # the first registered one.
            if action in ("eye_vanish", "eye_appear", "eye_stale"):
                tag = next((t for t in self.dev.eye_tags if t.discovered), None)
            if tag is None:
                tag = next((t for t in self.dev.eye_tags if not t.discovered), None)
        if tag is None:
            print(f"[{self.dev.hostname}] mock/control: unknown mac {mac!r} — ignored")
            return
        if action in ("eye_silence", "eye_resume"):
            tag.enabled = action == "eye_resume"
            print(f"[{self.dev.hostname}] mock/control: {tag.mac} "
                  f"{'resumed' if tag.enabled else 'silenced'}")
            if tag.enabled:
                # One immediate advertisement, so the recovery edge lands
                # now rather than up to one eye-interval later.
                publish_eye_sensors(self.client, self.dev, self.prefix)
        elif action in ("eye_vanish", "eye_appear"):
            # B6: membership. A discovered-only tag that leaves the array is
            # gone, whether or not it was ever flagged stale.
            tag.present = action == "eye_appear"
            print(f"[{self.dev.hostname}] mock/control: {tag.mac} "
                  f"{'back in' if tag.present else 'out of'} the snapshot")
            publish_eye_sensors(self.client, self.dev, self.prefix)
        elif action == "eye_stale":
            # ...and the flag, which is a different statement: still listed,
            # no longer heard. The portal keeps the row and marks it
            # disconnected.
            tag.stale = not tag.stale
            print(f"[{self.dev.hostname}] mock/control: {tag.mac} stale={tag.stale}")
            publish_eye_sensors(self.client, self.dev, self.prefix)
        elif action == "eye_provisioning":
            state = payload.get("state")
            allowed = ("pending", "provisioning", "provisioned", "failed", "read_only")
            if state not in allowed:
                print(f"[{self.dev.hostname}] mock/control: bad provisioning state "
                      f"{state!r} — one of {allowed}")
                return
            tag.provisioning = state
            print(f"[{self.dev.hostname}] mock/control: {tag.mac} provisioning={state}")
            publish_eye_sensors(self.client, self.dev, self.prefix)
        elif action == "eye_rssi":
            try:
                tag.rssi = int(payload.get("rssi"))
            except (TypeError, ValueError):
                print(f"[{self.dev.hostname}] mock/control: bad rssi {payload.get('rssi')!r}")
                return
            print(f"[{self.dev.hostname}] mock/control: {tag.mac} rssi={tag.rssi}")
            publish_eye_sensors(self.client, self.dev, self.prefix)

    # ----- unsigned Node commands (M5-D) -----

    def _sticker_by_eui(self, dev_eui) -> Optional[Sticker]:
        eui = str(dev_eui or "").lower()
        return next((s for s in self.dev.stickers if s.dev_eui == eui), None)

    # ----- crypto helpers -----

    def make_admin_certificate(self, signer_id: str) -> dict:
        """Self-issued admin certificate: the device CA certifies its own key
        as the admin signer (v1's sign_device_command model). 8 fields +
        certificate_signature over their sorted-key compact JSON."""
        now = int(time.time())
        cert = {
            "signer_id": signer_id,
            "full_name": "Mock Admin",
            "role": "admin",
            "public_key_ed25519": self.ca_public_hex,
            # Every permission the catalogue names, plus all five `level:*`
            # strings (design.md §4.3). v1 listed seven SHOUTING names that
            # matched nothing the portal mints — harmless while nobody checked
            # permissions, and a guaranteed refusal now that step 8 does. The
            # list is derived from the fixture rather than typed out, so a new
            # verb cannot leave this certificate behind.
            "permissions": sorted(
                {c["permission"] for c in CATALOGUE["commands"]}
                | set(LEVEL_PERMISSIONS.values())
            ),
            # RFC3339 strings, as `crypto/certificate.rs` declares them and
            # as `vectors/certificate.json` carries them. Until the 2026-09-15
            # second review these were `int(time.time())` — a certificate no
            # Hub would parse, minted into every `pair/response`, so a portal
            # paired against this mock held a certificate the device would
            # refuse at `parse_certificate` before the envelope was even read.
            # `--selftest` now replays the fixture certificate so this cannot
            # drift back.
            "issued_at": _rfc3339(now),
            "expires_at": _rfc3339(now + 10 * 365 * 86400),
            "issuer": "device_ca",
        }
        cert["certificate_signature"] = _b64(self.ca_key.sign(_canonical(cert)))
        return cert

    def _verify_certificate(self, cert: dict) -> Optional[Ed25519PublicKey]:
        """Check certificate_signature chains to our CA; return the signer's
        embedded public key on success, None (with loud log) on failure."""
        if not isinstance(cert, dict) or "certificate_signature" not in cert:
            print(f"[{self.dev.hostname}] SIGCHECK FAIL: certificate missing or malformed: {cert!r}")
            return None
        pre_image = {k: v for k, v in cert.items() if k != "certificate_signature"}
        message = _canonical(pre_image)
        try:
            self.ca_key.public_key().verify(base64.b64decode(cert["certificate_signature"]), message)
        except (InvalidSignature, ValueError):
            print(
                f"[{self.dev.hostname}] SIGCHECK FAIL: certificate_signature does not chain to CA {self.ca_public_hex}\n"
                f"  expected message bytes (hex): {message.hex()}\n"
                f"  received signature (b64):     {cert.get('certificate_signature')}"
            )
            return None
        try:
            return Ed25519PublicKey.from_public_bytes(bytes.fromhex(cert["public_key_ed25519"]))
        except (KeyError, ValueError) as e:
            print(f"[{self.dev.hostname}] SIGCHECK FAIL: bad certificate public_key_ed25519: {e}")
            return None

    def _check_envelope(self, payload: dict, kind: str) -> str:
        """Steps 1, 3 and 4 of design.md §3.3 — everything that happens
        **before** any crypto, and in that order.

        Returns the level. Raises [`Refused`] with the device's own wording.
        An unknown verb is refused here, which is what makes the mock's
        fall-through *fail*: v1's `_apply` ended in "accepted (no-op)", so
        the mock answered SUCCESS to `add_eye_tag` — a verb the device has
        rejected since the rename — and the dev loop reported a green run
        for a command no Hub would execute."""
        # 1. parse: `v` is a required integer equal to 2 — judged first and
        #    alone, whatever else the frame carries (design.md §3.2, amended
        #    2026-09-15). The device appends `, got <json>` when a value was
        #    present; `_json_text` renders it the way `serde_json::Value`'s
        #    `Display` does, so the two strings are byte-identical.
        v = payload.get("v")
        if v is None:
            raise Refused.unsupported_envelope()
        if isinstance(v, bool) or not isinstance(v, int) or v != ENVELOPE_VERSION:
            raise Refused.unsupported_envelope(_json_text(v))

        # 1. parse: once the frame *is* v2, `level` is a required string from
        #    the enum, and anything else is `InvalidCommand` — the envelope is
        #    fine, the claim inside it is not. Wording is the device's
        #    (`subscriber.rs::parse_envelope`).
        level = payload.get("level")
        if level is None:
            raise Refused.invalid_command("unknown level: <missing>")
        if not isinstance(level, str):
            raise Refused.invalid_command(f"unknown level: {_json_text(level)}")
        if level not in LEVEL_PERMISSIONS:
            raise Refused.invalid_command(f"unknown level: {level}")

        # 1. parse: `reason` is a required string; "" is the absent value and
        #    `null` is refused, so the v1 asymmetry (device canonicalized a
        #    missing reason as null, portal always sent "") cannot come back.
        if kind == "config_request" and not isinstance(payload.get("reason"), str):
            raise Refused.invalid_command("reason must be a string (\"\" when absent)")

        # 1. parse: the signature must be attributable to the certificate.
        cert = payload.get("certificate")
        if isinstance(cert, dict) and cert.get("signer_id") != payload.get("signer_id"):
            raise Refused.invalid_command("signer_id does not match the certificate")

        if kind == "config_confirm":
            return level

        # 3. command_type -> (permission, expected level), before any crypto.
        command_type = payload.get("command_type")
        spec = COMMAND_SPECS.get(command_type)
        if spec is None:
            raise Refused.unknown_command(command_type)

        # 4. The signed claim must be the level the table says. The signature
        #    binds the level; the table decides what it must be, so nobody can
        #    relabel `sticker_device_reset` as `read` to dodge step 8.
        if level != spec["level"]:
            raise Refused.invalid_command(
                f"level mismatch: {command_type} is {spec['level']}, request claims {level}"
            )
        return level

    def _verify_signed(self, payload: dict, pre_image: dict, kind: str,
                       required_permissions: tuple[str, ...] = ()) -> None:
        """Steps 5–10 of design.md §3.3: CA chain and expiry, timestamp,
        nonce, permissions, signature, record nonce. Raises [`Refused`].

        The pre-image reconstruction is the independent cross-check of the
        signer's canonicalization — a failure there is a FINDING, and is
        logged as loudly as it was in v1."""
        # 5. CA chain and certificate expiry.
        cert = payload.get("certificate")
        if cert is not None:
            signer_key = self._verify_certificate(cert)
            if signer_key is None:
                raise Refused.signature("certificate does not chain to the device CA")
            # `crypto/certificate.rs::is_expired` parses RFC3339 and treats an
            # unparseable value as **expired** — fail closed. So does this,
            # since the second review; before it, only an `int` was checked,
            # which the device never accepts in the first place.
            expires_at = cert.get("expires_at")
            expires = _parse_rfc3339(expires_at)
            if expires is None or expires < time.time():
                raise Refused.signature(f"CertificateExpired({expires_at!r})")
        else:
            # Device-key confirms may omit the certificate (mqtt.md) — the
            # only key we can then attribute is our own CA/admin key.
            signer_key = self.ca_key.public_key()

        # 6. timestamp within the window.
        now = int(time.time())
        ts = pre_image.get("timestamp")
        if not isinstance(ts, int) or abs(now - ts) > TIMESTAMP_WINDOW_S:
            raise Refused.signature(
                f"TimestampOutOfRange {{ timestamp: {ts}, drift_sec: {TIMESTAMP_WINDOW_S} }}"
            )

        # 7. nonce unused. Checked before the signature and recorded after it,
        #    exactly as `verification.rs` orders them.
        signer = str(pre_image.get("signer_id", "")).lower()
        nonce = pre_image.get("nonce")
        with self.lock:
            if nonce in self.used_nonces.setdefault(signer, set()):
                raise Refused.signature(f'NonceAlreadyUsed("{nonce}")')

        # 8. The certificate carries the verb's permission AND `level:<level>`
        #    (design.md §4.3). Exact string match against the list, as the
        #    device does — no wildcard, so the bootstrap `["*"]` certificate
        #    stays unusable on the signed path.
        held = set(cert.get("permissions") or []) if isinstance(cert, dict) else set()
        for permission in required_permissions:
            if permission not in held:
                raise Refused.signature(
                    f'PermissionDenied {{ signer_id: "{signer}", '
                    f'required_permission: "{permission}" }}'
                )

        # 9. signature over the canonical pre-image.
        message = _canonical(pre_image)
        signature_b64 = payload.get("signature", "")
        try:
            signer_key.verify(base64.b64decode(signature_b64), message)
        except (InvalidSignature, ValueError):
            print(
                f"[{self.dev.hostname}] SIGCHECK FAIL ({kind}) — CANONICALIZATION FINDING CANDIDATE:\n"
                f"  signer_id: {pre_image.get('signer_id')}\n"
                f"  expected message bytes (hex): {message.hex()}\n"
                f"  expected message (utf-8):     {message.decode()}\n"
                f"  received signature (b64):     {signature_b64}"
            )
            raise Refused.signature("InvalidSignature")

        # 10. record nonce.
        with self.lock:
            self.used_nonces.setdefault(signer, set()).add(nonce)

    def _publish_error(self, command: str, refusal: "Refused") -> None:
        command = refusal.command or command
        self.client.publish(
            self._t("errors"),
            json.dumps({"timestamp": _ts(), "command": command,
                        "error": refusal.error, "message": refusal.message}),
            qos=1, retain=False,
        )
        print(f"[{self.dev.hostname}] REFUSED {command}: {refusal.error} — {refusal.message}")

    # ----- pairing -----

    def handle_pair_request(self, payload: dict):
        request_id = payload.get("request_id")
        # Same rule the real agent applies (control-core/src/libs/pairing/mod.rs):
        # a "local" request encrypts under the shared constant, everything else
        # under the code on the display.
        local = bool(payload.get("local"))
        password = LOCAL_PAIRING_PASSWORD if local else self.pairing_code
        mode = "local (no code)" if local else f"remote (code {self.pairing_code})"
        print(f"[{self.dev.hostname}] pair/request from {payload.get('admin_username')!r} "
              f"(request_id={request_id}, mode={mode})")
        salt, nonce = os.urandom(16), os.urandom(12)
        kdf = PBKDF2HMAC(algorithm=hashes.SHA256(), length=32, salt=salt, iterations=PAIR_KDF_ITERATIONS)
        key = kdf.derive(password.encode())
        ciphertext = AESGCM(key).encrypt(nonce, self.ca_seed, None)  # raw 32-byte seed, no AAD
        response = {
            "request_id": request_id,
            "success": True,
            "encrypted_private_key": {"ciphertext": _b64(ciphertext), "salt": _b64(salt), "nonce": _b64(nonce)},
            "ca_public_key": self.ca_public_hex,
            "ca_id": "mock-ca-1",
            "admin_certificate": self.make_admin_certificate(payload.get("admin_username") or "mock-admin"),
        }
        self.client.publish(self._t("pair/response"), json.dumps(response), qos=2, retain=False)
        print(f"[{self.dev.hostname}] pair/response sent (encrypted CA seed, {mode})")

    # ----- config_request -> challenge -----

    def handle_config_request(self, payload: dict):
        pre_image = request_preimage(payload)
        command_type = payload.get("command_type")
        print(f"[{self.dev.hostname}] config/request {command_type} "
              f"level={payload.get('level')} (request_id={pre_image['request_id']})")

        try:
            level = self._check_envelope(payload, "config_request")
            spec = COMMAND_SPECS[command_type]
            self._verify_signed(
                payload, pre_image, "config_request",
                required_permissions=(spec["permission"], LEVEL_PERMISSIONS[level]),
            )
        except Refused as refusal:
            self._publish_error("config_request", refusal)
            return

        request_id = pre_image["request_id"]
        params = pre_image["params"] or {}

        # Step 11: branch on the level (design.md §5.1). `read` and `operate`
        # are single-shot — no challenge is minted, no slot is held, and the
        # 10-concurrent cap does not apply to them. The `challenge_id` still
        # exists, purely as the audit/correlation key the response is keyed
        # by, so the trail reads the same for every verb.
        if spec["single_shot"]:
            challenge_id = str(uuid.uuid4())
            # Execute, *then* publish the response carrying the real result
            # (design.md §5.1 as amended 2026-09-15): a SUCCESS published
            # before a failed execution is exactly the defect the confirm
            # path had already fixed. `_apply` also emits the verb's own data
            # on its existing topic, so a `read` produces two device
            # publishes — the data and this correlation response.
            try:
                message = self._apply(command_type, params)
            except Exception as e:
                self._respond(challenge_id, request_id, "ERROR", f"mock: apply failed: {e}")
                return
            self._respond(challenge_id, request_id, "SUCCESS", message)
            return

        # `manage` and above keep challenge/confirm.
        now = int(time.time())
        with self.lock:
            # Sweep before counting, so an expired slot is not a held one.
            self.challenges = {
                cid: c for cid, c in self.challenges.items() if c["expires_at"] > now
            }
            if len(self.challenges) >= MAX_CHALLENGES:
                self._publish_error(
                    "config_request",
                    Refused("authorization_failed",
                            f"RegistryFull: {MAX_CHALLENGES} concurrent challenges"),
                )
                return
            challenge_id = str(uuid.uuid4())
            # `expires_at` is the **request's** timestamp plus the TTL, not
            # now plus the TTL — the device computes it that way, so a
            # request signed 59 s in the past gets a 241 s challenge. Copied
            # rather than corrected: design.md §13 item 5 hands the skew to
            # `control.md`, and a mock that quietly did the right thing would
            # hide it.
            expires_at = int(pre_image["timestamp"]) + CHALLENGE_TTL_S
            self.challenges[challenge_id] = {
                "request_id": request_id,
                "signer_id": pre_image["signer_id"],
                "command_type": command_type,
                "params": params,
                "level": level,
                "expires_at": expires_at,
            }
        challenge = {
            "challenge_id": challenge_id,
            "request_id": request_id,
            "signer_id": pre_image["signer_id"],
            "expires_at": expires_at,
            "preview": {"command_type": command_type, "params": params},
        }
        body = json.dumps(challenge)
        self.client.publish(self._t("config/challenge"), body, qos=2, retain=False)
        print(f"[{self.dev.hostname}] challenge issued: {challenge_id} (level={level})")
        if self.inject_duplicate_challenge:
            # The redelivery rumqttc performs on reconnect, on demand: the
            # portal must answer the pair with exactly one confirm
            # (design.md §6.2's seen-set). Without this the case is only
            # reachable by killing a broker at the right microsecond.
            self.client.publish(self._t("config/challenge"), body, qos=2, retain=False)
            print(f"[{self.dev.hostname}] challenge {challenge_id} redelivered "
                  f"(--inject-duplicate-challenge)")

    # ----- config_confirm -> apply -> response -----

    def _respond(self, challenge_id, request_id, status: str, message: str,
                 result: Optional[dict] = None):
        now = int(time.time())
        response = {
            "challenge_id": challenge_id,
            "request_id": request_id,
            "status": status,
            "applied_at": now if status == "SUCCESS" else None,
            "effective_at": now if status == "SUCCESS" else None,
            "message": message,
        }
        # C2: published only by the verbs that have one, SUCCESS or ERROR alike.
        if result is not None:
            response["result"] = result
        self.client.publish(self._t("config/response"), json.dumps(response), qos=2, retain=False)
        # Never the result itself: it can hold a broker password.
        print(f"[{self.dev.hostname}] config/response {status}: {message}"
              + (f" (result keys: {sorted(result)})" if result else ""))

    # ----- cluster-client-add / -remove (plan/control/cluster.md §7, C2) -----

    def _cluster(self, command_type: str, params: dict, request_id: Optional[str] = None):
        """The Server half, as C2's agent answers it: two effects, each
        reported, SUCCESS only when the account is ok and the gateway is not
        failed, and the minted password in `result` exactly when the account
        is ok -- sealed to the request's signed `reply_key`, never in clear
        (`seal_password`). A re-run without an EUI keeps the gateway the
        account has; this Hub's own gateway EUI is refused. `(status,
        message, result)`.

        Nothing is minted for real: the mock keeps a dict of the accounts it
        "holds", so a remove of an unknown Client still answers like the
        idempotent agent does."""
        raw = params.get("hostname") or ""
        hostname = raw.strip().lower()
        if not re.fullmatch(r"proximos-[0-9a-f]{8}", hostname):
            raise ValueError(f"hostname {raw!r} is not a Hub hostname")
        username = f"peer-{hostname}"
        account = os.environ.get("MOCK_CLUSTER_ACCOUNT", "ok")
        gateway_wanted = os.environ.get("MOCK_CLUSTER_GATEWAY", "ok")
        accounts = getattr(self, "cluster_accounts", {})
        self.cluster_accounts = accounts

        if command_type == "cluster-client-remove":
            eui = accounts.pop(hostname, None) if account == "ok" else accounts.get(hostname)
            gateway = "skipped" if eui is None else gateway_wanted
            result = {"hostname": hostname, "username": username, "gateway_eui": eui,
                      "account": account, "gateway": gateway,
                      "message": f"account {username} {'removed' if account == 'ok' else 'failed'}; "
                                 f"gateway {gateway}"}
        else:
            reply_key = params.get("reply_key")
            if not reply_key:
                raise ValueError("Missing reply_key: cluster-client-add seals the new password to it")
            eui = params.get("gateway_eui")
            eui = re.sub(r"[:\-\s]", "", eui).lower() if eui else None
            if eui is not None and not re.fullmatch(r"[0-9a-f]{16}", eui):
                raise ValueError(f"gateway_eui {params.get('gateway_eui')!r} is not a gateway EUI")
            if eui is not None and self.dev.gateway_present and eui == self.dev.gateway_eui:
                raise ValueError(f"gateway_eui {eui} is this Hub's own concentrator")
            existing = accounts.get(hostname)
            if eui is None:
                # C2: a re-run without an EUI keeps what the account has.
                gateway = "skipped"
                note = (f"gateway {existing} kept as registered" if existing
                        else "gateway skipped: no gateway EUI given")
                eui = existing
            elif self.dev.radio_mode not in ("lorawan", "dual"):
                gateway, note = "skipped", "gateway skipped: this Hub's radio.mode runs no LoRaWAN"
                eui = existing
            else:
                gateway, note = gateway_wanted, f"gateway {eui} {gateway_wanted}"
                if existing and existing != eui and account == "ok":
                    note += f"; old gateway {existing} deregistered"
            sealed = None
            if account == "ok":
                accounts[hostname] = eui
                sealed = seal_password(secrets.token_urlsafe(16), reply_key,
                                       request_id or "", hostname)
            result = {"hostname": hostname, "username": username, "gateway_eui": eui,
                      "account": account, "gateway": gateway, "password_sealed": sealed,
                      "message": f"account {username} {'minted' if account == 'ok' else 'failed'}; {note}"}
        if result["account"] == "ok" and result["gateway"] != "failed":
            return "SUCCESS", result["message"], result
        return ("ERROR", f"Execution failed: registration incomplete: {result['message']}", result)

    def handle_config_confirm(self, payload: dict):
        pre_image = confirm_preimage(payload)
        challenge_id = pre_image["challenge_id"]
        with self.lock:
            pending = self.challenges.get(challenge_id)
        request_id = pending["request_id"] if pending else None

        try:
            level = self._check_envelope(payload, "config_confirm")
            # The confirm's level must equal the challenge's — the level the
            # device recorded off the *request*. A confirm cannot escalate.
            if pending is not None and level != pending["level"]:
                raise Refused.invalid_command(
                    f"level mismatch: challenge is {pending['level']}, confirm claims {level}"
                )
            self._verify_signed(
                payload, pre_image, "config_confirm",
                required_permissions=(LEVEL_PERMISSIONS[level],),
            )
        except Refused as refusal:
            # The device answers a refused confirm on `errors` and **nothing
            # else** — `monitor.rs`'s `Err` arm publishes no `config/response`.
            # Until the 2026-09-15 second review the mock also answered
            # `config/response` ERROR here, which resolved the portal's
            # pending entry instantly where a Hub leaves it to the 20 s
            # timeout: the exact failure the relay's `dispatch` split was
            # about, made invisible by the mock. One document, like the device.
            self._publish_error("config_confirm", refusal)
            return

        if pending is None:
            # `AuthError::ChallengeNotFound`, `Display`ed and published as
            # `authorization_failed` — `errors` only, as above.
            self._publish_error(
                "config_confirm",
                Refused("authorization_failed", f"Challenge not found: {challenge_id}"),
            )
            return
        with self.lock:
            self.challenges.pop(challenge_id, None)
        if int(time.time()) > pending["expires_at"]:
            self._publish_error(
                "config_confirm",
                Refused("authorization_failed", f"Challenge expired: {challenge_id}"),
            )
            return
        if pre_image["confirmation"] != "APPROVED":
            # A signed REJECTED is not a refusal: the device answers
            # `config/response` with `status: "REJECTED"` and these words.
            self._respond(challenge_id, request_id, "REJECTED",
                          "Configuration rejected by authorized signer")
            return
        if pending["command_type"] in ("cluster-client-add", "cluster-client-remove"):
            # `run_and_answer` runs these two itself on the agent, because the
            # answer carries a result document (C2).
            try:
                status, message, result = self._cluster(pending["command_type"],
                                                        pending["params"] or {},
                                                        request_id)
            except Exception as e:
                self._respond(challenge_id, request_id, "ERROR", f"Execution failed: {e}")
                return
            self._respond(challenge_id, request_id, status, message, result)
            return
        try:
            message = self._apply(pending["command_type"], pending["params"] or {})
        except Exception as e:
            # `run_and_answer`'s wording for an execution that failed after
            # the signature verified.
            self._respond(challenge_id, request_id, "ERROR", f"Execution failed: {e}")
            return
        self._respond(challenge_id, request_id, "SUCCESS", message)

    # ----- apply semantics (phase-1 commands) -----

    def _apply(self, command_type: str, params: dict) -> str:
        dev = self.dev
        if command_type == "set_threshold":
            line = params.get("line")
            s = next((x for x in dev.sensors if x.line == line), None)
            if s is None:
                raise ValueError(f"no sensor line={line}")
            # control-core's shape, on both its paths
            # (authorization/manager.rs build_command_from_challenge,
            # mqtt/monitor.rs build_dev_command): the bounds sit under
            # `params.thresholds`, and a flat body is "Missing thresholds".
            # Until 2026-09-26 this mock read the flat keys and so hid a
            # portal sending a shape the device refuses.
            thresholds = params.get("thresholds")
            if not isinstance(thresholds, dict):
                raise ValueError("Missing thresholds")
            # And as control-core does, a missing or null bound is not
            # "leave it": it becomes 0.0 (alarm_high 100.0) on the device.
            def _bound(key, default=0.0):
                v = thresholds.get(key)
                return default if v is None else float(v)
            s.critical_low = _bound("critical_low")
            s.low_alarm = _bound("alarm_low")
            s.warning_low = _bound("warning_low")
            s.warning_high = _bound("warning_high")
            s.high_alarm = _bound("alarm_high", 100.0)
            s.critical_high = _bound("critical_high")
            s.has_override = True
            publish_config_state(self.client, dev, self.prefix)  # effect observable immediately
            return f"mock: thresholds applied for line {line}"
        if command_type in ("set_sensor_name", "set_sensor_location"):
            line = params.get("line")
            s = next((x for x in dev.sensors if x.line == line), None)
            if s is None:
                raise ValueError(f"no sensor line={line}")
            field = "name" if command_type == "set_sensor_name" else "location"
            value = params.get(field)
            # Empty string clears the field, like the real agent's config write.
            setattr(s, field, value if value else None)
            publish_config_state(self.client, dev, self.prefix)
            return f"mock: {field} applied for line {line}"
        if command_type == "set_system_info_interval":
            interval = int(params.get("interval_seconds"))
            dev.system_info_interval_s = interval
            publish_config_state(self.client, dev, self.prefix)
            return f"mock: system/info interval set to {interval} s"
        if command_type == "restart_application":
            print(f"[{dev.hostname}] restart_application: going offline, back in 3 s")

            def restart():
                publish_status(self.client, dev, self.prefix, online=False)
                time.sleep(3)
                dev.start_ts = time.time()
                publish_config_state(self.client, dev, self.prefix)
                publish_system_info(self.client, dev, self.prefix)
                publish_status(self.client, dev, self.prefix, online=True)
                print(f"[{dev.hostname}] restart complete — back online")

            threading.Timer(0.2, restart).start()  # let the SUCCESS response out first
            return "mock: restarting application"
        if command_type == "power_off":
            print(f"[{dev.hostname}] power_off: going offline and staying off (SIGHUP powers back on)")

            def off():
                dev.powered_off = True
                publish_status(self.client, dev, self.prefix, online=False)

            threading.Timer(0.2, off).start()
            return "mock: powering off"
        if command_type == "factory_reset":
            print(f"[{dev.hostname}] factory_reset (post_action={params.get('post_action')!r}): resetting thresholds")
            for s in dev.sensors:
                s.critical_low = DEFAULT_CRITICAL_LOW
                s.low_alarm = DEFAULT_WARNING_LOW
                s.warning_low = DEFAULT_WARNING_LOW
                s.warning_high = DEFAULT_WARNING_HIGH
                s.high_alarm = DEFAULT_WARNING_HIGH
                s.critical_high = DEFAULT_CRITICAL_HIGH
                s.has_override = False
            publish_config_state(self.client, dev, self.prefix)
            return "mock: factory reset applied (thresholds restored to defaults)"
        if command_type == "set_sticker_config":
            s = self._sticker_by_eui(params.get("dev_eui"))
            if s is None:
                raise ValueError(f"no sticker dev_eui={params.get('dev_eui')!r}")
            config = params.get("config") or {}
            # The Node alarm table travels as one `alarms.alarm_N` key per slot (nodes_phase1
            # WP2, !99); the mock accepts any slot rather than listing sixteen (LE-22).
            unknown = [
                k for k in config
                if k not in s.device_config and not re.fullmatch(r"alarms\.alarm_\d+", k)
            ]
            if unknown:
                raise ValueError(f"unknown config keys: {unknown}")  # -> ERROR response
            s.device_config.update(config)
            publish_sticker_command_result(
                self.client, dev, self.prefix, s, "set_sticker_config", "ok",
                detail="saved" if params.get("save") else "applied",
            )
            publish_sticker_config_pages(self.client, dev, self.prefix, s)
            return f"mock: sticker config applied ({len(config)} keys, save={bool(params.get('save'))})"
        if command_type in ("add_lorawan_sticker", "node-add"):
            # B4/WP4.4. Before this branch the command fell through to
            # "accepted (no-op)" below, which meant the portal's registration
            # dialog reported success and the Node never appeared —
            # indistinguishable, from the browser, from a portal bug.
            #
            # `node-add` is the v2 verb the portal sends (control.radio.md
            # §7): `{radio, profile, serial, dev_eui, app_key, name?}`. The
            # real agent enrols through control_radio; the mock has no
            # ChirpStack, so both verbs end in the same "registered, waiting
            # for join" state. `add_lorawan_sticker` stays for a v1 Hub.
            if command_type == "node-add":
                radio = params.get("radio")
                if radio != "lorawan":
                    raise ValueError(
                        f"node-add --radio {radio}: only the LoRaWAN arm is wired "
                        "(P2P is Phase2NotImplemented)")
                missing = [k for k in ("profile", "serial", "dev_eui", "app_key")
                           if not params.get(k)]
                if missing:
                    raise ValueError(f"node-add: missing {', '.join(missing)}")
            serial_number = params.get("serial") if command_type == "node-add" \
                else params.get("serial_number")
            eui = str(params.get("dev_eui", "")).lower()
            if any(x.dev_eui == eui for x in dev.stickers):
                raise ValueError(f"sticker {eui} already registered")
            sticker = Sticker(
                dev_eui=eui,
                name=params.get("name") or serial_number or eui,
                serial_number=serial_number,
                location=None,
                # `enabled=False` is what "registered — waiting for join"
                # looks like on the wire: the Node is in `config/state` but
                # absent from every `lorawan/sensors` frame, so it has no
                # `last_seen` at all. `dev/node.sh join` flips it, which is
                # the first uplink.
                enabled=False,
                field_thresholds=[
                    FieldThreshold(field="temperature", critical_low=-10.0, warning_low=0.0,
                                   warning_high=8.0, critical_high=12.0),
                    FieldThreshold(field="humidity", critical_low=20.0, warning_low=30.0,
                                   warning_high=75.0, critical_high=90.0),
                ],
            )
            dev.stickers.append(sticker)
            print(f"[{dev.hostname}] {command_type}: {eui} registered, waiting for join "
                  f"(dev/node.sh join {eui})")
            publish_config_state(self.client, dev, self.prefix)
            return f"mock: sticker {eui} registered (waiting for join)"
        if command_type in ("remove_lorawan_sticker", "node-remove"):
            # `node-remove` is the v2 verb (control.radio.md §7); the mock has
            # no ChirpStack to deprovision, so both end the same way.
            eui = str(params.get("dev_eui", "")).lower()
            before = len(dev.stickers)
            dev.stickers = [x for x in dev.stickers if x.dev_eui != eui]
            if len(dev.stickers) == before:
                raise ValueError(f"no sticker dev_eui={eui!r}")
            print(f"[{dev.hostname}] {command_type}: {eui} removed")
            publish_config_state(self.client, dev, self.prefix)
            publish_lorawan_sensors(self.client, dev, self.prefix, self.history_depth_s)
            return f"mock: sticker {eui} removed (ChirpStack deprovision skipped in the mock)"
        if command_type == "sticker_reboot":
            s = self._sticker_by_eui(params.get("dev_eui"))
            if s is None:
                raise ValueError(f"no sticker dev_eui={params.get('dev_eui')!r}")
            # A log line and nothing else: the mock Node has no boot to
            # simulate, and inventing one would make the walk-through prove
            # something that is not true.
            print(f"[{dev.hostname}] sticker_reboot: {s.dev_eui} would reboot now")
            publish_sticker_command_result(
                self.client, dev, self.prefix, s, "sticker_reboot", "ok", detail="rebooting",
            )
            return f"mock: sticker {s.dev_eui} rebooting"
        if command_type == "beacon-add":
            mac = str(params.get("mac", "")).upper()
            existing = next((t for t in dev.eye_tags if t.mac == mac), None)
            if existing:
                existing.discovered = False  # discovered tag promoted to registered
                if params.get("name"):
                    existing.name = params["name"]
                tag = existing
            else:
                tag = EyeTag(mac=mac, name=params.get("name"))
                dev.eye_tags.append(tag)
            # B6: a freshly enrolled Beacon is `pending` until the Hub gets
            # round to writing the profile into it (central.md §5). The
            # portal renders that as "waiting for configuration"; the
            # `beacon-provision` handler below is what moves it on.
            tag.provisioning = "pending"
            publish_config_state(self.client, dev, self.prefix)
            publish_eye_sensors(self.client, dev, self.prefix)
            return f"mock: beacon {mac} added"
        if command_type == "beacon-provision":
            # The reconfiguration request (devices.beacon.md §3.5). It is no
            # longer removal's first half — that pre-step was dropped on
            # 2026-09-15 (design.md §8). Two-phase on a timer, so
            # the walk-through can watch "konfiguruje se…" become normal
            # rather than being told it happened.
            mac = str(params.get("mac", "")).upper()
            tag = next((t for t in dev.eye_tags if t.mac == mac), None)
            if tag is None:
                raise ValueError(f"no eye tag mac={mac}")
            if tag.provisioning == "read_only":
                raise ValueError(f"beacon {mac} refuses configuration writes")
            tag.provisioning = "provisioning"
            publish_eye_sensors(self.client, dev, self.prefix)

            def finish(mac=mac):
                t = next((x for x in dev.eye_tags if x.mac == mac), None)
                if t is None or t.provisioning != "provisioning":
                    return  # removed, or something else moved it on
                t.provisioning = "provisioned"
                publish_eye_sensors(self.client, dev, self.prefix)
                print(f"[{dev.hostname}] beacon-provision: {mac} provisioned")

            threading.Timer(3.0, finish).start()
            return f"mock: beacon {mac} configuring"
        if command_type == "beacon-history":
            mac = str(params.get("mac", "")).upper()
            tag = next((t for t in dev.eye_tags if t.mac == mac), None)
            if tag is None:
                raise ValueError(f"no eye tag mac={mac}")
            if BEACON_HISTORY_MODE == "silent":
                # Accepted and nothing else: no progress, no records, so the
                # job stays pending until the portal's reaper times it out.
                print(f"[{dev.hostname}] beacon-history: {mac} — accepted, silent")
                return f"mock: beacon history readout requested for {mac}"
            threading.Thread(target=publish_beacon_archive, daemon=True, args=(
                self.client, dev, self.prefix, tag, params)).start()
            return f"mock: beacon history readout requested for {mac}"
        if command_type == "beacon-remove":
            mac = str(params.get("mac", "")).upper()
            tag = next((t for t in dev.eye_tags if t.mac == mac), None)
            if tag is not None:
                # Deregistered, NOT gone: the device is a broadcast and keeps
                # advertising, so with auto-discover on it reappears among the
                # discovered ones straight away (devices.beacon.md §5 —
                # "objeví se znovu v zachycených", which is correct and
                # visible behaviour, not a bug). It leaves `eye.tags` and so
                # leaves config/state, which is what tells the portal it is no
                # longer registered.
                tag.discovered = True
                tag.name = None
                tag.provisioning = None
                tag.field_thresholds = []
            publish_config_state(self.client, dev, self.prefix)
            publish_eye_sensors(self.client, dev, self.prefix)
            return f"mock: beacon {mac} removed"
        if command_type == "set_lorawan_field_threshold":
            # The Hub keeps a Node field's band and echoes it in
            # config/state.lorawan_sensors, which the portal reads as the
            # authoritative one. Without this the mock acked the command and
            # kept echoing the old band, so every limit change reverted.
            s = self._sticker_by_eui(params.get("dev_eui"))
            if s is None:
                raise ValueError(f"no sticker dev_eui={params.get('dev_eui')!r}")
            field_name = params.get("field")
            th = next((x for x in s.field_thresholds if x.field == field_name), None)
            if th is None:
                th = FieldThreshold(field=field_name)
                s.field_thresholds.append(th)
            for key in ("critical_low", "warning_low", "warning_high", "critical_high"):
                setattr(th, key, params.get(key))
            publish_config_state(self.client, dev, self.prefix)
            return f"mock: lorawan threshold set for {s.dev_eui}/{field_name}"
        if command_type == "set_eye_field_threshold":
            mac = str(params.get("mac", "")).upper()
            tag = next((t for t in dev.eye_tags if t.mac == mac), None)
            if tag is None:
                raise ValueError(f"no eye tag mac={mac}")
            field_name = params.get("field")
            th = next((x for x in tag.field_thresholds if x.field == field_name), None)
            if th is None:
                th = FieldThreshold(field=field_name)
                tag.field_thresholds.append(th)
            for key in ("critical_low", "warning_low", "warning_high", "critical_high"):
                setattr(th, key, params.get(key))  # nullable = inherit, per mqtt.md
            publish_eye_sensors(self.client, dev, self.prefix)
            return f"mock: eye threshold set for {mac}/{field_name}"
        if command_type == "delete_eye_field_threshold":
            mac = str(params.get("mac", "")).upper()
            tag = next((t for t in dev.eye_tags if t.mac == mac), None)
            if tag is None:
                raise ValueError(f"no eye tag mac={mac}")
            tag.field_thresholds = [x for x in tag.field_thresholds if x.field != params.get("field")]
            publish_eye_sensors(self.client, dev, self.prefix)
            return f"mock: eye threshold deleted for {mac}/{params.get('field')}"
        # --- Unit tab: display, buzzer, label (display-config.md §4) ---
        # The real agent clamps none of these (it casts as_u64() as u8), so
        # neither does the mock: the portal is what validates, and the dev
        # loop should show exactly what a bad value would do on a device.
        SCALARS = {
            "set_buzzer_volume": ("volume", "buzzer_volume"),
            "set_screen_brightness": ("brightness", "screen_brightness"),
            "set_led_brightness": ("brightness", "led_brightness"),
            "set_screen_timeout": ("timeout_secs", "screen_timeout_secs"),
        }
        if command_type in SCALARS:
            param, attr = SCALARS[command_type]
            value = params.get(param)
            if value is None:
                raise ValueError(f"missing {param}")
            setattr(dev, attr, int(value))
            publish_config_state(self.client, dev, self.prefix)
            return f"mock: {attr} set to {int(value)}"
        if command_type == "set_device_label":
            label = params.get("label")
            if not label:
                raise ValueError("missing label")
            dev.label = str(label)
            publish_config_state(self.client, dev, self.prefix)
            publish_system_info(self.client, dev, self.prefix)  # it carries the label too
            return f"mock: device label set to {dev.label!r}"
        if command_type == "set_display_lines":
            lines = params.get("lines")
            if not isinstance(lines, list):
                raise ValueError("lines must be a list")
            if len(lines) > 16:
                raise ValueError(f"at most 16 display lines (got {len(lines)})")
            dev.display_lines = lines
            publish_config_state(self.client, dev, self.prefix)
            return f"mock: {len(lines)} display lines applied"
        # ---- the eight ex-`PLAIN` verbs (design.md §1, §4.2) -------------
        #
        # These arrived on flat `commands/<verb>` topics in v1, unsigned and
        # unauthenticated. They are signed rows now, served from the one path
        # like everything else — the mock's half of "one dialect". Five of
        # them it already answered on the flat topics; `silence_buzzer` is the
        # one that answered nothing at all in v1 and now gets a
        # `config/response` like any other verb.
        if command_type == "get_sticker_config":
            s = self._sticker_by_eui(params.get("dev_eui"))
            if s is None:
                raise ValueError(f"no sticker dev_eui={params.get('dev_eui')!r}")
            publish_sticker_config_pages(self.client, dev, self.prefix, s, keys=params.get("keys"))
            return f"mock: sticker config pages published for {s.dev_eui}"
        if command_type == "get_sticker_info":
            s = self._sticker_by_eui(params.get("dev_eui"))
            if s is None:
                raise ValueError(f"no sticker dev_eui={params.get('dev_eui')!r}")
            publish_sticker_info(self.client, dev, self.prefix, s)
            return f"mock: sticker info published for {s.dev_eui}"
        if command_type == "get_sticker_full_config":
            s = self._sticker_by_eui(params.get("dev_eui"))
            if s is None:
                raise ValueError(f"no sticker dev_eui={params.get('dev_eui')!r}")
            publish_sticker_full_config(self.client, dev, self.prefix, s)
            return f"mock: sticker full config published for {s.dev_eui}"
        if command_type == "get_sticker_history":
            s = self._sticker_by_eui(params.get("dev_eui"))
            if s is None:
                raise ValueError(f"no sticker dev_eui={params.get('dev_eui')!r}")
            publish_sticker_history(self.client, dev, self.prefix, s,
                                    params.get("from"), params.get("to"), self.history_depth_s)
            return f"mock: sticker history replayed for {s.dev_eui}"
        if command_type == "sticker_force_send":
            s = self._sticker_by_eui(params.get("dev_eui"))
            if s is None:
                raise ValueError(f"no sticker dev_eui={params.get('dev_eui')!r}")
            publish_sticker_command_result(self.client, dev, self.prefix, s,
                                           "sticker_force_send", "requested")
            publish_lorawan_sensors(self.client, dev, self.prefix, self.history_depth_s)
            publish_sticker_command_result(self.client, dev, self.prefix, s,
                                           "sticker_force_send", "ok")
            return f"mock: sticker {s.dev_eui} forced an uplink"
        if command_type == "silence_buzzer":
            # `operate`: a transient physical effect that does not survive a
            # reboot, and the reason the verb got a permission of its own
            # (design.md §4.2 ★) — mapping it onto `get_status` would have let
            # a read-only certificate silence a medical alarm.
            print(f"[{dev.hostname}] silence_buzzer: buzzer silenced")
            return "mock: buzzer silenced"
        if command_type == "get_sensor_config":
            publish_config_state(self.client, dev, self.prefix)
            return "mock: sensor config republished on config/state"
        if command_type == "get_interval":
            return (f"mock: aggregation {dev.aggregation_interval_ms}ms, "
                    f"report {dev.report_interval_ms}ms")

        # Any other signed command in the catalogue: acknowledge without side
        # effects. The fall-through is only reachable for a verb that **is** in
        # `catalogue.json` — an unknown one was refused before any crypto, with
        # the device's wording (`_check_envelope`). That is the difference from
        # v1, where this line answered SUCCESS to anything at all and hid the
        # whole `*_eye_*` rename from the dev loop.
        return "mock: accepted (no-op)"


# ============================================================================
# Fixture self-test (design.md §10)
# ============================================================================

def run_selftest() -> bool:
    """Replay every vector through this mock's canonicaliser and verifier.

    Three implementations sign or verify this envelope — `fiber-protocol`,
    `control-core` and this file — and only the first two are in one language.
    The Python half is where they can silently disagree: `json.dumps` defaults
    to `ensure_ascii=True` and would escape a non-ASCII `reason` into `\\uXXXX`
    where `serde_json` emits the character, producing a different pre-image and
    a signature that verifies nowhere. `_canonical` passes `ensure_ascii` by
    omission — the default is wrong — so `non_ascii_reason` is not a decorative
    case, it is the one that fails if anybody 'simplifies' that call.

    Needs no broker: it compares strings and verifies signatures under the
    fixture's own test seed."""
    key = Ed25519PublicKey.from_public_bytes(
        bytes.fromhex(ENVELOPE_VECTORS["signing_key"]["public_key_ed25519"])
    )
    failures = []
    checked = 0

    for kind, builder in (("requests", request_preimage), ("confirms", confirm_preimage)):
        for case in ENVELOPE_VECTORS[kind]:
            name = case["name"]
            envelope = case["envelope"]
            expected = case["canonical"]

            actual = _canonical(builder(envelope)).decode()
            if actual != expected:
                failures.append(
                    f"{kind}/{name}: canonical string differs\n"
                    f"    expected: {expected}\n"
                    f"    actual:   {actual}"
                )
                continue
            try:
                key.verify(base64.b64decode(envelope["signature"]), actual.encode())
            except (InvalidSignature, ValueError) as e:
                failures.append(f"{kind}/{name}: signature does not verify ({e!r})")
                continue
            checked += 1
            print(f"  [OK] {kind}/{name}")

    # The certificate half (design.md §3.4, §10): `certificate.json` pins the
    # eight-field canonical form and a CA signature over it. Until the
    # 2026-09-15 second review this loop did not exist, and the mock minted
    # `issued_at`/`expires_at` as integers where the fixture — and the device
    # — carry RFC3339 strings; nothing here could have noticed. Now the
    # fixture's payload must canonicalize to its own `canonical` string and
    # verify under `ca_public_key_ed25519`, and the mock's own certificate
    # timestamps must round-trip through the same parser the verifier uses.
    cert_vector = _load_vectors("certificate.json")
    cert_payload = cert_vector["payload"]
    cert_actual = _canonical(cert_payload).decode()
    if cert_actual != cert_vector["canonical"]:
        failures.append(
            "certificate: canonical string differs\n"
            f"    expected: {cert_vector['canonical']}\n"
            f"    actual:   {cert_actual}"
        )
    else:
        try:
            Ed25519PublicKey.from_public_bytes(
                bytes.fromhex(cert_vector["ca_public_key_ed25519"])
            ).verify(base64.b64decode(cert_vector["certificate_signature"]), cert_actual.encode())
            checked += 1
            print("  [OK] certificate/fixture")
        except (InvalidSignature, ValueError) as e:
            failures.append(f"certificate: signature does not verify ({e!r})")
    for field_name in ("issued_at", "expires_at"):
        if _parse_rfc3339(cert_payload[field_name]) is None:
            failures.append(f"certificate: fixture {field_name} is not RFC3339 to this parser")
    if _parse_rfc3339(_rfc3339(1_700_000_000)) != 1_700_000_000:
        failures.append("certificate: _rfc3339 does not round-trip through _parse_rfc3339")

    # The sealed reply (cluster.md §7, amended 2026-10-02): given the vector's
    # ephemeral secret and nonce, `seal_password` must produce the vector's
    # seal byte for byte, so the mock seals as the agent does.
    sealed_vector = _load_vectors("sealed_reply.json")
    resealed = seal_password(
        sealed_vector["password"], sealed_vector["reply_key"],
        sealed_vector["request_id"], sealed_vector["hostname"],
        bytes.fromhex(sealed_vector["agent_secret_hex"]),
        bytes.fromhex(sealed_vector["nonce_hex"]),
    )
    if resealed != sealed_vector["sealed"]:
        failures.append(f"sealed_reply: the seal differs\n    expected: "
                        f"{sealed_vector['sealed']}\n    actual:   {resealed}")
    else:
        checked += 1
        print("  [OK] sealed_reply/vector")

    # The catalogue half: every verb the mock could be asked for resolves, and
    # single-shot is exactly `read` + `operate` — the property `handle_config_request`
    # branches on.
    for command_type, spec in COMMAND_SPECS.items():
        expected_single_shot = spec["level"] in ("read", "operate")
        if spec["single_shot"] != expected_single_shot:
            failures.append(
                f"catalogue/{command_type}: single_shot={spec['single_shot']} "
                f"but level={spec['level']}"
            )
    if set(LEVEL_PERMISSIONS) != {"read", "operate", "manage", "admin", "destructive"}:
        failures.append(f"catalogue: unexpected level set {sorted(LEVEL_PERMISSIONS)}")

    print()
    if failures:
        print(f"SELFTEST FAILED — {len(failures)} problem(s):")
        for f in failures:
            print(f"  {f}")
        return False
    print(f"SELFTEST PASSED — {checked} vectors, {len(COMMAND_SPECS)} catalogue rows")
    return True

# ============================================================================
# Main
# ============================================================================

def main():
    parser = argparse.ArgumentParser(description="Mock MQTT publisher for the ProXimos v2 portal dev loop")
    parser.add_argument("--host", default="localhost")
    parser.add_argument("--port", type=int, default=1883)
    parser.add_argument("--prefix", default=TOPIC_PREFIX)
    parser.add_argument("--aggregation-s", type=float, default=DEFAULT_AGGREGATION_S)
    parser.add_argument("--system-info-s", type=float, default=DEFAULT_SYSTEM_INFO_S)
    parser.add_argument("--probe-1m-s", type=float, default=DEFAULT_PROBE_1M_S)
    parser.add_argument("--sticker-interval-s", type=float, default=DEFAULT_STICKER_INTERVAL_S,
                        help="lorawan/sensors aggregate telemetry interval")
    parser.add_argument("--eye-interval-s", type=float, default=DEFAULT_EYE_INTERVAL_S,
                        help="eye/sensors snapshot interval")
    parser.add_argument("--beacon-history", choices=("archive", "silent"), default="archive",
                        help="answer beacon-history with a synthetic archive, or say nothing")
    parser.add_argument("--history-depth-s", type=int, default=DEFAULT_HISTORY_DEPTH_S,
                        help="Node history ring-buffer depth in seconds")
    parser.add_argument("--radio-gateways-s", type=float, default=DEFAULT_RADIO_GATEWAYS_S,
                        help="radio/gateways report interval")
    # Env var rather than a flag: the dev loop runs the mock from docker compose, where an
    # environment entry is the one knob that needs no command rewrite.
    parser.add_argument("--radio-api-status", default=os.environ.get("MOCK_RADIO_API_STATUS", "ok"),
                        choices=RADIO_API_STATUSES,
                        help="api_status in radio/gateways (env MOCK_RADIO_API_STATUS) — "
                             "auth_failed/unreachable drive the card's banner")
    parser.add_argument("--hub", default="server", choices=sorted(PROFILES),
                        help="Which Hub of the demo cluster this process is. 'server' is the "
                             "unit the portal runs on (dev/config.yaml local_hostname) and "
                             "reproduces the single-Hub fleet; the rest are client Hubs.")
    # Both default to the profile's value rather than to a literal, so a
    # second Hub cannot silently share the server's CA identity.
    parser.add_argument("--seed", default=None,
                        help="Deterministic device-CA seed phrase (SHA256 -> 32B Ed25519 seed). "
                             "Defaults to the --hub profile's own seed.")
    parser.add_argument("--pairing-code", default=None,
                        help="Fixed pairing code used as the PBKDF2 password in pair/response. "
                             "Remote pairing only: a request with \"local\": true is encrypted "
                             "under LOCAL_PAIRING_PASSWORD and ignores this value. Defaults to "
                             "the --hub profile's own code.")
    parser.add_argument("--selftest", action="store_true",
                        help="Replay every vector in fiber-protocol/vectors/envelope_v2.json "
                             "through this mock's canonicaliser and verifier, print the result "
                             "and exit. No broker needed. The non-ASCII `reason` case is the one "
                             "that catches Python's ensure_ascii.")
    # Env var as well as a flag, like --radio-api-status: the dev loop runs the mock from
    # docker compose, where an environment entry is the one knob that needs no command rewrite.
    parser.add_argument("--pair-invite", action="store_true",
                        default=os.environ.get("MOCK_PAIR_INVITE", "").lower() in ("1", "true", "yes"),
                        help="Publish one pair/invite on startup (env MOCK_PAIR_INVITE) — the "
                             "device-side half of GitLab #61: the Hub asks the portal on this "
                             "machine to pair, so the operator never needs the LAN. Carries "
                             "--pairing-code and origin=ble. QoS 1, retain=false, exactly once: "
                             "a retained invite is dropped by the portal by design, and the "
                             "portal only accepts one for a hostname it has already heard from, "
                             "so this goes out after the first status.")
    parser.add_argument("--inject-duplicate-challenge", action="store_true",
                        help="Publish every config/challenge twice — the redelivery rumqttc "
                             "performs on reconnect. The portal must answer with exactly one "
                             "confirm (design.md §6.2).")
    args = parser.parse_args()
    global BEACON_HISTORY_MODE
    BEACON_HISTORY_MODE = args.beacon_history

    if args.selftest:
        sys.exit(0 if run_selftest() else 1)

    if mqtt is None:
        sys.exit("paho-mqtt not installed. Run: pip install paho-mqtt "
                 "(not needed for --selftest)")


    profile = PROFILES[args.hub]
    seed = args.seed or profile.seed
    pairing_code = args.pairing_code or profile.pairing_code

    dev = build_device(profile)
    dev.radio_api_status = args.radio_api_status
    if profile.publishes_gateways:
        # The whole cluster's table, published by the one Hub that owns the
        # topic — including Hubs whose own process is not running, which is
        # what a client that has dropped off the broker looks like anyway.
        dev.gateway_members = list(PROFILES.values())
    seed_sticker_history(dev, args.history_depth_s, args.sticker_interval_s)

    client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2,
                         client_id=f"mock-{profile.key}-{random.randint(1000, 9999)}")

    sim = CommandSim(client, dev, args.prefix, seed, pairing_code,
                     history_depth_s=args.history_depth_s,
                     inject_duplicate_challenge=args.inject_duplicate_challenge)

    def on_message(_client, _userdata, msg):
        sim.route(msg.topic, msg.payload)

    client.on_message = on_message

    def on_connect(_client, _userdata, _flags, _reason, _props=None):
        # Subscribe on every (re)connect: after a broker drop (laptop suspend)
        # the clean session has lost them, and commands time out silently.
        for topic in sim.subscriptions():
            _client.subscribe(topic, qos=2)
        # Config sync: a Hub asks on every broker (re)connect.
        sim.sync_request()

    client.on_connect = on_connect

    lwt_topic = f"{args.prefix}/{dev.hostname}/status"
    client.will_set(lwt_topic, json.dumps({"status": "offline", "timestamp": _ts()}), qos=1, retain=True)

    print(f"Connecting to {args.host}:{args.port} ...")
    client.connect(args.host, args.port)
    client.loop_start()

    publish_config_state(client, dev, args.prefix)
    publish_system_info(client, dev, args.prefix)
    publish_standby(client, dev, args.prefix)
    if dev.gateway_members:
        publish_radio_gateways(client, dev, dev.radio_api_status, dev.gateway_members)
    for s in dev.stickers:
        publish_sticker_config_tombstone(client, dev, args.prefix, s)
        publish_sticker_info(client, dev, args.prefix, s)
        publish_node_reports(client, dev, s.dev_eui, s.serial_number, s.report,
                             int(s.voltage * 1000))
    for n in dev.p2p_nodes:
        publish_radio_node_info(client, dev, n)
        publish_node_reports(client, dev, n.dev_eui, n.serial_number, n.report,
                             int(n.voltage * 1000))
    publish_status(client, dev, args.prefix, online=True)
    if args.pair_invite:
        # After the status publish, and on the same connection: the portal refuses an invite
        # for a hostname it has not heard from, and QoS 1 on one client preserves the order.
        publish_pair_invite(client, dev, args.prefix, pairing_code)
    print(f"[{dev.hostname}] online — {len(dev.sensors)} probes, {len(dev.stickers)} stickers, "
          f"{len(dev.p2p_nodes)} p2p nodes, {len(dev.eye_tags)} eye tags")
    print("=" * 64)
    print(f"  HUB          : {profile.key} ({dev.cluster_role}) — {dev.hostname}")
    print(f"  PAIRING CODE : {pairing_code}   (remote pairing only)")
    print(f"  CA public key: {sim.ca_public_hex}")
    print(f"  CA id        : mock-ca-1   (seed phrase: {seed!r})")
    print("=" * 64)

    stop = threading.Event()

    def handle_signal(signum, frame):
        print("\nShutting down ...")
        stop.set()

    def handle_sighup(signum, frame):
        if dev.powered_off:
            print(f"[{dev.hostname}] SIGHUP — powering back on")
            dev.powered_off = False
            publish_config_state(client, dev, args.prefix)
            publish_system_info(client, dev, args.prefix)
            publish_status(client, dev, args.prefix, online=True)

    signal.signal(signal.SIGINT, handle_signal)
    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGHUP, handle_sighup)

    last_aggregated = 0.0
    last_system_info = time.time()
    last_probe_1m = time.time()
    last_sticker = 0.0
    last_eye = 0.0
    last_radio_gateways = time.time()
    discover_at = time.time() + DISCOVERED_TAG_DELAY_S
    discovered_added = False
    while not stop.is_set():
        now = time.time()
        if dev.powered_off or dev.offline_forced:
            time.sleep(0.2)
            continue
        if (now - last_aggregated) >= args.aggregation_s:
            publish_sensors_aggregated(client, dev, args.prefix)
            last_aggregated = now
        if (now - last_system_info) >= args.system_info_s:
            publish_system_info(client, dev, args.prefix)
            last_system_info = now
        if (now - last_probe_1m) >= args.probe_1m_s:
            publish_probe_1m(client, dev, args.prefix)
            last_probe_1m = now
        if (now - last_sticker) >= args.sticker_interval_s:
            publish_lorawan_sensors(client, dev, args.prefix, args.history_depth_s)
            publish_radio_node_readings(client, dev)
            last_sticker = now
        if not discovered_added and now >= discover_at:
            # A second, DISCOVERED-only tag shows up on the air after 30 s —
            # in eye/sensors with discovered=true, never in config/state.
            if not any(t.mac == dev.discovered_mac for t in dev.eye_tags):
                dev.eye_tags.append(EyeTag(mac=dev.discovered_mac, name=None, discovered=True))
                print(f"[{dev.hostname}] eye: discovered-only tag {dev.discovered_mac} now visible")
            discovered_added = True
        if dev.gateway_members and (now - last_radio_gateways) >= args.radio_gateways_s:
            publish_radio_gateways(client, dev, dev.radio_api_status, dev.gateway_members)
            last_radio_gateways = now
        if (now - last_eye) >= args.eye_interval_s:
            publish_eye_sensors(client, dev, args.prefix)
            last_eye = now
        time.sleep(0.2)

    publish_status(client, dev, args.prefix, online=False)
    client.loop_stop()
    client.disconnect()


if __name__ == "__main__":
    main()
