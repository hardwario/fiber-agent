FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI:append = " file://10-proximos-unenrolled.conf"

do_install:append() {
    install -d ${D}${sysconfdir}/systemd/system/qbee-agent.service.d
    install -m 0644 ${UNPACKDIR}/10-proximos-unenrolled.conf \
        ${D}${sysconfdir}/systemd/system/qbee-agent.service.d/10-proximos-unenrolled.conf
}