SUMMARY = "ChirpStack MQTT Forwarder"
DESCRIPTION = "Bridges chirpstack-concentratord's ZeroMQ IPC sockets to MQTT. \
Publishes gateway events (uplinks, stats) and subscribes for downlinks on the \
loopback broker, where chirpstack.service - in this recipe set since 4.19.1 - \
reads them. See plan/control/radio/lorawan.md §8.1: v2 uses the forwarder, not \
chirpstack-gateway-bridge."
HOMEPAGE = "https://www.chirpstack.io/"
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

# Fetched and pinned, not committed - plan/system/design.md §3.4. Same reasoning
# as the sibling chirpstack-concentratord recipe, which has the long version.
# NO .toml HERE ANY MORE. This recipe used to ship a static
# /etc/chirpstack-mqtt-forwarder/chirpstack-mqtt-forwarder.toml with a hardcoded
# topic_prefix="eu868". That file is now rendered to
# /run/proximos/chirpstack-mqtt-forwarder/ by `proximosctl render` from
# portal/packaging/templates/chirpstack-mqtt-forwarder/, key for key the same
# content with topic_prefix taken from radio.region - so it cannot drift from the
# region_<region>.toml the same render pass writes for ChirpStack, which was the
# one way this pair could be misconfigured without either process complaining.
SRC_URI = "https://artifacts.chirpstack.io/downloads/${BPN}/${BPN}_${PV}_linux_arm64.tar.gz;name=bin \
           file://chirpstack-mqtt-forwarder.service \
"
SRC_URI[bin.sha256sum] = "2aa3d4b6d36020354f0997ea634eebdd2b0cffccfd57d79846fdebe4cc55a398"

S = "${UNPACKDIR}"

inherit systemd

# Started by the concentratord unit's Wants=, not by multi-user.target: with
# nothing on the IPC sockets this process has nothing to forward, and on a Hub
# with no RAK5146 fitted it would fail and restart forever. The concentratord
# unit is itself udev-started, so the whole LoRaWAN chain appears and disappears
# with the card. Hence no [Install] section in the unit and "disable" here.
SYSTEMD_SERVICE:${PN} = "chirpstack-mqtt-forwarder.service"
SYSTEMD_AUTO_ENABLE = "disable"

INHIBIT_PACKAGE_STRIP = "1"
INHIBIT_SYSROOT_STRIP = "1"
INHIBIT_PACKAGE_DEBUG_SPLIT = "1"

# Verified against this tarball: statically linked (no NEEDED, so no GNU_HASH and
# our LDFLAGS were never involved), stripped by upstream, and carrying upstream's
# CI paths in the binary.
INSANE_SKIP:${PN} += "ldflags buildpaths already-stripped"

COMPATIBLE_HOST = "aarch64.*-linux"
PACKAGE_ARCH = "${TUNE_PKGARCH}"

# mosquitto is a hard runtime dependency - the forwarder's only output is MQTT to
# 127.0.0.1:1883 and it retries silently rather than exiting if nothing is
# listening. chirpstack-concentratord is equally hard: without it there are no IPC
# sockets to read. proximos-render joins them now that the configuration is
# rendered rather than shipped: it produces the toml this unit's ExecStart names
# and its ConditionPathExists= tests, and the unit Requires= it. None of the three
# is expressed only in the unit file, because a unit's Requires= on a unit that is
# not installed is a start failure at runtime, whereas this is a packaging fact.
RDEPENDS:${PN} = "mosquitto chirpstack-concentratord proximos-render"

do_install() {
    install -d ${D}${bindir}
    install -m 0755 ${UNPACKDIR}/chirpstack-mqtt-forwarder ${D}${bindir}/chirpstack-mqtt-forwarder

    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/chirpstack-mqtt-forwarder.service ${D}${systemd_system_unitdir}/
}

# No ${sysconfdir} row: nothing of this package lands on /etc any more. A device
# that carried the old static toml keeps it as an orphaned file until the next
# RAUC slot switch, which replaces the whole rootfs - there is nothing to clean up
# and nothing that reads it, since the ExecStart no longer names that path.
FILES:${PN} = "\
    ${bindir}/chirpstack-mqtt-forwarder \
    ${systemd_system_unitdir}/chirpstack-mqtt-forwarder.service \
"
