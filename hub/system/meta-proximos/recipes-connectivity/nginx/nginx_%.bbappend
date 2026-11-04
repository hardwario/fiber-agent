PACKAGECONFIG:append = " http2"

do_install:append() {
    rm -f ${D}${sysconfdir}/nginx/sites-enabled/default_server
}
