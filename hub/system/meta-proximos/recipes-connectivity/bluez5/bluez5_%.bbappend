# Raise BlueZ's default LE connection supervision timeout for beacon dialling.
#
# Teltonika EYE (BTSMP1) beacons are slow to start servicing a GATT connection:
# once the LE link is up they take longer than BlueZ's default 420 ms
# supervision timeout to answer the first ATT exchange, so the controller tears
# the link down with HCI "Connection Failed to be Established" (0x3e) before
# service discovery even begins. control-ble's provisioning dial then only ever
# sees its own 30 s session timeout ("did not complete provisioning within
# 30 s") — the real failure is one connection parameter, not the provisioning
# code. A bare `bluetoothctl connect` fails identically, and both succeed the
# moment the supervision timeout is raised.
#
# The [LE] block in main.conf is BlueZ's supported knob for this: the values are
# loaded into the kernel via MGMT_LOAD_DEFAULT_PARAMETERS *before* the adapter is
# powered, so every connection the Hub initiates as central — every beacon dial —
# uses them, with no per-connection race (BlueZ has no D-Bus API to set the
# supervision timeout per connection, and the link dies long before an L2CAP
# parameter-update could run). ConnectionSupervisionTimeout is in 10 ms units
# (Bluetooth Core spec), so 400 = 4000 ms.
#
# Hardware-verified on fiber-4e970f4a (RPi CM4 onboard controller, BlueZ 5.86):
# at 420 ms the link is dropped ~0.3 s after LE Connection Complete with zero
# ATT traffic; at 4000 ms the full attribute database resolves and the whole
# provisioning write sequence completes. Only the supervision timeout is
# changed — the default connection interval (30-50 ms) was never the problem and
# is left to the kernel default.

do_install:append() {
    conf="${D}${sysconfdir}/bluetooth/main.conf"
    if [ ! -f "$conf" ]; then
        bbfatal "bluez5: expected main.conf at $conf; upstream no longer installs it"
    fi
    if ! grep -q '^[#[:space:]]*ConnectionSupervisionTimeout[[:space:]]*=' "$conf"; then
        bbfatal "bluez5: no ConnectionSupervisionTimeout key under [LE] in main.conf (upstream layout changed?)"
    fi
    sed -i 's/^[#[:space:]]*ConnectionSupervisionTimeout[[:space:]]*=.*/ConnectionSupervisionTimeout=400/' "$conf"
    if ! grep -q '^ConnectionSupervisionTimeout=400$' "$conf"; then
        bbfatal "bluez5: failed to set ConnectionSupervisionTimeout in main.conf"
    fi
}
