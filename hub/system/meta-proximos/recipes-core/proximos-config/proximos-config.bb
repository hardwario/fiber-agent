SUMMARY = "ProXimos Hub platform configuration"
DESCRIPTION = "Configuration files that belong to the platform rather than to \
any single package: the u-boot environment location used by RAUC's uboot \
backend, and the /data directory contract - the set of directories that must \
exist on the persistent partition before anything writes to it, plus the unit \
that creates them at the one moment when it is safe to, and the boot-time \
pruning of journal directories left under an old machine-id (#127)."
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://fw_env.config \
           file://data-dirs.conf \
           file://proximos-data-dirs.service \
           file://proximos-journal-prune.sh \
           file://proximos-journal-prune.service \
"

# wrynose puts SRC_URI files in ${UNPACKDIR} (= ${WORKDIR}/sources, per
# bitbake.conf), not in ${WORKDIR} as releases before styhead did.
S = "${UNPACKDIR}"

# Nothing is compiled, so skip the default build dependencies entirely.
INHIBIT_DEFAULT_DEPS = "1"

inherit systemd

SYSTEMD_SERVICE:${PN} = "proximos-data-dirs.service proximos-journal-prune.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

# Not allarch, despite being text: /boot/uboot.env is a property of this board's
# boot layout (which partition the firmware mounts, where u-boot was configured
# to keep its environment). A second machine would need a different file, and an
# allarch package would happily hand it this one.
PACKAGE_ARCH = "${MACHINE_ARCH}"

do_install() {
    install -d ${D}${sysconfdir}
    install -m 0644 ${UNPACKDIR}/fw_env.config ${D}${sysconfdir}/fw_env.config

    # Deliberately NOT ${nonarch_libdir}/tmpfiles.d - see the header of
    # data-dirs.conf. systemd-tmpfiles-setup.service must never find this file.
    install -d ${D}${datadir}/proximos
    install -m 0644 ${UNPACKDIR}/data-dirs.conf ${D}${datadir}/proximos/data-dirs.conf

    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/proximos-data-dirs.service ${D}${systemd_system_unitdir}/proximos-data-dirs.service
    install -m 0644 ${UNPACKDIR}/proximos-journal-prune.service ${D}${systemd_system_unitdir}/proximos-journal-prune.service

    install -d ${D}${bindir}
    install -m 0755 ${UNPACKDIR}/proximos-journal-prune.sh ${D}${bindir}/proximos-journal-prune.sh
}

FILES:${PN} = "${sysconfdir}/fw_env.config \
               ${datadir}/proximos/data-dirs.conf \
               ${systemd_system_unitdir}/proximos-data-dirs.service \
               ${systemd_system_unitdir}/proximos-journal-prune.service \
               ${bindir}/proximos-journal-prune.sh \
"

# It is configuration an operator may legitimately edit on a running device
# (e.g. while debugging a boot problem); do not silently replace their copy.
# data-dirs.conf is not a CONFFILE: it is the product's storage contract, and a
# device whose /data layout was edited by hand is not a device this build can
# reason about.
CONFFILES:${PN} = "${sysconfdir}/fw_env.config"

# systemd owns /usr/bin/systemd-tmpfiles, which is proximos-data-dirs.service's
# ExecStart. It is in every image this distro builds, but a package that would
# install without the binary it runs is a package that lies about itself.
RDEPENDS:${PN} = "systemd"

# meta-raspberrypi installs a byte-identical /etc/fw_env.config from its
# u-boot_%.bbappend, into the `u-boot` package. That package is not in any
# ProXimos image - u-boot reaches the card through the wic boot partition, not
# through the rootfs - so there is no conflict today. If u-boot is ever added to
# IMAGE_INSTALL, do_rootfs will fail on the duplicate file and the fix is to
# drop this recipe's copy, not to rename it.
