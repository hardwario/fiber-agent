FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI:append = " file://journald.conf file://proximos-hosts.tmpfiles.conf"

dirs755 += "/data"

# /etc/hosts becomes a symlink to a runtime file (proximos-hosts.tmpfiles.conf
# says why). base-files' own hosts content - its localhost lines plus the
# `127.0.1.1 ${hostname}` line it appends - moves to hosts.base unchanged and is
# copied to /run/proximos/net/hosts at every boot, before anything resolves a
# name. glibc and musl both follow the symlink; nothing on the image writes
# /etc/hosts except set-hostname.sh, which resolves the link first.
#
# Not a conffile any more: there is nothing in /etc to preserve across a package
# upgrade, and the image is replaced whole by RAUC anyway.
CONFFILES:${PN}:remove = "${sysconfdir}/hosts"

do_install:append() {
    install -d ${D}${sysconfdir}/systemd/journald.conf.d
    install -m 0644 ${UNPACKDIR}/journald.conf ${D}${sysconfdir}/systemd/journald.conf.d/proximos.conf

    install -d ${D}${datadir}/proximos
    mv ${D}${sysconfdir}/hosts ${D}${datadir}/proximos/hosts.base
    chmod 0644 ${D}${datadir}/proximos/hosts.base
    ln -s /run/proximos/net/hosts ${D}${sysconfdir}/hosts

    install -d ${D}${nonarch_libdir}/tmpfiles.d
    install -m 0644 ${UNPACKDIR}/proximos-hosts.tmpfiles.conf \
        ${D}${nonarch_libdir}/tmpfiles.d/proximos-hosts.conf
}
