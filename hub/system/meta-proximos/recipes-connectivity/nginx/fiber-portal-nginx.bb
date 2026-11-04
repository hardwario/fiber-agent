SUMMARY = "nginx site config: TLS termination in front of the ProXimos Portal"
DESCRIPTION = "Installs fiber-portal-nginx.conf (portal/packaging/) as an \
nginx sites-enabled fragment: 443 with a modern TLS config, proxy_pass to the \
portal's loopback bind, and the buffering-disabled handling /events (Server- \
Sent Events) needs that a stock reverse-proxy config would silently break."
LICENSE = "CLOSED"

FILESEXTRAPATHS:prepend := "${PROXIMOS_SRC}/portal/packaging:"

SRC_URI = "file://fiber-portal-nginx.conf"

S = "${UNPACKDIR}"

INHIBIT_DEFAULT_DEPS = "1"

do_install() {
    install -d ${D}${sysconfdir}/nginx/sites-enabled
    install -m 0644 ${UNPACKDIR}/fiber-portal-nginx.conf ${D}${sysconfdir}/nginx/sites-enabled/fiber-portal.conf
}

FILES:${PN} = "${sysconfdir}/nginx/sites-enabled/fiber-portal.conf"

RDEPENDS:${PN} = "nginx"
