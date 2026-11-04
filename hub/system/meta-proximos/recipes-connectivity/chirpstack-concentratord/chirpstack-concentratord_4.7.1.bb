SUMMARY = "ChirpStack Concentratord (SX1302) for the RAK5146 LoRaWAN concentrator"
DESCRIPTION = "LoRa concentrator HAL daemon. Owns the RAK5146's SX1302, presents \
uplinks/downlinks on two ZeroMQ IPC sockets under /run/chirpstack-concentratord, \
and is what chirpstack-mqtt-forwarder connects to. This is the device-facing half \
of the LoRaWAN transport described in plan/control/radio/lorawan.md; the server \
half - the ChirpStack binary, redis and the credential bootstrap of that plan's \
§4/§6/§9 - is in this recipe set too, as recipes-connectivity/chirpstack and \
recipes-connectivity/redis."
HOMEPAGE = "https://www.chirpstack.io/"
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

# FETCHED AND PINNED, NOT COMMITTED - plan/system/design.md §3.4. v1 commits this
# same daemon as a 5 MB blob in meta-fiber git history beside a README that says
# "place the pre-built binary here", with no version and no checksum recorded
# anywhere; the version that shipped is therefore unknowable from the repo. Here
# the version is the recipe filename and the checksum is below, so git holds a
# pin instead of a blob and a bump is a reviewable two-line diff.
#
# upstream publishes no aarch64 asset on its GitHub releases - the arm64 tarballs
# live only on artifacts.chirpstack.io, which is also where the plan's §2 pins the
# ChirpStack server itself from. The tarball contains exactly one file, the
# binary, at its root.
SRC_URI = "https://artifacts.chirpstack.io/downloads/${BPN}/chirpstack-concentratord-sx1302_${PV}_linux_arm64.tar.gz;name=bin \
           file://chirpstack-concentratord.toml \
           file://chirpstack-concentratord.service \
           file://99-chirpstack-concentratord.rules \
"
SRC_URI[bin.sha256sum] = "328faf7e6fe6fcc9f5a50f8368fd520807ac2939214c607593318e017e857345"

S = "${UNPACKDIR}"

inherit systemd

# NOT auto-enabled, and there is deliberately no [Install] section in the unit.
# A Hub with no RAK5146 fitted is a supported configuration (plan/control/radio/
# plan.md's `off` and `p2p` modes), so a unit wanted by multi-user.target would
# spend every boot on such a device failing and restarting against a /dev node
# that will never exist - noise the health gate would have to learn to ignore.
# Instead files/99-chirpstack-concentratord.rules starts it from udev when the
# concentrator actually appears, which covers both a card present at boot and a
# card that appears later. See the unit for the rest of that chain.
SYSTEMD_SERVICE:${PN} = "chirpstack-concentratord.service"
SYSTEMD_AUTO_ENABLE = "disable"

# Pre-built upstream binary: nothing here was compiled by this build, so the
# usual post-processing has nothing valid to do.
INHIBIT_PACKAGE_STRIP = "1"
INHIBIT_SYSROOT_STRIP = "1"
INHIBIT_PACKAGE_DEBUG_SPLIT = "1"

# The three QA checks a pinned upstream binary cannot pass, each verified against
# this exact tarball rather than copied from a template:
#   ldflags      - no GNU_HASH; upstream links statically (readelf -d shows no
#                  NEEDED entries at all), so our LDFLAGS were never involved.
#   buildpaths   - the binary embeds upstream's CI paths (/home/runner/.cargo/...).
#   already-stripped - upstream ships chirpstack-mqtt-forwarder stripped and this
#                  one with a .symtab; skipping it here keeps the two sibling
#                  recipes identical rather than encoding a difference that is
#                  upstream's to change.
INSANE_SKIP:${PN} += "ldflags buildpaths already-stripped"

# Static aarch64 ELF, and the SRC_URI names arm64 explicitly - fail at parse time
# on anything else rather than installing a binary that cannot execute.
COMPATIBLE_HOST = "aarch64.*-linux"
PACKAGE_ARCH = "${TUNE_PKGARCH}"

# No RDEPENDS on libusb1, unlike v1's copy of this recipe. The binary is
# statically linked and, more to the point, the USB variant of the RAK5146 is not
# reached through libusb at all: it is a CDC-ACM device and the daemon opens
# /dev/rak5146, udev's link to its ttyACM<n> (`com_dev_path`, see the .toml and
# the rules file). v1's libusb1 dependency pulled a
# library nothing ever loads.

do_install() {
    install -d ${D}${bindir}
    install -m 0755 ${UNPACKDIR}/chirpstack-concentratord-sx1302 ${D}${bindir}/chirpstack-concentratord-sx1302

    install -d ${D}${sysconfdir}/chirpstack-concentratord
    install -m 0644 ${UNPACKDIR}/chirpstack-concentratord.toml \
        ${D}${sysconfdir}/chirpstack-concentratord/chirpstack-concentratord.toml

    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/chirpstack-concentratord.service ${D}${systemd_system_unitdir}/

    install -d ${D}${nonarch_base_libdir}/udev/rules.d
    install -m 0644 ${UNPACKDIR}/99-chirpstack-concentratord.rules \
        ${D}${nonarch_base_libdir}/udev/rules.d/99-chirpstack-concentratord.rules
}

FILES:${PN} = "\
    ${bindir}/chirpstack-concentratord-sx1302 \
    ${sysconfdir}/chirpstack-concentratord \
    ${systemd_system_unitdir}/chirpstack-concentratord.service \
    ${nonarch_base_libdir}/udev/rules.d/99-chirpstack-concentratord.rules \
"
