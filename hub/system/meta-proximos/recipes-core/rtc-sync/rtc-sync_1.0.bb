SUMMARY = "Keep the ProXimos Hub's clock and its RTC in step"
DESCRIPTION = "The Hub carries a battery-backed PCF85063A on i2c. This recipe \
takes the system clock from it at boot and writes the clock back to it - at a \
clean shutdown and periodically - so that records written before any NTP server \
is reachable still carry a plausible timestamp. EU MDR 2017/745 retention is \
only meaningful if the timestamps are."
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://rtc-sync.service \
           file://rtc-save.service \
           file://rtc-save.timer \
           file://60-rtc-sync.rules \
"

S = "${UNPACKDIR}"

INHIBIT_DEFAULT_DEPS = "1"

PACKAGE_ARCH = "${MACHINE_ARCH}"

inherit systemd

SYSTEMD_SERVICE:${PN} = "rtc-save.timer"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

do_install() {
    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/rtc-sync.service ${D}${systemd_system_unitdir}/rtc-sync.service
    install -m 0644 ${UNPACKDIR}/rtc-save.service ${D}${systemd_system_unitdir}/rtc-save.service
    install -m 0644 ${UNPACKDIR}/rtc-save.timer   ${D}${systemd_system_unitdir}/rtc-save.timer

    install -d ${D}${nonarch_libdir}/udev/rules.d
    install -m 0644 ${UNPACKDIR}/60-rtc-sync.rules ${D}${nonarch_libdir}/udev/rules.d/60-rtc-sync.rules
}

FILES:${PN} = "${systemd_system_unitdir}/rtc-sync.service \
               ${systemd_system_unitdir}/rtc-save.service \
               ${systemd_system_unitdir}/rtc-save.timer \
               ${nonarch_libdir}/udev/rules.d/60-rtc-sync.rules \
"

RDEPENDS:${PN} = "util-linux-hwclock udev"
