# THE UNIT ONLY. This recipe used to ship a transitional POSIX-shell renderer
# (files/proximos-render.sh) standing in for plan/system/design.md §7's real
# one; that script is deleted and its ExecStart is now
# `/usr/bin/proximosctl render`, the typed renderer in
# portal/crates/proximos-render/.
#
# The deletion is safe by test, not by inspection: proximos-render's
# tests/shim_parity.rs holds the shim's exact rendered bytes as a fixture and
# diffs every render against them, so byte-for-byte parity of the one file the
# shim produced is asserted on every `cargo test` rather than argued for once
# in a commit message. The renderer additionally emits three files the shim
# never could (the control, chirpstack and forwarder configs), which is what
# the whole LoRaWAN server chain waits on.
#
# What is left here is the boot integration: a unit, its ordering, and the four
# RuntimeDirectory= entries whose lifetimes the renderer depends on. The binary
# it runs and the templates it reads both come from the proximos-control
# package (portal/packaging/fiber-portal.bb), which is why this recipe carries
# no source of its own.

SUMMARY = "Render the ProXimos runtime configuration at boot"
DESCRIPTION = "The boot unit half of plan/system/design.md §7's renderer: runs \
`proximosctl render`, which merges /usr/share/proximos/templates/defaults.yaml \
with the per-device overlay on /data, injects the secrets it reads from files \
there, and writes /run/proximos/{portal,control,chirpstack,chirpstack-mqtt-forwarder}/ \
— four configs whose presence or absence is the start condition of every service \
behind them."
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://proximos-render.service"

S = "${UNPACKDIR}"

INHIBIT_DEFAULT_DEPS = "1"

inherit systemd

SYSTEMD_SERVICE:${PN} = "proximos-render.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

do_install() {
    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/proximos-render.service ${D}${systemd_system_unitdir}/proximos-render.service
}

FILES:${PN} = "${systemd_system_unitdir}/proximos-render.service"

# systemd           the unit itself.
#
# proximos-control  the ExecStart. That package ships /usr/bin/proximosctl AND
#                   /usr/share/proximos/templates/ — the binary and its entire
#                   input set — and it owns the `proximos` account whose group
#                   /run/proximos/control/config.yaml is chowned to. Without it
#                   this unit's ExecStart does not exist.
#
# proximos-migrate  the unit half of design.md §6.2, which this unit's
#                   Requires=proximos-data-migrate.service names. A Requires= on
#                   a unit that is not installed is a start failure at runtime;
#                   this is the packaging fact behind it.
#
# fiber-portal      the consumer of /run/proximos/portal/config.yaml, and a
#                   declarer of the account it is chowned to: `proximos` since
#                   plan/portal/monolith.md §15 (it was `fiber-portal`), so
#                   fiber-portal.service can read a file containing db_key
#                   without it being world-readable. proximos-control declares
#                   the same account; either package alone is enough for the
#                   chown, this edge is for the consumer.
#
# NOT chirpstack, though the rendered chirpstack.toml is chowned to that group
# too — and the omission is load-bearing rather than an oversight. Two reasons,
# either sufficient: chirpstack_4.19.1.bb already RDEPENDS on this package (it
# needs the rendered config that is its own ConditionPathExists=), so naming it
# back would be a dependency cycle; and the renderer treats a group that does not
# resolve on the device as a legitimate answer meaning "that consumer is not
# installed" (fsutil::write_atomic), not as an error — the file is still written,
# it simply keeps root as its group, and the unit that would have read it is not
# there to care.
#
# POSIX sh is no longer in the picture at all — the shim that needed busybox
# applets is gone.
RDEPENDS:${PN} = "systemd proximos-control proximos-migrate fiber-portal"
