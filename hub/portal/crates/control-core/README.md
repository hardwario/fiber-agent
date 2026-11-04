# control-core

**Stance: copy-and-edit (decision #3, monolith.md §11.8)**

Imported byte-for-byte from proximos-v1/application@3e22122028d3f1b45b2032ce20290f8bbae6b2ec (origin/dev).

This crate is the v1 Hub agent (~79 kLOC), moved into the portal workspace as-is
so the import is reviewable as an import. Nothing inside `src/`, `tests/`,
`proto/`, `scripts/`, `build.rs` or the three YAML files was edited as
imported in MR !13 (squash commit `4eb2cb9`); two files have since been
edited — see "Edits to imported code" below. Reviewers verify the as-imported
claim by re-running the command that produced the tree:

```bash
V1=<path to>/proximos-v1/application
git -C "$V1" archive --format=tar 3e22122028d3f1b45b2032ce20290f8bbae6b2ec \
    src proto tests build.rs scripts \
    fiber.config.yaml fiber.sensors.config.yaml authorized_signers.yaml \
  | tar -x -C portal/crates/control-core
```

Internal identifiers keep their v1 spelling. The lib is still `fiber_app`, and
the crate still builds `fiber_app`, `fiberctl` and `fiber-factory-reset`. The
rename sweep is §11.3; collapsing the transitional second service binary into
`portal` is §11.4. Both are later MRs.

## What was deliberately not copied

| Not copied | Why |
| --- | --- |
| `Cargo.toml` | Rewritten as this crate's manifest: package renamed `control-core`, `[lib] name` held at `fiber_app` until §11.3, `tempfile` dev-dep aligned to the portal package's `3.27.0` pin, and a `[lints]` debt table added. Dependencies, build-dependencies, features and the three `[[bin]]` targets are transplanted verbatim. |
| `Cargo.lock` | One lockfile per workspace: `portal/Cargo.lock`. |
| `rust-toolchain.toml` | Promoted to `portal/rust-toolchain.toml` (1.94.0, same channel v1 pinned). |
| `.cargo/config.toml` | Only held the `aarch64-unknown-linux-gnu` linker override; CI sets `CARGO_TARGET_*_LINKER` instead of shipping a committed cross-linker path. |
| `.gitlab-ci.yml`, `.gitlab/ci/*` | Folded into the root CI. Two v1 job settings are now workspace-level policy rather than per-crate: clippy is a **blocking** gate here (v1 ran it `allow_failure: true`), and `--test-threads 1` is mandatory because several of these tests share global state. |
| `README.md` | `plan/v1/application/architecture.md` is the inventory of record for this code. |
| `.releaserc.json`, `commitlint.config.js`, `.gitleaks.toml`, `.gitleaksignore`, `.vscode/`, `.config/nextest.toml`, `LICENSE`, `.viewer` (stray file) | Per-repo tooling; the v2 superproject and portal repo own their equivalents. |

## Inherited debt (not fixed here — editing imported files would break the byte-for-byte claim)

- **Lints.** 189 unique findings (169 clippy + 20 rustc) are parked per-lint in
  `Cargo.toml`'s `[lints.clippy]` / `[lints.rust]` tables, each with its
  occurrence count. v1 tolerated these with a non-blocking clippy job; the
  workspace gate is blocking, so they are declared instead of hidden.
  `await_holding_lock` (2) is a real correctness smell, not style.
- **rustfmt.** `src/libs/beacon/mod.rs:31` (the `SharedBeaconConnections` type
  alias) was 108 columns and failed `cargo fmt --check` on 1.94 — pre-existing
  v1 drift, present at application@3e22122 with no rustfmt config in either repo
  to explain it away. Wrapped after the import; see "Edits to imported code".
- **Sleep-based test synchronisation.** Several imported tests wait on
  `tokio::time::sleep` for the storage thread instead of on its reply channel.
  One of them (`libs::mqtt_export::drain`) raced hard enough to fail; see "Edits
  to imported code". The pattern is still present elsewhere in the tree and is
  latent flake, not settled behaviour.

## Compile-blocking edits made to imported files

None. The tree compiles unmodified on Rust 1.94.0.

## Edits to imported code

The copy-and-edit stance (decision #3) allows editing imported files; each such
edit is listed here so the byte-for-byte import as landed in MR !13 (squash
commit `4eb2cb9`) stays the audit baseline and every departure from it is
enumerated.

| File | Edit | Why |
| --- | --- | --- |
| `src/libs/mqtt_export/drain.rs` (tests only) | Both drain tests now synchronise with the storage thread via `StorageHandle::flush_sync(30s)` instead of a fixed `tokio::time::sleep`: two call sites replaced `flush()` + a 100 ms sleep, one (around line 297) replaced a bare 150 ms sleep with no preceding `flush()`. | `drain_advances_cursor_and_stops_on_publish_failure` failed deterministically under `cargo test --workspace`. Measured cause: the storage thread needs ~105-110 ms from `spawn` to have the five queued rows committed (SQLite open + `create_schema`/`verify_schema` dominate), against a 100 ms fixed budget — the assertion won or lost by a few ms depending on codegen and machine load. Real time really elapsed across the sleep (101.3 ms measured), so tokio's `test-util` feature was **not** the mechanism; the same ~107 ms row-visibility time reproduces in a build without `test-util`. `flush_sync` rides the same FIFO queue as the writes, so its reply proves the writes landed. No production code touched. |
| `src/libs/beacon/mod.rs` (line 31) | Wrapped the 108-column `SharedBeaconConnections` type alias onto two lines. | `cargo fmt --check` is a blocking workspace gate; this was the only line in the 163 imported files that failed it. Formatting only, no semantic change. **Superseded:** the file is deleted by the control-ble integration below. |

### The `control-ble` integration

`plan/control/ble/*.md`'s Part B. `control-ble` becomes this crate's Bluetooth
component: it owns the adapter, both roles, the BTSMP1 decode and the EN12830
archive, and this crate becomes its host. The dependency runs
`control-core → control-ble` and never the other way — decision #12's standalone
rule binds the crate, not its consumers.

**Deleted, superseded by `control-ble`:**

| Deleted | Superseded by |
| --- | --- |
| `src/libs/beacon/{advertising,config,en12830,mod,monitor,provisioning,state}.rs` | `control_ble::{decode, central}` — `plan/control/ble/central.md`, `beacon-btsmp1.md` |
| `src/libs/ble/gatt/{auth,beacon_add,device_info,node,service,state,mod}.rs` | `control_ble::peripheral::gatt` — `plan/control/ble/peripheral.md` |
| `src/libs/ble/gatt/terminal.rs` | **Nothing.** CP-1 removes the BLE terminal; `fiber.config.yaml`'s `ble.enable_terminal` is now a start-up failure rather than an ignored key, so the switch cannot appear to still exist. |
| `src/libs/ble/{advertising,config,event_router,mod}.rs` | `control_ble::peripheral::{advertise, events}` and `control_ble::config` |
| `tests/beacon_add_simulated.rs` | `tests/beacon_commands_simulated.rs`, which asserts the AC 4 equivalence across all three origins rather than pinning one applier method |

**Moved (`git mv`, history preserved):**

| From | To | Why |
| --- | --- | --- |
| `src/libs/ble/gatt/{wifi,lan,net_error,time_sync}.rs` | `src/libs/network/` | The surviving `nmcli` exec surface. CP-1 removed the BLE terminal and the shell recovery ladder, not NetworkManager; these belong to the network module, not to a GATT server this process no longer hosts. |

**Edited:**

| File | Edit | Why |
| --- | --- | --- |
| `Cargo.toml` | `control-ble = { path = "../control-ble" }`. | The dependency itself. Adds no third-party crate to `Cargo.lock`. |
| `src/libs/mod.rs` | `pub mod ble_host;`; `pub mod beacon;` and `pub mod ble;` removed. | The nine host-trait implementations; the two deleted modules. |
| `src/lib.rs` | The `libs::ble::{spawn_ble_event_router, BleConfig, BleEvent, BleHandle, BleMonitor}` re-export removed. | All five types are deleted. |
| `src/main.rs` | `BleMonitor::new`, `BeaconMonitor::new`, `beacon_active_connections` and `spawn_ble_event_router` removed; `start_bluetooth` + `bridge_peripheral_event` added. | The swap. Started after MQTT deliberately: the Beacon publisher writes into MQTT's channel, and starting first would put the first minutes of readings into a channel nobody reads. The ~30-step startup order is otherwise untouched (architecture.md quirk 6). |
| `src/libs/config_applier/applier.rs` | Added `apply_beacon_tag_config_and_enable` and `log_audit_public`. | AC 4: enrolling the first Beacon on a Hub with `eye.enabled: false` is two edits, and two `apply_*` calls would write the file twice — leaving a window where the tag exists and the subsystem is off — and put two audit rows in the trail for one operator action. `log_audit_public` is the door `ConfigStore::audit` needs for actions that change no config (an enrolment that was already there) and would otherwise leave no trail. |
| `src/libs/storage/thread.rs`, `src/libs/storage/reader.rs` | Added `StorageMessage::MaxBeaconReadingTs` + `StorageHandle::max_beacon_reading_ts`; `StorageReader::max_beacon_reading_ts_of` parameterises the event type, and the old hard-coded signature delegates to it. | `ReadingSink::max_reading_ts` takes the event type from its caller — the archive resume point and the advertising watermark are different questions of the same table — and the existing reader took a `&Connection`, which only code on the storage thread has. |
| `src/libs/authorization/manager.rs` | The eight `*_eye_*` `command_type`s replaced by the nine §5 verbs in all three registries; permission changed to `beacon.manage`; `beacon-provision` added. | plan.md §9.8: v2 never accepts a `*_eye_*` string. v1 reused `set_lorawan_sensor_config` to avoid re-issuing certificates; the rename forces re-issuance anyway, so this is the moment to stop a Node-management certificate authorising every Beacon operation. `set_eye_field_threshold`/`delete_eye_field_threshold` survive on purpose — they are fleet config, renamed with the rest of the threshold family in `plan/portal/control.md`. |
| `src/libs/mqtt/messages.rs` | `MqttCommand::ProvisionBeacon` added; the eight `name()` strings re-keyed. | v1 could only auto-provision on discovery, so a Beacon that failed its one attempt stayed unprovisioned until it was carried to the Hub and power-cycled. `beacon-provision` is the retry. |
| `src/libs/mqtt/monitor.rs` | The nine Beacon executor arms route through `control_ble::central::enroll` and the dial queue; `queue_beacon_dial` added; the threshold arms lose their live-config mirror; the `config_state` publish reads the raw `eye:` YAML. | One implementation behind all three origins (AC 4). `beacon-monitoring` is real now — `applier.rs:2009` said "the thread is spawned once at boot, so flipping the flag cannot start it". `beacon-recording` writes the interval **before** queueing, or the gap sync would re-start recording on a Beacon an operator just stopped (v1 defect H1). |
| `src/libs/config.rs` | `ble` and `beacon` become `serde_yaml::Value` passthrough; `ble_section_yaml()` added. | The crate's `BleConfig` is the schema and is `deny_unknown_fields`. Held as raw YAML so a malformed `ble:` section is fatal to Bluetooth alone, not to the whole agent — a Hub with a bad BLE section still measures temperature (CP-2). |
| `src/libs/display/{monitor,overview}.rs` | Read `libs::ble_host::display` instead of `libs::beacon::state`. | The LCD's Beacon rows now come from the crate's snapshot; the rendering is unchanged. |
| `src/libs/display/buttons.rs` | The ENTER-hold flow mints through `ble_host::begin_phone_session` / `end_phone_session`. | The crate owns the credential, because the crate is what checks it at FB01; a token minted here would be one the Hub refuses. It also stops v1 printing the live token to stderr on every hold — it was landing in journald and in every support bundle taken afterwards. |
| `src/libs/network/provisioning_session.rs` | `from_payload` added alongside `new`. | Same reason: this side keeps the QR matrix the LCD draws, and takes the token from the crate rather than minting a second one. |
| `src/libs/network/mod.rs` | Declares the four moved modules. | The move above. |
| `tests/node_add_simulated.rs` | Rewritten against `LorawanNodeEnroll`. | FB0D's transport half is the crate's now; what stays is this crate's enrolment path and what it persists. |
| `src/libs/lorawan/node_remove.rs` | **New** (2026-08-27). The `MqttCommand::RemoveLoRaWANNode` arm extracted verbatim; the arm now calls it. | The mirror of `node_add.rs`'s extraction, for §9.6's reason: FB12 needs this path, and reaching it by writing a fourth mutation dialect next to the inline arm would satisfy the store-layer criterion only for as long as both copies happened to agree. |
| `src/libs/ble_host/node_enroll.rs` | `LorawanNodeEnroll` gains `list` and `remove`, and takes the `fiber.config.yaml` path alongside its `NodeAddDeps` (2026-08-27). | FB11/FB12. `list` reads the persisted `lorawan.sensors[]` rather than `deps.lorawan_configs`, which is `None` on a Hub whose LoRaWAN monitor never started — and BLE is precisely the surface used on such a Hub. Same reasoning as `node_add` reading its ChirpStack credentials from disk on every call. |
| `tests/node_remove_simulated.rs` | **New** (2026-08-27). | The §9.6 assertion for the Node half: FB12's store and the signed command's store, compared, on two identically-seeded Hubs. Plus the list's source-of-truth and refusal paths. |
| `fiber.config.yaml` | `ble:` replaced with plan §4's block; `eye:` gains `en12830:` and drops `publish_interval_s` / `tag_timeout_s`. | `pin` is gone — the phone credential is an ephemeral session token, not a shared secret that outlives every operator who read it. The two dropped `eye:` keys are the same settings as `ble.central.scan_interval_s` / `beacon_timeout_s` under names that said "publish" for something governing the radio; a deployed file carrying them is accepted and warned about. |
