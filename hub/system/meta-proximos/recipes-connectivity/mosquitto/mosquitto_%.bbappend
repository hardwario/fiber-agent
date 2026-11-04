FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI:append = " file://proximos.conf file://10-proximos.conf file://proximos-mosquitto.tmpfiles.conf file://mosquitto-unit.sed"

PACKAGECONFIG:remove = "websockets"

do_install:append() {
    install -d ${D}${sysconfdir}/mosquitto/conf.d
    install -m 0644 ${UNPACKDIR}/proximos.conf ${D}${sysconfdir}/mosquitto/conf.d/proximos.conf
    # Two include_dirs, in this order: the static loopback half (proximos.conf),
    # then the renderer's Server-only 8883 listener. mosquitto 2.1.2 accepts
    # include_dir only in the main file, so the second cannot live in
    # proximos.conf.
    cat > ${D}${sysconfdir}/mosquitto/mosquitto.conf << 'EOF'
include_dir /etc/mosquitto/conf.d
include_dir /run/proximos/mosquitto
EOF

    install -d ${D}${nonarch_libdir}/tmpfiles.d
    install -m 0644 ${UNPACKDIR}/proximos-mosquitto.tmpfiles.conf \
        ${D}${nonarch_libdir}/tmpfiles.d/proximos-mosquitto.conf

    install -d ${D}${systemd_system_unitdir}/mosquitto.service.d
    install -m 0644 ${UNPACKDIR}/10-proximos.conf \
        ${D}${systemd_system_unitdir}/mosquitto.service.d/10-proximos.conf

    # Upstream's unit has After= and Wants=network-online.target. It is the only
    # unit on the image that pulls that target in, and with it
    # NetworkManager-wait-online, which holds every boot 60 s and marks it
    # degraded whenever a wired profile never completes (GitLab #98). A drop-in
    # can add dependencies but cannot take one back, so mosquitto-unit.sed
    # deletes the two lines from the unit itself. 10-proximos.conf orders the
    # broker After=network.target instead.
    unit="${D}${systemd_system_unitdir}/mosquitto.service"
    if [ ! -f "$unit" ]; then
        bbfatal "mosquitto: expected the upstream unit at $unit; upstream no longer installs it there"
    fi
    sed -i -f ${UNPACKDIR}/mosquitto-unit.sed "$unit"
    if grep -q 'network-online' "$unit"; then
        bbfatal "mosquitto: $unit still names network-online after mosquitto-unit.sed (upstream unit changed shape?)"
    fi
}

FILES:${PN} += "${systemd_system_unitdir}/mosquitto.service.d/10-proximos.conf \
                 ${nonarch_libdir}/tmpfiles.d/proximos-mosquitto.conf"

# The Server's 8883 listener (rendered to /run/proximos/mosquitto/8883.conf by
# proximos-render) reads /data/mosquitto/{passwd,acl} (created by
# proximos-config's data-dirs.conf) and /data/tls/server.{crt,key} (kept by
# proximos-tls-cert); the drop-in orders the unit after both. Target only:
# the recipe is BBCLASSEXTEND native, and neither has a native variant.
#
# NOT proximos-render, though the drop-in orders mosquitto after it: that
# package already reaches mosquitto through proximos-control
# (RDEPENDS:proximos-control = "mosquitto ..."), so naming it here would be a
# runtime cycle, and it is not needed - the drop-in's Wants=/After= on a unit
# that is not installed are no-ops, and the tmpfiles line below keeps the
# include_dir openable with or without a renderer.
RDEPENDS:${PN}:append:class-target = " proximos-config proximos-tls-cert"
