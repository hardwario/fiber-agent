SUMMARY = "ProXimos Hub packet filter"
DESCRIPTION = "A default-deny nftables rule set installed before any network \
interface is configured. EU MDR 2017/745 security hardening: the Hub must only \
be reachable on ports a service in this image actually listens on."
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://firewall.nft \
           file://proximos-firewall.service \
           file://firewall-server.nft \
           file://proximos-firewall-server.service \
"

S = "${UNPACKDIR}"

INHIBIT_DEFAULT_DEPS = "1"

PACKAGE_ARCH = "${MACHINE_ARCH}"

inherit systemd

# proximos-firewall-server opens TCP 8883 on a Server Hub only, gated on the
# renderer's /run/proximos/mosquitto/8883.conf (plan/control/cluster.md §5).
SYSTEMD_SERVICE:${PN} = "proximos-firewall.service proximos-firewall-server.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

do_install() {
    install -d ${D}${datadir}/proximos
    install -m 0644 ${UNPACKDIR}/firewall.nft ${D}${datadir}/proximos/firewall.nft
    install -m 0644 ${UNPACKDIR}/firewall-server.nft ${D}${datadir}/proximos/firewall-server.nft

    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/proximos-firewall.service ${D}${systemd_system_unitdir}/proximos-firewall.service
    install -m 0644 ${UNPACKDIR}/proximos-firewall-server.service ${D}${systemd_system_unitdir}/proximos-firewall-server.service
}

FILES:${PN} = "${datadir}/proximos/firewall.nft \
               ${datadir}/proximos/firewall-server.nft \
               ${systemd_system_unitdir}/proximos-firewall.service \
               ${systemd_system_unitdir}/proximos-firewall-server.service \
"

RDEPENDS:${PN} = "nftables"
