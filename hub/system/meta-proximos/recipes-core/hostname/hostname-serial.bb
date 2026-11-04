SUMMARY = "Set the ProXimos Hub hostname from the CM4 serial number"
DESCRIPTION = "Every Hub gets a stable, unique hostname (proximos-<serial>) derived \
from the SoC serial, so that mDNS discovery, ssh and the fleet manager can tell \
devices apart without any per-device build or provisioning step."
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://hostname-serial.service \
           file://set-hostname.sh \
"

S = "${UNPACKDIR}"

INHIBIT_DEFAULT_DEPS = "1"

inherit systemd

SYSTEMD_SERVICE:${PN} = "hostname-serial.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

do_install() {
    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/hostname-serial.service ${D}${systemd_system_unitdir}/hostname-serial.service

    install -d ${D}${bindir}
    install -m 0755 ${UNPACKDIR}/set-hostname.sh ${D}${bindir}/set-hostname.sh
}

FILES:${PN} = "${systemd_system_unitdir}/hostname-serial.service \
               ${bindir}/set-hostname.sh \
"

RDEPENDS:${PN} = "systemd"
