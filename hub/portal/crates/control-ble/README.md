# `control-ble`

**Stance: clean-room** (`plan/portal/monolith.md` §11.8; `plan/control/ble/plan.md` header).

Written fresh from the plan set alone — `plan/control/ble/{plan,central,peripheral,beacon-btsmp1}.md`,
`plan/portal/control.ble.md`, plus `plan/control/radio/plan.md` §5–§6 for the shared error table and
`plan/system/design.md` for system contracts. No v1 `.rs` file is copied, transcribed or required
reading. Other permitted inputs: the pinned `bluer 0.17` D-Bus surface and the Linux L2CAP socket ABI.

**Standalone guarantee** (decision #12): this crate depends on no other crate in this workspace and
its harness (`control-ble-harness`) runs with no agent subsystem started. It is not separately
shippable — that is the stated price.

**One adapter, two roles** (`plan.md` §0.1): the same controller serves the phone (peripheral, FB00)
and the Beacons (central). Every outbound dial claims the adapter slot first; the GATT server consults
the same slot so it never mistakes our own dial for a phone.

Command namespace `control.ble`, reached as `proximosctl control.ble <command>`. Linux only (BlueZ);
`portal/Makefile` excludes it from the macOS dev loop like `control-core`.
