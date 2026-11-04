FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI:append = " file://10-proximos.conf"

do_install:append() {
    install -d ${D}${systemd_system_unitdir}/redis.service.d
    install -m 0644 ${UNPACKDIR}/10-proximos.conf \
        ${D}${systemd_system_unitdir}/redis.service.d/10-proximos.conf
}

FILES:${PN} += "${systemd_system_unitdir}/redis.service.d/10-proximos.conf"
