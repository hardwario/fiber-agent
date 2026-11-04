FILESEXTRAPATHS:prepend:rpi := "${THISDIR}/files:"
SRC_URI:append:rpi = " file://rauc-slots.ignorelist"

do_install:append:rpi() {
    install -m 0644 ${UNPACKDIR}/rauc-slots.ignorelist \
        ${D}${sysconfdir}/udev/mount.ignorelist.d/rauc-slots
}

FILES:${PN}-automount:append:rpi = " ${sysconfdir}/udev/mount.ignorelist.d/rauc-slots"
