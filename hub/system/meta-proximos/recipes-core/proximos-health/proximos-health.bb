SUMMARY = "ProXimos Hub health gate"
DESCRIPTION = "The userland half of the A/B update loop: a set of checks that \
decide whether the running RAUC slot has earned `rauc status mark-good`. u-boot \
spends one of the slot's three attempts per boot; without something that resets \
that counter every device eventually falls back and then runs out of slots, and \
with something that resets it unconditionally a broken update is never rolled \
back. This is that something."
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://proximos-health.sh \
           file://proximos-health.service \
           file://proximos-health.timer \
           file://10-data-mount.sh \
           file://20-clock.sh \
           file://30-qbee.sh \
           file://40-firewall.sh \
           file://50-data-grown.sh \
           file://60-services.sh \
           file://65-radio-setup.sh \
           file://70-landlock.sh \
"

S = "${UNPACKDIR}"

INHIBIT_DEFAULT_DEPS = "1"

PACKAGE_ARCH = "${MACHINE_ARCH}"

inherit systemd

SYSTEMD_SERVICE:${PN} = "proximos-health.timer"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

HEALTH_CHECKS_DIR = "${datadir}/proximos/health-checks.d"

do_install() {
    install -d ${D}${libexecdir}
    install -m 0755 ${UNPACKDIR}/proximos-health.sh ${D}${libexecdir}/proximos-health

    install -d ${D}${HEALTH_CHECKS_DIR}
    for check in ${UNPACKDIR}/[0-9][0-9]-*.sh; do
        install -m 0755 $check ${D}${HEALTH_CHECKS_DIR}/$(basename $check)
    done

    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/proximos-health.service ${D}${systemd_system_unitdir}/proximos-health.service
    install -m 0644 ${UNPACKDIR}/proximos-health.timer   ${D}${systemd_system_unitdir}/proximos-health.timer
}

FILES:${PN} = "${libexecdir}/proximos-health \
               ${HEALTH_CHECKS_DIR} \
               ${systemd_system_unitdir}/proximos-health.service \
               ${systemd_system_unitdir}/proximos-health.timer \
"

RDEPENDS:${PN} = "rauc systemd"
