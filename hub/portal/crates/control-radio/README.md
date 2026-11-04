# `control-radio`

**Stance: clean-room** (`plan/portal/monolith.md` §11.8).

The implementation is written fresh from the plan set alone — `plan/control/radio/plan.md`,
`lorawan.md`, `p2p.md`, `node-firmware-v1.4.0.md`, plus `plan/portal/control.radio.md` and
`plan/system/design.md` for the contracts they own. No v1 `.rs` file is copied, transcribed,
or required reading. The other permitted inputs are the pinned `chirpstack_api` crate, the
pinned Node firmware at `sticker-firmware` v1.4.0, and the public standards the P2P crypto
cites (RFC 3610 AES-CCM, NIST SP 800-38B AES-CMAC).

**Standalone guarantee** (decision #12, `monolith.md` §11.7): this crate depends on no other
crate in this workspace, and its integration stack runs through `control-radio-harness` with
no agent subsystems started. It is no longer separately shippable — that is the stated price
of living in one workspace.

**Build prerequisite: `protoc` and the well-known types.** The pinned `chirpstack_api` crate
generates its Rust from protos in a build script on every build (all six proto groups,
unconditionally), and `prost-build` 0.14 bundles no `protoc`. So this crate makes
`protobuf-compiler` **and** `libprotobuf-dev` requirements of the whole `portal/` workspace,
not just of itself: without them every `cargo` command dies in that build script. Both are
needed — the crate vendors only `google/api/{annotations,http}.proto`, while ChirpStack's
`common/common.proto` imports `google/protobuf/timestamp.proto` and protoc resolves that from
`/usr/include`, which the compiler package does not populate. CI installs both in
`.gitlab-ci.yml`'s `.native_deps`; the device build gets them together from
`DEPENDS += "protobuf-native"` (`lorawan.md` §3), which also names the fallback if codegen
ever becomes a blocker — vendoring the pre-generated stubs, never a second hand-rolled
client.

Command namespace is `control.radio`, reached as `proximosctl control.radio <command>`.

## Binaries

| Binary | What it is |
|---|---|
| `control-radio-harness` | the `control.radio <command>` surface with a real store and no agent subsystems — what the integration stack drives |

### The P2P central

The central's event loop is not a binary of this crate. Since PI1
(`plan/portal/p2p_integration.md` §4.1) it runs as the supervised `p2p` task of
`proximos-control`, on every Hub whose `radio.mode` allows the P2P transport, over the
northbridge UART alone (the RAK5146 is LoRaWAN's). The bench host `control-radio-p2p-host`
that stood in for it is gone; `proximos-control` runs the same loop, locally too:

| Environment | Effect |
|---|---|
| `PROXIMOS_CONFIG` | the rendered device config (default `/run/proximos/control/config.yaml`) |
| `PROXIMOS_RADIO_DATA` | the store root holding `p2p/net_id`, `p2p/sessions.db` and its lock (default `/data/proximos/radio`) |
| `PROXIMOS_P2P_UART` | modem device (default `/dev/ttyAMA3`) |
| `PROXIMOS_RADIO_ALIVE` | the heartbeat file (default `/run/proximos/radio/alive`) — point it somewhere writable off-Hub |
| `PROXIMOS_P2P_COUNTER_BLOCK` | **test/HIL only**: TX counter block size, 8..=4096 (default 4096), logged loudly when set (`p2p_tower_gateway.md` §8.5) |

The Northbridge runs the P2P gateway image: it ACKs, seals downlinks and keeps the peer table,
and the central feeds it (`plan/control/radio/p2p_tower_gateway.md` §6). There is no
`--no-mqtt` — with no broker the publishes fail on their own and the RX path carries on.

| Key | Default | Meaning |
|---|---|---|
| `radio.p2p_radio.modulation` | `lora` | `lora` only; `fsk` is refused |
| `radio.p2p_radio.sync_word` | `0x12` | LoRa sync word, `0x12` (private) or `0x34` (public) |
| `radio.p2p_radio.tx_power` | `14` | the Northbridge's own TX power, -16..=15 dBm (RFO_LP PA; refused outside at load) |
| `radio.p2p_radio.node_tx_power_dbm` | `0` | the Nodes' TX power, sent once in the JoinAccept; `0` = no assignment, above 14 dBm clamped to 14 (#486) |

`deliver_time` and `rekey_counter_threshold` are still accepted, so existing configs load, and
do nothing.

All P2P diagnostics go to **stderr**, one line per event, via the `diag!` macro — there is no
`tracing` in this crate. That trace is the bench's primary evidence, so redirect it; on a Hub
it is `journalctl -u proximos-control`.

### Bench runbook

The engine is exercised without any hardware by the FakeModem tests; run those first, and read
the trace they print — it is what a real bench log looks like, line for line:

```sh
cargo test -p control-radio --lib p2p:: -- --nocapture
```

Against real hardware, with the northbridge on a J-Link and the RTT↔PTY bridge from
`proximos/firmware` `fiber-northbridge/tests/hil_p2p/nb_rtt_bridge.py` in front of it:

```sh
export PROXIMOS_RADIO_DATA=$HOME/p2p-bench/radio && mkdir -p "$PROXIMOS_RADIO_DATA"
export PROXIMOS_CONFIG=$HOME/p2p-bench/radio.yaml     # radio.mode: p2p (or dual)
cargo build -p control-radio --bins && cargo build -p proximos-control

target/debug/control-radio-harness create --radio p2p                 # -> {"net_id": "…"}
printf '{"app_key":"%s"}' "<32 hex>" > dut.key && chmod 600 dut.key    # never on argv
target/debug/control-radio-harness node-add --radio p2p \
    --serial 0x80e007b2 --dev-eui <16 hex> --profile default --keys-file dut.key

PROXIMOS_P2P_UART=/tmp/p2p-modem PROXIMOS_RADIO_ALIVE=$HOME/p2p-bench/alive \
    target/debug/proximos-control 2> p2p.log
```

**`--dev-eui` must be the node's own DevEUI, byte for byte.** Since #73 it is what the node
puts in its JoinRequest and what both ends feed to the session-key KDF, so a mismatch does not
fail the join loudly — the central simply never finds a registry entry for the DevEUI on the
air and logs `JoinRequest from unregistered dev_eui …`. Read it off the node
(`ats device info`, the `DevEUI:` line) rather than transcribing it from a label. An all-zero
DevEUI means the node is unprovisioned and will not join.

**A `sessions.db` written before the DevEUI identity change (GitLab #73) is refused, not
migrated.** Its nodes and sessions are keyed by serial, and every session in it holds a key
derived under the old `serial`-keyed KDF — a migrated session would decrypt nothing. The load
error names the files; wipe them and register again:

```sh
rm "$PROXIMOS_RADIO_DATA"/p2p/sessions.db "$PROXIMOS_RADIO_DATA"/p2p/cache/*.json
# then node-add as above; `net_id` is kept (it is a separate file).
```

Sessions of the earlier P2P radio link (before the gateway image) are not refused: they are
dropped on load with one `[p2p]` line, the nodes stay registered and simply join again. Prefer
`clear` to deleting the file by hand: it keeps `ctr_block_end`, the gateway counter floor that
belongs to the kept `net_id`; `wipe` drops both.

Expected first three lines: `modem GET_INFO ok: …` → `radio armed: … net_id=0x…` →
`P2P central running`. A `no RSP for command 0x01 within 800ms` instead means the bridge, the
probe, or the RTT down-buffer size is wrong, not the central.

`node-send` works while the host is running — the session DB is locked across load-mutate-save,
so the CLI and the event loop cannot lose each other's writes:

```sh
target/debug/control-radio-harness node-send --radio p2p --dev-eui 70b3d57ed0000abe --fport 85 --hex 2200
target/debug/control-radio-harness node-send --radio p2p --dev-eui 70b3d57ed0000abe get-info

# `--serial` is still accepted everywhere as an alias — it is what is printed on the device —
# and resolves through the registry to the same node.
target/debug/control-radio-harness node-send --radio p2p --serial 0x80e007b2 get-info
```

`--fport 85` is required and is the only accepted value: every P2P downlink is an fPort-85
command, carried as `0x81 86 ‖ body` (at most 76 B of body). The structured form allocates a
`Command.seq` and the runtime correlates the node's port-85 answer against it, after stripping the `APP_PROTO_VERSION` byte the
node prefixes every fPort-85 response with (`decode::strip_version_prefix`).

### Northbound topics

One family for both carriers, with no transport level (decision D3, GitLab #73):

```
proximos/{hostname}/nodes/{dev_eui}/{info,reading,alarm,config,command,history}
```

`{dev_eui}` is the node's identity on both arms. The transport is redundant in the topic — a
node is hardware-bound to one carrier, so the DevEUI already determines it, and
`reading`/`alarm`/`command` bodies are byte-identical across carriers — so it rides inside the
retained `info` document as a `transport` field instead. Subscribe with
`northbound::node_filter(area)` (`proximos/+/nodes/+/{area}`) rather than a hand-written
string: one wrong level subscribes successfully and receives nothing, which looks exactly like
a Hub with no traffic.

`info` is the only retained area. P2P publishes it on every successful join and tombstones it
(an empty retained payload) when a removed node's session is dropped — or, if the node had no
live session to detach, from `node-remove` itself.

### Changing the network spreading factor

The spreading factor is network-wide — the northbridge has one receiver, so a per-node SF would
only make that node unhearable. `radio-set` moves it for the whole link:

```sh
target/debug/control-radio-harness radio-set --radio p2p --spreading-factor 12
target/debug/control-radio-harness radio-set --radio p2p --reset     # back to the config key
target/debug/control-radio-harness status --radio p2p                # what is actually in force
```

The value is recorded in `sessions.db`, never in the config file: `radio.yaml` is rendered
(decision #4), so a lever that wrote there would be lost on the next render. That makes the DB
the authority — it overrides `radio.p2p_radio.spreading_factor` at startup, on every `EVT_BOOT`
re-init, and live — and `status` reports both sides of it, as `p2p.spreading_factor:
{config, effective, overridden}`.

A running central picks the change up on its next event, or within 2 s on a quiet link, and
pushes it as `RX_STOP → SET_RADIO_CONFIG → TWR_START`, moving that one field and nothing else.
It waits first for a JoinAccept in the modem's single TX slot, since retuning under one would
send it on the wrong SF or not at all. A modem that refuses the config keeps the one it holds,
gateway receive is restarted, and the switch is retried on the next event.

Nothing is sent to the nodes — there is no downlink for this, by design. A paired node goes
quiet, self-heals after 8 consecutive failed uplink cycles (`P2P_REJOIN_FAIL_THRESHOLD`), and
re-discovers the SF with a join-time sweep, persisting the one that answered. So expect the
fleet back after roughly 8 report intervals plus one sweep pass, and the node's own
`ats radio status` to read `sf: 12 (config 12)` once it has.

### Updating the Northbridge firmware

The P2P task compares the Northbridge image with the bundled one
(`/usr/share/proximos/northbridge`, or `PROXIMOS_NB_BUNDLE`) every time it starts, and flashes
it when they differ (`plan/control/radio/northbridge_update.md` §3.4). To flash now, again or
with a hotfix:

```sh
proximosctl control.radio northbridge-update --radio p2p                 # the bundled image
proximosctl control.radio northbridge-update --radio p2p --force         # even if it runs already
proximosctl control.radio northbridge-update --radio p2p \
    --file northbridge.bin --manifest northbridge.json                   # a hotfix
```

The command writes `northbridge/update-request.json` under the radio store and waits up to
`--timeout` (180 s) for the task's answer in `northbridge/update-state.json`. `status --json`
shows the outcome as `northbridge.update`. `PROXIMOS_NB_VERIFY=both|readback` replaces the
default CRC verify with a read-back.

### Retiring a stalled history replay

A `ReqHistory` is answered by a stream of `HistoryFrame`s, not by one response, so the queue head
stays in flight until the last frame arrives. A node that dies mid-replay would hold it forever
and block every later downlink to that device.

| Key | Default | Meaning |
|---|---|---|
| `radio.p2p_radio.history_stall_timeout_s` | `3600` | seconds without a frame after which the central drops the collector, pops the head and returns the session to `Idle` (`plan/control/radio/p2p_history_replay.md` §4.3 option 1, §5.1) |

An hour, not minutes: a node's 1 % duty ledger can legitimately block a stream for about half of
one, and a shorter timeout would retire healthy replays. Retirement publishes **nothing** — a
`frame_count: 0` sentinel would mark the portal's backfill job DONE and the window would never be
re-requested — so the gaps are logged and the portal's reconciler asks again.

## What `tests/carrier_parity.rs` does not cover

`tests/carrier_parity.rs` drives one command sequence — `get-info`, `get-config`,
`set-config`, `cmd history-replay` — down both carriers for one node and asserts that the
downlink bytes, the northbound documents, the `node-get` cache slots and the `node-get`
answers match. A second sequence — `get-config`, the boot settings-info, `get-param`, a
second `get-config` — checks that both arms fold a dump into the config they hold the same
way (merge, then replace). That is a claim about **this crate's** decode and publish path,
and nothing wider. It says nothing about:

- the `proximos-control` MQTT command router — the parity test publishes through a recording
  publisher and a pure `Outcome`, never a broker;
- the portal's P2P subscribe/route side: which topics it joins and what it does with a
  document once it arrives;
- `SOURCE_REPLAY` and the backfill accounting a history replay feeds;
- **H0, DevEUI identity.** The fixture gives the node one DevEUI on both arms because that is
  what a parity claim needs; how a P2P node acquires that DevEUI, and what happens when the
  registry and ChirpStack disagree, is not exercised;
- multi-frame history over both carriers — the golden replay here is one frame of one.
