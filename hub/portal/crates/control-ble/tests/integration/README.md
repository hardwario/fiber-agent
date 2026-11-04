# `control-ble` integration / bench recipe

`ble.yaml` here is the `ble:` block of `plan/control/ble/plan.md` §4 with `mode: dual` — the shipped
default. `control-ble-harness` drives the **real** dispatch path against it with **no agent
subsystem started**, which is what decision #12's standalone guarantee means in a form that survives
a shared process.

## One-shot

```sh
export PROXIMOS_CONFIG=crates/control-ble/tests/integration/ble.yaml
export PROXIMOS_FLEET_CONFIG=/tmp/ble-harness/fleet.yaml
export PROXIMOS_BLE_DATA=/tmp/ble-harness
mkdir -p /tmp/ble-harness && printf 'eye:\n  enabled: true\n' > "$PROXIMOS_FLEET_CONFIG"

control-ble-harness create
control-ble-harness beacon-add --mac 7C:D9:F4:13:10:DE --name "Bench 1"
control-ble-harness beacon-list --page 0 --mtu 185   # byte-for-byte what FB0F answers
```

## Live — the scan loop and the FB00 server, with nothing else in the process

```sh
PROXIMOS_BLE_HARNESS_LIVE=1 control-ble-harness status --role central
```

The process stays up until SIGINT and re-prints `status` every `scan_interval_s`. This is the mode
the bench rows need: without it, `ensure_scanning` has no loop to start and every enrollment reports
`monitoring: unavailable`.

## Three things that will waste an afternoon if you do not know them

1. **A controller cannot scan its own advertisements.** Testing the central role against a simulated
   beacon needs a **second adapter** — a USB dongle, a second machine, or the Hub plus the desktop.
   `plan/control/ble/plan.md` §Testing says the same.
2. **Nothing else may be serving FB00 or scanning on the same adapter.** On a bench Hub set
   `ble.enabled: false` and `eye.enabled: false` in the agent's config first, or `fiber_app` and the
   harness will fight over the controller and both will look broken.
3. **The beacon side is the Part S simulator** (`~/hardwario-projects/ble-tag-sim`, local-only, never
   committed). It simulates the beacon only — the phone-facing FB00 surface needs a central-role
   client, which is out of its scope and out of this crate's.

## Known environment trap, found 2026-08-26

On the development desktop (kernel `7.0.0-30-generic`, MediaTek controller) **BlueZ refuses every LE
advertisement registration**: `org.bluez.Error.Failed`, with the kernel MGMT layer reporting
`Invalid Parameters (0x0d)`. This is not a `control-ble` or simulator defect — `bluetoothctl advertise
on`, BlueZ's own reference client with nothing of ours running, fails identically.

Consequence: GATT registration works, so the configuration and recorder services are exercisable by
pointing a central at the adapter's address directly, but **proving the advertisement on air needs a
different adapter.** Use a USB dongle or the Hub.
