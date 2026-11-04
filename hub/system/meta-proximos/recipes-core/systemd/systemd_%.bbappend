FILESEXTRAPATHS:prepend := "${THISDIR}/files:"
SRC_URI:append = " file://50-data.conf \
                   file://timesyncd-proximos.conf \
"

do_install:append() {
    install -d ${D}${nonarch_libdir}/repart.d
    install -m 0644 ${UNPACKDIR}/50-data.conf ${D}${nonarch_libdir}/repart.d/50-data.conf
    install -d ${D}${sysconfdir}/systemd/timesyncd.conf.d
    install -m 0644 ${UNPACKDIR}/timesyncd-proximos.conf ${D}${sysconfdir}/systemd/timesyncd.conf.d/10-proximos.conf
}

FILES:${PN} += "${nonarch_libdir}/repart.d"
