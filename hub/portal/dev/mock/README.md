# Mock MQTT publisher

Origin: trimmed from `proximos-v1/viewer/dev/mock_publisher.py` (copied
2026-08-18). Why: the v2 portal dev harness and the M1 golden-diff harness
both need a device-shaped MQTT stream, and v1's publisher is the executable
spec for those payload shapes (`plan/v1/viewer/mqtt.md`) — reusing it beats
re-deriving the wire format from scratch.

## The demo cluster (`--hub`)

One process is one Hub. `--hub` picks which, out of `PROFILES` in
`mock_publisher.py`; `make multi-up` starts all three at once and
`make mock-up` still starts the server alone.

The portal needed no change for any of this. It subscribes `fiber/+/…` and
keys one `DeviceState` per hostname (`state/models.rs`), so a cluster is a
matter of starting this file more than once. `local_hostname` in
`dev/config.yaml` names the server, the unit the portal runs on, which pairs
without a code; every other hostname pairs remotely with its own.

| `--hub` | hostname | code | what only this one covers |
|---|---|---|---|
| `server` | `fiber-mock-001` | `ABC123` | 8 probes, 3 Nodes (one rich), 3 Beacons, the discovered stranger. Identical to the single-Hub fleet, identifiers included. Publishes the cluster-wide `radio/gateways`. Radio mode `lorawan`. |
| `cold-store` | `proximos-c01d0002` | `DEF456` | A **P2P Hub** (radio mode `p2p`): 3 P2P Nodes on the v2 radio family and no LoRaWAN Node, because a Hub runs one transport. 4 probes, WiFi instead of ethernet. |
| `truck` | `proximos-7a0c0003` | `GHI789` | **No concentrator** (UI-4 "waiting / no radio", radio mode `off`), off DC at 14 % battery and 93 % storage (both past the system-alert thresholds), one probe parked in CRITICAL. |
| `joiner` | `proximos-c5c1e000` | `JKM234` | The **Client Hub of Add hub** (C5): not in `--profile multi`; started with `make joiner-up` once registered. See "The joiner" below. |

Every profile's `system/info` carries `lorawan.mode` and a `hardware` block
(a Compute Module 4 rev 1.1 with a 16-hex CPU serial derived from the
index), so the Hub's description card has something to show.

Both clients also *hear* `EE:E0:00:00:00:01`, the Beacon enrolled on the
server — a stranger in their `eye/sensors`, absent from their
`config/state`, which is all a Hub can honestly say about somebody else's
Beacon. That is the input `render::beacon_hubs` ranks, and one Hub cannot
produce it.

Identity is derived from the profile's `index`, so two instances cannot
collide on a DevEUI, a MAC, a serial or a gateway EUI. Index 0 reproduces
the v1-era literals exactly, which is what `dev/node.sh`, `dev/beacon.sh`,
`dev/alarm.sh` and `e2e_client.py` address by hard-coded id. Those three
scripts default to the server; point them at a client with
`MOCK_HOSTNAME=proximos-c01d0002 dev/node.sh silence 70b3d50100000000`.

`dev/hub.sh` drives a whole Hub: `offline`/`online` (retained `offline`
status, publishing stops, no container killed), `standby`, and
`gateway-stale`, which ages one gateway row past the portal's 300 s window
while the Hub itself stays up.

Two things the cluster makes visible that one Hub hides. `radio/gateways` is
retained on a single topic with no hostname level, so two publishers would
overwrite each other rather than merge: the server publishes the whole
table, the clients publish nothing. And `PortalState::hub_of_beacon` picks
the Hub with the newer `last_seen` when several hold the same MAC, which for
an overheard Beacon is a coin flip between three live Hubs — worth knowing
before a write for that Beacon goes somewhere surprising.

## STICKER firmware 2.0.0 Nodes (!179, !180)

Every Node, LoRaWAN or P2P, also publishes the two retained documents of
the v2 radio family a 2.0.0 Node produces at boot:
`proximos/{host}/nodes/{dev_eui}/device_info` (the `Info`: firmware,
battery, reset cause, clock, 1-Wire slot state, and the crash record) and
`…/config` (the `ConfigDump`: `sensors.cap_*` incl. the GP pin enums, and
`w1_slot_type`). `NODE_REPORTS` in `mock_publisher.py` sets them per Node:

| Node | firmware | GP pins and sensors | shows |
|---|---|---|---|
| `70b3d50000000000` Sticker 1 | 2.0.0 | off | the plain case |
| `70b3d50000000001` Sticker 2 | **1.5.0** | off | "FW < 2.0.0": any config save is refused (`monitoring.pending.firmware_too_old`) |
| `70b3d50000000010` Sticker 3 — full | 2.0.0 | inputs: A pulse (`input_a_count`), B analog (`input_b_voltage`); barometer, light, 1-Wire slots 1-2 Dallas (`w1_N_temperature`) | count rows, input voltage, 1-Wire rows |
| `70b3d50100000020` (cold-store) | 2.0.0 | `pir` (`motion_count`), barometer reporting NA | **Last fault** card (`K_ERR_ARM_BUS_PRECISE_DATA_BUS`, PC/LR), pressure/altitude at a dash (`no_data`) |
| `70b3d50100000021` (cold-store) | 2.0.0 | inputs: A analog, B digital (`input_b_count`) | input A voltage goes NA in ~1 of 4 readings and back (the held value is cleared) |
| `70b3d50100000022` (cold-store) | 2.0.0 | `buzzer`, light sensor reporting NA | buzzer = no GP rows, illuminance at a dash |

The NA → null cases need the v2 `reading` (a `no_data` event), so they sit
on the P2P Nodes; the LoRaWAN Nodes still report on `lorawan/sensors`.
The Device card is on the Node's **Diagnostics** tab.

## Seeding the demo (`make demo-seed`)

What lives in the portal's own DB or needs the portal to ask: run it once
`make demo` is up and the admin has been created at `/setup` (with no admin
`dev/seed_demo.py` stops and says so). Pair the Hubs first if you want the
Beacon archives: the request is a signed command.

- `dev/seed_demo.py`: mints a 10-minute session for the first admin
  (`target/debug/portal mint-session`, `PORTAL_CONFIG=dev/config.yaml`,
  token in a private temp file, revoked at the end) and posts
  - calibration certificates (a one-page PDF generated on the fly) in every
    state, dates relative to today: **valid** Node `…0000`, Beacon
    `EE:E0:00:00:00:01`, server probe lines 1-2 (line 1 has an expired
    certificate in its history), cold-store line 1; **warning** (≤ 30 d)
    Node `…0001`, Beacon `…00:02`, server line 3; **second warning**
    (≤ 7 d) Node `…0010`, Beacon `…00:03` (last day), server line 4;
    **expired** Node `…0020`, Node `…0021` (expires today), Beacon
    `EE:E0:00:00:01:01`, server line 5, truck line 1; **none** for the rest
    (Node `…0022`, Beacon `EE:E0:00:00:02:01`, server lines 6-8, …);
  - a Beacon archive download for the server's three Beacons. The mock
    answers `beacon-history` like control/ble (`requested` → `receiving`
    x/y → `records` → `done`), three days at 15 min (288 rows each);
    `--beacon-history silent` brings back the old accept-and-say-nothing.
- `dev/alarm.sh` on the server: Probe 6 critical, Probe 7 warning, Probe 8
  raised and cleared (alarm history); wire lines 5-7.
- `dev/node.sh silence 70b3d50000000000`: offline after 4 missed intervals.
- `dev/hub.sh offline truck`: the truck Hub goes offline.

Re-running uploads a fresh current certificate per device (the previous one
moves to its history) and re-raises the alarms. Undo the rest with
`make node ARGS="resume 70b3d50000000000"`, `make hub ARGS="online truck"`,
`make alarm-clear LINE=5` (and 6). A clean slate is `make portal-down`,
removing `dev-data/`, and `/setup` again.

## Clients in the server's cluster

`cold-store` and `truck` are Clients, and a real Client is registered on the
server (Add hub, `cluster-client-add`) before it can reach the broker at all.
So they carry real Hub hostnames like the joiner, and `make demo-seed`
registers both through `POST /hubs/fiber-mock-001/cluster/clients` (skipped
when the hostname is already in `cluster_clients`). The server runs LoRaWAN,
so Add hub requires a gateway EUI: cold-store gets `70b3d54900aa0002`, truck
`70b3d54900aa0003` (registered with its gateway, the concentrator missing
since). Their rows turn *complete* on the next status. Renamed from
`fiber-mock-002`/`-003` on 2026-10-14: a pairing of the old names is stale,
pair them again (`DEF456`, `GHI789`).

## The joiner: a Client Hub for Add hub (C5)

`--hub joiner` is `proximos-c5c1e000`, a LoRaWAN gateway Hub (gateway EUI
`70b3d54900aa0004`) with a real Hub hostname, because the portal's Add hub
accepts nothing but decision #17's `proximos-<8 hex>`. It is in no compose
profile but its own: a Client is not on the Server's broker until it has
joined, and its first `status` is what moves the portal's registry row from
*pending* to *complete* (`plan/control/cluster.md` §9). So:

1. Start the portal with a CA to fingerprint. A dev host has no
   `/data/tls/ca.crt`; a **debug** build reads `PROXIMOS_DEV_CA_CERT`
   instead (any PEM certificate, e.g. one made with `openssl req -x509`), and
   a release build never does.
2. Pair the server Hub, then **Hubs → Přidat hub**: hostname
   `proximos-c5c1e000`, EUI `70b3d54900aa0004`. The server mock answers
   `cluster-client-add` the way C2's agent does: two effects, and a
   22-character password in `config/response.result`, sealed to the
   request's signed `reply_key` (`password_sealed`, never in clear). The
   portal opens it in memory and shows the join bundle once.
3. `make joiner-up`. Its status arrives and the row turns complete.

The server mock's effects are set by `MOCK_CLUSTER_ACCOUNT` and
`MOCK_CLUSTER_GATEWAY` (`ok` or `failed`, default `ok`) to drive the portal's
"registration incomplete" and "removal incomplete" paths. The gateway is
`skipped` when no EUI is given or the server's radio mode runs no LoRaWAN,
exactly as on a Hub. As on a Hub, a re-run without an EUI keeps the one the
account has, and the server's own gateway EUI is refused. The mock never
prints the password, and `--selftest` checks its seal against
`fiber-protocol/vectors/sealed_reply.json`.

## What changed vs v1

The v1 file drives a fleet (`--devices N`, offline/edge-case variants,
lorawan gateways, EYE tag simulation, and command handlers that react to
almost every mutating MQTT command). None of that exists yet in v2 — M1 is
read-only ingest for a single Hub. Trimmed to:

- **One Hub per process**, from a small profile table, rather than v1's
  `--devices N` fleet inside one process. See the demo cluster above.
- **Topics**: `status`, `system/info`, `sensors/aggregated`, `config/state`
  (now incl. `eye_tags`), `alarms/events`, `power/standby`,
  `export/probe_1m/{line}` (M1 set), plus the M5-D Node/EYE set:
  `lorawan/sensors`, `lorawan/sensors/{dev_eui}/{config,info,full-config,
  command,history}`, `eye/sensors`, plus the prefix-less, retained
  `radio/gateways` (one row per Hub that has a concentrator, online with
  RSSI/SNR, republished every 60 s — `--radio-gateways-s`, server only). Its `api_status` is `ok` unless
  `MOCK_RADIO_API_STATUS=auth_failed|unreachable` (or `--radio-api-status`)
  says otherwise, which is how the card's banner gets tested. Still dropped:
  `lorawan/gateways`, `responses/*` (no v2 consumer yet).
- **The v2 envelope, one dialect (P3)** — see the section at the bottom.
- **Command handling is signed-only (M4-E)** — v1's sprawling
  `COMMAND_HANDLERS` table (which accepted unsigned commands and never
  verified signatures) is gone. Instead the mock implements the real
  device-side protocol: a deterministic device-CA Ed25519 identity
  (`--seed`, defaulting to the `--hub` profile's own phrase, SHA256 → 32B
  seed so restarts keep the same CA), pairing on `pair/request`
  (PBKDF2-HMAC-SHA256 480k iters + AES-256-GCM over the raw 32-byte CA seed,
  keyed either by the fixed `--pairing-code`, the profile's own, or — when the request carries
  `"local": true`, meaning the portal runs on this device — by the shared
  `LOCAL_PAIRING_PASSWORD` constant, which ignores `--pairing-code`
  entirely), and the full signed challenge/confirm loop on
  `commands/config/request` / `commands/config/confirm` with Ed25519
  verification of the **9-field request / 7-field confirm** pre-images
  (sorted-key compact JSON per `plan/portal/mqtt/design.md` §3.2, built from
  the checked-in vectors — the v1 7/5-key shape is gone with P3), **±60 s**
  timestamp window and per-signer nonce single-use. Verification failures log the
  expected message bytes hex — a mismatch against a correctly-keyed signer
  is a canonicalization FINDING. Applied phase-1 commands: `set_threshold`
  (control-core's `{line, thresholds: {…}}` shape only, a flat body is
  "Missing thresholds", a null bound becomes 0.0 as on the device; reflected
  in an immediate `config/state` republish), `restart_application`
  (offline → online after 3 s), `power_off` (offline until SIGHUP),
  `factory_reset` (thresholds back to defaults). A verb **not in the
  catalogue is refused before any crypto**, with the device's own wording —
  that fall-through is what P3 closed. A catalogued verb with no handler
  here still answers a no-op SUCCESS; see the P3 section below.
- **Three Nodes (STICKER), three registered Beacon (EYE) tags — live (M5-D)**.
  Sticker 1 and 2 carry the plain three-field profile; **Sticker 3 is
  `rich=True`** and reports the whole continuous+counter registry
  (`crates/fiber-protocol/src/fields/lorawan.rs`) — external probes, machine
  probes, the environmental group and all five counters — so the Node detail
  page has values in every group instead of 22 dashes. The eight `event`-kind
  fields stay unpublished on purpose: their wire shape is not in mqtt.md, and
  this file is the executable spec for those payloads. EYE tags report
  `moving`/`movement_count` (two of that registry's four fields, previously
  always null) and assert `low_battery` from their own voltage, which is why
  `battery` is not thresholdable portal-side.
  - `lorawan/sensors` aggregate telemetry every `--sticker-interval-s`
    (default 20 s): temperature random-walk 2–10 °C with rare CRITICAL
    spikes above 12 °C, humidity 40–70 %, voltage ~3.0 V; monotonic `fCnt`
    + occasional `hall_left_count`; `field_alarm_states` computed from each
    Node's `field_thresholds`; `rssi`/`snr`/`gateways` (`mock-gw-{index}`);
    `last_seen` fresh on every publish (gates portal persistence);
    `events[]` carries an fPort-3-shaped `type:"alarm"` entry on a
    crossing into CRITICAL.
  - Per-Node device-config store (STICKER_SETTABLE-ish keys:
    `application.*`, `sensors.cap_*`, `alarms.*`). These five verbs used to
    arrive unsigned on flat `commands/<verb>` topics; **P3 moved them onto
    the one signed path** and deleted the flat subscriptions. The answers
    are unchanged:
    config comes back as 1–2 pages on `…/{dev_eui}/config` (empty retained
    tombstone published at startup; pages themselves not retained), info is
    retained on `…/info`, full-config adds `last_seq`, `sticker_force_send`
    answers `requested` → immediate telemetry → `ok` on `…/command`.
  - History ring buffer per Node (`--history-depth-s`, default 3600 s,
    pre-seeded backdated at startup, one record per telemetry tick).
    `get_sticker_history {from,to}` replays `…/history` frames (≤20
    records, `frame_index`/`frame_count`) then an `ok` command result; a
    window entirely outside the buffer yields result `out_of_range` and
    **no frames** — the command topic is how the portal learns refusals.
  - Signed `set_sticker_config {dev_eui, config, save}` (challenge/confirm
    loop): keys validated against the settable store — unknown keys →
    ERROR response; on success a `…/command` result + config-page
    republish.
  - `eye/sensors` every `--eye-interval-s` (default 10 s, the BTSMP1
    advertisement cadence): the registered tag (≈5 °C / 45 % / 2900 mV,
    `stale:false`) plus a DISCOVERED-only tag (`EE:E0:00:00:00:99`,
    `discovered:true`, no name) appearing 30 s after startup and never
    listed in `config/state`. Signed `add_eye_tag`/`remove_eye_tag`/
    `set_eye_field_threshold`/`delete_eye_field_threshold` mutate the tag
    list/thresholds — renamed to `beacon-add`/`beacon-remove` in P3, with
    `beacon-provision` and `beacon-history` alongside them (echoed via the
    next `eye/sensors` + `config/state.eye_tags`). B5 dropped the EN12830
    probe: no `is_en12830` key and no `eye/detect` publish. `detect_eye_tag`
    no longer falls through to "accepted (no-op)" — it is not in the
    catalogue, so it is **refused**, which is the point of P3.
  - `mock/control` Beacon actions (B5, `dev/beacon.sh`): `eye_silence`
    drops a tag out of `eye/sensors` altogether — which is what a quiet
    broadcast looks like — `eye_resume` puts it back and advertises at
    once, `eye_rssi` moves it between signal bands.
- Payload shapes for the topics that are published are unchanged from v1,
  except `power/standby` and `export/probe_1m/{line}`, which v1's mock never
  actually published (the real device does) — those two were built fresh
  from the documented wire shape in `plan/v1/viewer/mqtt.md`.

## Run standalone

```bash
python mock_publisher.py --host localhost --port 1883                    # server
python mock_publisher.py --host localhost --port 1883 --hub cold-store   # a client
```

Requires `paho-mqtt` and `cryptography`. Ctrl-C publishes a retained
`offline` status before exiting. The pairing code and CA public key are
printed in a banner at startup; each `pair/request` logs which mode it
answered in (`local (no code)` / `remote (code …)`).

## The v2 envelope (P3)

`plan/portal/mqtt/design.md` §3–§5. What changed, and why it matters for
anyone reading a mock log:

- **The mock no longer invents the contract.** It loads
  `portal/crates/fiber-protocol/vectors/{catalogue.json,envelope_v2.json}` at
  start, by relative path, the way `control-core`'s `v2_tests.rs` does. There
  is no copy of the verb table here, because a copy is a thing that drifts.
  The Docker service mounts that directory read-only for the same reason.
- **Nine-key request / seven-key confirm pre-images**, with the signed `v: 2`
  and `level` fields. The old seven/five shapes verified nothing the portal
  has signed since P0 landed — for one day, `make mock-up` was not evidence of
  anything, which is what P3 fixes.
- **Verification follows design.md §3.3 in order**: `v`, `level` parses,
  unknown verb refused **before any crypto**, level matches the catalogue, CA
  chain and expiry, timestamp, nonce, the verb's permission **and**
  `level:<level>` on the certificate, signature, record nonce.
- **The fall-through fails.** `_apply` still ends in "accepted (no-op)", but it
  is now only reachable for a verb that *is* in the catalogue. In v1 it
  answered SUCCESS to anything, which hid the whole `*_eye_*` → `beacon-*`
  rename: the dev loop reported green runs for commands no Hub would execute.
- **The device's numbers**: ±60 s timestamp window (was ±600), 300 s challenge
  TTL (was 60), max 10 concurrent challenges, and `expires_at` computed from
  the *request's* timestamp — the skew design.md §13 item 5 hands to
  `control.md`, copied rather than quietly corrected.
- **`read` and `operate` are single-shot**: no challenge, no slot held. The
  device executes and *then* publishes `config/response` with the real result.
- **The flat dialect is gone.** All eight ex-`PLAIN` verbs are served from the
  signed path; there are no `commands/<verb>` subscriptions left.

### Self-test — no broker needed

```bash
python mock_publisher.py --selftest
```

Replays every vector in `envelope_v2.json` through this mock's canonicaliser
and verifier, replays `certificate.json` the same way (canonical form, CA
signature, and that the mock's own certificate timestamps are the RFC3339
strings the device parses), and sanity-checks the catalogue. It found two real
cross-language disagreements on 2026-09-15: Python's `ensure_ascii` escaping a
Czech `reason` (the one design.md §10 predicted) and Python zero-padding
negative float exponents to two digits — `1e-07` where `serde_json` writes
`1e-7`. Both are fixed in `_canonical`; the test is what keeps them fixed —
when it is run. Nothing in `.gitlab-ci.yml` runs it yet; it needs no broker
and no paho, only `cryptography`, so a job is cheap when someone adds one.

## E2E signed-command test

`e2e_client.py` plays the portal's role end to end: pairs over the remote path
(decrypts the CA seed with the pairing code, cross-checks the derived public
key and the admin-certificate signature), mints a user certificate carrying
the catalogue's permissions plus the five `level:*` strings, and then drives
one verb per shape:

- `get_sticker_info` (`read`) and `sticker_force_send` (`operate`) —
  single-shot, asserting a response, **no challenge**, and (for the read) the
  data publish on `lorawan/sensors/{dev_eui}/info`;
- `set_threshold` (`manage`) — challenge → confirm → response, then the
  retained `config/state` read back to check the thresholds actually changed;
- the four Beacon actions, `beacon-add/-provision/-history/-remove`;
- `add_eye_tag` — refused with the device's own slug and wording
  (`authorization_failed` / `Invalid command: Unknown command type: add_eye_tag`);
- a v1-shaped envelope — refused with `unsupported_envelope` / `v=2 required`
  under `command: config_request`; a `v: 3` with `v=2 required, got 3`; an
  unknown `level` with `Invalid command: unknown level: root`.

The negative checks ("no challenge") are real since 2026-09-15: frames
`wait_for` skips on its way to the topic it wants are handed to the next
`drain` instead of dropped, so a challenge published *before* the response
can no longer hide.

```bash
python e2e_client.py --host localhost --port 1883   # exits non-zero on failure
```

To exercise the redelivery case, run the mock with
`--inject-duplicate-challenge` (it publishes every `config/challenge` twice,
the way rumqttc replays one on reconnect) and the client with
`--expect-duplicate-challenge`. The client then confirms **every** frame — as
a portal without design.md §6.2's seen-set would — and asserts the device's
half of the rule: one `SUCCESS`, no second `config/response`, and each
duplicate confirm refused on `errors` with `Challenge not found`. That is why
the portal keeps a seen-set; the seen-set itself is pinned where it lives, by
`relay.rs::a_redelivered_challenge_produces_one_confirm`:

```bash
python mock_publisher.py --inject-duplicate-challenge
python e2e_client.py --expect-duplicate-challenge
```

## Q3 kill tests

`portal/tests/q3_redelivery.rs` (`#[ignore]`d, needs this broker) measures the
two numbers design.md §6.3 owes. See that file's header for why they are
measured against `rumqttc` rather than through this mock.

```bash
make mock-up
cargo test -p portal --test q3_redelivery -- --ignored --test-threads=1 --nocapture
```
