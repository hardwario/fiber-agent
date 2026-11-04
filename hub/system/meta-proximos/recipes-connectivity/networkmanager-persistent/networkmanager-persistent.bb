SUMMARY = "Persist NetworkManager connection profiles across ProXimos updates"
DESCRIPTION = "Points NetworkManager's keyfile plugin at /data/network/\
system-connections so WiFi credentials and static addressing survive a RAUC \
update, and orders NetworkManager after the /data mount so it can never write \
a profile into the rootfs slot instead."
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://10-proximos-keyfile-path.conf \
           file://10-proximos-data.conf \
"

S = "${UNPACKDIR}"

INHIBIT_DEFAULT_DEPS = "1"

PACKAGE_ARCH = "${MACHINE_ARCH}"

do_install() {
    install -d ${D}${sysconfdir}/NetworkManager/conf.d
    install -m 0644 ${UNPACKDIR}/10-proximos-keyfile-path.conf \
        ${D}${sysconfdir}/NetworkManager/conf.d/10-proximos-keyfile-path.conf
    install -d ${D}${systemd_system_unitdir}/NetworkManager.service.d
    install -m 0644 ${UNPACKDIR}/10-proximos-data.conf \
        ${D}${systemd_system_unitdir}/NetworkManager.service.d/10-proximos-data.conf
}

FILES:${PN} = "${sysconfdir}/NetworkManager/conf.d/10-proximos-keyfile-path.conf \
               ${systemd_system_unitdir}/NetworkManager.service.d/10-proximos-data.conf \
"

RDEPENDS:${PN} = "networkmanager proximos-config"
