SUMMARY = "Hub-local CA and the CA-signed server certificate for the ProXimos Hub's TLS front end"
DESCRIPTION = "Boot-time oneshot that keeps /data/tls/{ca.key,ca.crt} (made once \
per Hub, until a factory reset) and /data/tls/{server.key,server.crt} (signed by \
that CA, SAN <hostname> and <hostname>.local) in place before nginx starts. \
Replaces the self-signed bring-up pair and upgrades it in place; see \
files/proximos-tls-cert.sh and plan/control/cluster.md section 6."
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://proximos-tls-cert.sh \
           file://proximos-tls-cert.service \
"

S = "${UNPACKDIR}"

INHIBIT_DEFAULT_DEPS = "1"

inherit systemd

SYSTEMD_SERVICE:${PN} = "proximos-tls-cert.service"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

do_install() {
    install -d ${D}${bindir}
    install -m 0755 ${UNPACKDIR}/proximos-tls-cert.sh ${D}${bindir}/proximos-tls-cert.sh

    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/proximos-tls-cert.service ${D}${systemd_system_unitdir}/proximos-tls-cert.service
}

FILES:${PN} = "${bindir}/proximos-tls-cert.sh \
               ${systemd_system_unitdir}/proximos-tls-cert.service \
"

# openssl for the CLI; hostname-serial is what names the Hub the certificate is
# issued for (the unit is ordered after it).
RDEPENDS:${PN} = "openssl hostname-serial"
