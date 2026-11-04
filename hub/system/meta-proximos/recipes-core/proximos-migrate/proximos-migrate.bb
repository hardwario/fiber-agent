SUMMARY = "ProXimos /data schema migration runner"
DESCRIPTION = "The mechanism plan/system/design.md §6.2 specifies: a single \
integer in /data/.schema, numbered idempotent scripts in \
/usr/lib/proximos/migrations/, and a boot oneshot that applies the ones this \
partition has not seen. A virgin or factory-reset /data reads as 0 and runs \
everything, so first-boot initialization and upgrade are the same code path - \
the one v1 split between a 277-line fiber-firstboot.sh and nothing at all."
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://proximos-data-migrate.sh \
           file://proximos-data-migrate.service \
"

S = "${UNPACKDIR}"

INHIBIT_DEFAULT_DEPS = "1"

inherit systemd

SYSTEMD_SERVICE:${PN} = "proximos-data-migrate.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

PROXIMOS_MIGRATIONS_DIR = "${nonarch_libdir}/proximos/migrations"

do_install() {
    install -d ${D}${libexecdir}
    install -m 0755 ${UNPACKDIR}/proximos-data-migrate.sh ${D}${libexecdir}/proximos-data-migrate

    install -d ${D}${PROXIMOS_MIGRATIONS_DIR}

    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/proximos-data-migrate.service ${D}${systemd_system_unitdir}/proximos-data-migrate.service
}

FILES:${PN} = "${libexecdir}/proximos-data-migrate \
               ${PROXIMOS_MIGRATIONS_DIR} \
               ${systemd_system_unitdir}/proximos-data-migrate.service \
"

RDEPENDS:${PN} = "systemd"
