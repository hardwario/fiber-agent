SUMMARY = "ChirpStack LoRaWAN Network Server (SQLite build)"
DESCRIPTION = "The server half of the LoRaWAN transport: the network server the \
concentratord/mqtt-forwarder pair has been publishing uplinks at, with its \
device/gateway/application model in SQLite on /data and its gRPC API on \
127.0.0.1:8080 for control-radio. Ships with the credential bootstrap of \
plan/control/radio/lorawan.md §4 and the /data seed migration of \
plan/system/design.md §6.2, so a virgin device provisions itself without an \
`admin`/`admin` login ever existing on a listening socket."
HOMEPAGE = "https://www.chirpstack.io/"
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

# FETCHED AND PINNED, NOT COMMITTED - plan/system/design.md §3.4, and the sibling
# chirpstack-concentratord recipe carries the long version of why (v1 commits
# this same daemon as a 51 MB blob with no version and no checksum recorded
# anywhere). Here the version is the recipe filename, the checksum is below, and
# a bump is a reviewable two-line diff.
#
# THE `sqlite` INFIX IS NOT OPTIONAL: since v4.10.1 upstream publishes the
# SQLite-backend build as its own artifact, and the default artifact of the same
# version is a PostgreSQL binary that would fail at startup against this
# product's [sqlite] configuration section (lorawan.md §2). The tarball contains
# exactly one file, the binary, at its root.
#
# Version 4.19.1 is the definitive pin of lorawan.md §2; the floor is 4.10.1 (the
# sqlite artifact) and every capability this product uses - create-api-key,
# set-password, key revocation, the read-only key scope from 4.17.0 - is present
# at it. crates.io's chirpstack_api tops out at 4.19.0, which is why
# control-radio's version_pin test matches on major.minor.
SRC_URI = "https://artifacts.chirpstack.io/downloads/${BPN}/${BPN}_${PV}_sqlite_linux_arm64.tar.gz;name=bin \
           file://chirpstack.service \
           file://chirpstack-bootstrap.service \
           file://chirpstack-bootstrap.sh \
           file://005-chirpstack-seed.sh \
           file://chirpstack.sysusers.conf \
"
SRC_URI[bin.sha256sum] = "a107ac4fbb2302af90319788a454252cc517dac4943825b3ac35f8765c6853d0"

S = "${UNPACKDIR}"

inherit systemd useradd

# ENABLED, unlike both concentratord-side recipes, and the asymmetry is the
# design rather than an inconsistency. Those two are started from udev when the
# RAK5146 enumerates, because a card appearing is the event that means "a
# concentrator is wanted"; a network server has no such event. So both units
# below install and enable on every image, and the runtime gate is the rendered
# config: proximosctl render emits /run/proximos/chirpstack/chirpstack.toml only
# when radio.mode ∈ {lorawan, dual}, and both units carry
# ConditionPathExists= on it. A Hub in `off` or `p2p` mode therefore shows two
# condition-skipped units, not two failed ones.
SYSTEMD_SERVICE:${PN} = "chirpstack.service chirpstack-bootstrap.service"
SYSTEMD_AUTO_ENABLE = "enable"

# The account is declared in sysusers.d (the authority); this mirrors it for
# do_install's benefit, since useradd.bbclass runs while the image is built and
# the sysusers fragment is processed on target. Same arrangement, and the same
# flags, as portal/packaging/fiber-portal.bb - keep the two lines in step with
# files/chirpstack.sysusers.conf.
USERADD_PACKAGES = "${PN}"
USERADD_PARAM:${PN} = "--system --no-create-home --home /var/lib/chirpstack \
                       --shell /sbin/nologin --user-group chirpstack"

# Pre-built upstream binary: nothing here was compiled by this build, so the
# usual post-processing has nothing valid to do.
INHIBIT_PACKAGE_STRIP = "1"
INHIBIT_SYSROOT_STRIP = "1"
INHIBIT_PACKAGE_DEBUG_SPLIT = "1"

# The three QA checks a pinned upstream binary cannot pass, each verified against
# this exact tarball (sha256 above, 15877113 bytes, 2026-08-27) rather than
# copied from the sibling recipes:
#   ldflags      - `readelf -d` reports no dynamic section at all: upstream links
#                  statically, there is no GNU_HASH and our LDFLAGS were never
#                  involved.
#   already-stripped - the binary carries a .symtab and no .debug_* sections, the
#                  same shape as chirpstack-concentratord's; skipping it keeps
#                  the three sibling recipes identical rather than encoding a
#                  difference that is upstream's to change.
#   buildpaths   - it embeds upstream's build-script output paths
#                  (/build/chirpstack_api-<hash>/out/api/api.rs, and the same for
#                  rquickjs-sys), which are string constants in generated code
#                  rather than debug info, so no strip or path remap reaches them.
INSANE_SKIP:${PN} += "ldflags buildpaths already-stripped"

# Static aarch64 ELF, and the SRC_URI names arm64 explicitly - fail at parse time
# on anything else rather than installing a binary that cannot execute.
COMPATIBLE_HOST = "aarch64.*-linux"
PACKAGE_ARCH = "${TUNE_PKGARCH}"

# Where the migration runner looks; the same expression proximos-migrate.bb uses
# to create the directory. ${nonarch_libdir} (= /usr/lib), not ${libdir}, so a
# multilib configuration cannot move a shell script out from under the runner's
# compiled-in default.
PROXIMOS_MIGRATIONS_DIR = "${nonarch_libdir}/proximos/migrations"

do_install() {
    install -d ${D}${bindir}
    install -m 0755 ${UNPACKDIR}/chirpstack ${D}${bindir}/chirpstack

    # ${bindir} rather than ${libexecdir} for the bootstrap script, following
    # proximos-render.bb: unlike a health check or the migration runner, this is
    # something an operator legitimately runs by hand - it is the documented
    # rotation path for the admin token (delete the file, run this).
    install -m 0755 ${UNPACKDIR}/chirpstack-bootstrap.sh ${D}${bindir}/chirpstack-bootstrap.sh

    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/chirpstack.service ${D}${systemd_system_unitdir}/chirpstack.service
    install -m 0644 ${UNPACKDIR}/chirpstack-bootstrap.service ${D}${systemd_system_unitdir}/chirpstack-bootstrap.service

    # /data/chirpstack is created by this migration, not by a tmpfiles fragment -
    # see the script's own header for the data.mount race that rules tmpfiles out
    # for anything on /data, and system/design.md §6.1's table, whose
    # "Created by: tmpfiles.d" row for this path this recipe deliberately
    # contradicts.
    install -d ${D}${PROXIMOS_MIGRATIONS_DIR}
    install -m 0755 ${UNPACKDIR}/005-chirpstack-seed.sh ${D}${PROXIMOS_MIGRATIONS_DIR}/005-chirpstack-seed.sh

    install -d ${D}${libdir}/sysusers.d
    install -m 0644 ${UNPACKDIR}/chirpstack.sysusers.conf ${D}${libdir}/sysusers.d/chirpstack.conf
}

FILES:${PN} = "\
    ${bindir}/chirpstack \
    ${bindir}/chirpstack-bootstrap.sh \
    ${systemd_system_unitdir}/chirpstack.service \
    ${systemd_system_unitdir}/chirpstack-bootstrap.service \
    ${PROXIMOS_MIGRATIONS_DIR}/005-chirpstack-seed.sh \
    ${libdir}/sysusers.d/chirpstack.conf \
"

# Same reasoning as chirpstack-mqtt-forwarder.bb's RDEPENDS: none of these is
# expressed only in a unit file, because a unit's Requires= on a unit that is not
# installed is a start failure at runtime, whereas this is a packaging fact.
#
#   redis           - chirpstack.service Requires= it, and even the offline
#                     bootstrap step needs it: `create-api-key` runs
#                     storage::setup(), which sets up a Redis client before it
#                     writes the key (verified live; lorawan.md §2's correction).
#   proximos-migrate - owns /usr/lib/proximos/migrations and the unit that runs
#                     it. Without it 005-chirpstack-seed.sh is a file nobody
#                     executes, so /data/chirpstack, the api-secret and the admin
#                     password never exist and both units below fail on their
#                     first line.
#   proximos-render  - produces /run/proximos/chirpstack/chirpstack.toml, which
#                     is both units' ConditionPathExists=. Without it every boot
#                     is condition-skipped and the LoRaWAN chain silently stops
#                     at the forwarder - exactly the half-ported state this
#                     recipe set exists to end.
#
# mosquitto is deliberately absent: chirpstack.service only Wants= the broker
# (its MQTT integration reconnects indefinitely), and chirpstack-mqtt-forwarder,
# which cannot work without one, already pins it into any image that has a
# LoRaWAN chain at all.
RDEPENDS:${PN} = "redis proximos-migrate proximos-render"
