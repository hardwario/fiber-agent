do_install:append() {
    install -d ${D}${sysconfdir}/systemd/system
    ln -sf /dev/null ${D}${sysconfdir}/systemd/system/hciuart.service
    ln -sf /dev/null ${D}${sysconfdir}/systemd/system/bthelper@.service
}

SYSTEMD_AUTO_ENABLE:${PN} = "mask"
