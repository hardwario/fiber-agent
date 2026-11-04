SUMMARY = "The provisioning station's restricted SSH account"
DESCRIPTION = "Account provisioning: the station runs the factory test and a \
read-only boot check over SSH as this account and can do nothing else \
(forced command, no forwarding, a factory socket that takes the factory test \
commands only). Its key comes per Hub through qbee and expires; the image \
carries none (plan/system/factory_test.md 4.1)."
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://50-provisioning.conf \
           file://provisioning-shell \
           file://proximos-provisioning.tmpfiles.conf \
"

S = "${UNPACKDIR}"

inherit useradd

# Static uid/gid 985 in files/proximos-{passwd,group}. A home for qbee to put the
# key in (~/.ssh/authorized_keys); /bin/sh because sshd runs the forced command
# through the account's shell.
USERADD_PACKAGES = "${PN}"
USERADD_PARAM:${PN} = "--system --create-home --home-dir /home/provisioning \
                       --shell /bin/sh --user-group provisioning"

do_install() {
    install -d ${D}${libexecdir}/proximos
    install -m 0755 ${UNPACKDIR}/provisioning-shell ${D}${libexecdir}/proximos/

    install -d ${D}${sysconfdir}/ssh/sshd_config.d
    install -m 0644 ${UNPACKDIR}/50-provisioning.conf ${D}${sysconfdir}/ssh/sshd_config.d/

    install -d ${D}${nonarch_libdir}/tmpfiles.d
    install -m 0644 ${UNPACKDIR}/proximos-provisioning.tmpfiles.conf \
        ${D}${nonarch_libdir}/tmpfiles.d/proximos-provisioning.conf

    install -d -m 0700 ${D}/home/provisioning/.ssh
    chown -R provisioning:provisioning ${D}/home/provisioning
}

# useradd leaves the password field "!", a locked account, and sshd (built
# without PAM) refuses a locked account even for a key. "*" matches no password
# and is not a lock.
pkg_postinst:${PN}() {
    sed -i 's/^provisioning:[^:]*:/provisioning:*:/' $D${sysconfdir}/shadow
}

FILES:${PN} = "${libexecdir}/proximos/provisioning-shell \
               ${sysconfdir}/ssh/sshd_config.d/50-provisioning.conf \
               ${nonarch_libdir}/tmpfiles.d/proximos-provisioning.conf \
               /home/provisioning \
"
RDEPENDS:${PN} = "openssh-sshd fiber-app"
