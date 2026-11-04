SUMMARY = "Northbridge (NB) radio firmware image for the Hub's automatic NB update"
DESCRIPTION = "The NB application image (northbridge.bin, linked at 0x0800_0800) \
and its manifest (northbridge.json), as built by the firmware:north-build CI job \
of the same pipeline. control-radio compares the manifest with what the NB \
reports in GET_INFO and re-flashes the NB through its ROM bootloader when they \
differ (plan/control/radio/northbridge_update.md). The rootfs is read-only and \
covered by the RAUC signature, so the files need no signature of their own."
# The image links Zephyr and the NB application (Apache-2.0) with the STM32Cube
# HAL (BSD-3-Clause).
LICENSE = "Apache-2.0 & BSD-3-Clause"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/Apache-2.0;md5=89aea4e17d99a7cacdbeed46a0096b10 \
                    file://${COMMON_LICENSE_DIR}/BSD-3-Clause;md5=550794465ba0ec5312d6919e203a55f9"

# Directory holding northbridge.bin + northbridge.json, or empty. It comes from
# the build environment through the `env:` block of system/kas/kas-project.yml;
# system/scripts/lib/build-env.sh resolves it (firmware/nb-package by default).
# Empty: the package is installed empty, the image carries no NB firmware and
# the Hub never updates the NB by itself - a warning says so.
NORTHBRIDGE_PKG_DIR ??= ""

# The manifest is read at parse time (PV), and the files can change under an
# unchanged path whenever firmware/nb-package is refreshed.
BB_DONT_CACHE = "1"

# Contract, plan/control/radio/northbridge_update.md §10.
NB_MANIFEST_FORMAT = "1"
NB_BOARD = "stm32wl5moc"
NB_LOAD_ADDR = "0x08000800"
NB_MAX_IMAGE_LEN = "241664"

NB_FW_DIR = "${datadir}/proximos/northbridge"

FILESEXTRAPATHS:prepend := "${@(d.getVar('NORTHBRIDGE_PKG_DIR') + ':') if d.getVar('NORTHBRIDGE_PKG_DIR') else ''}"
SRC_URI = "${@'file://northbridge.bin file://northbridge.json' if d.getVar('NORTHBRIDGE_PKG_DIR') else ''}"

S = "${UNPACKDIR}"

def nb_manifest(d):
    """The parsed northbridge.json of NORTHBRIDGE_PKG_DIR, or None if unset."""
    import json
    import os

    pkg = d.getVar('NORTHBRIDGE_PKG_DIR')
    if not pkg:
        return None
    if not os.path.isabs(pkg):
        bb.fatal("northbridge-firmware: NORTHBRIDGE_PKG_DIR=%s is not an "
                 "absolute path" % pkg)
    path = os.path.join(pkg, 'northbridge.json')
    try:
        with open(path) as fh:
            return json.load(fh)
    except (OSError, ValueError) as exc:
        bb.fatal("northbridge-firmware: cannot read %s: %s\n"
                 "  Refresh the package (system/scripts/fetch-nb-package.sh) "
                 "or unset NORTHBRIDGE_PKG_DIR to build without NB firmware."
                 % (path, exc))

def nb_pv(d):
    import re
    m = nb_manifest(d)
    if m is None:
        return '0'
    # Only a version-safe subset; do_compile rejects a malformed manifest.
    clean = lambda v: re.sub(r'[^0-9A-Za-z.]', '', str(v)) or '0'
    return '%s+g%s' % (clean(m.get('fw_version')), clean(m.get('git_sha')))

# NB firmware version + path-scoped NB commit, so the image manifest says which
# NB image the rootfs carries; "0" when it carries none.
PV = "${@nb_pv(d)}"

inherit allarch
INHIBIT_DEFAULT_DEPS = "1"

do_configure[noexec] = "1"

python do_compile() {
    import hashlib
    import os
    import re
    import zlib

    if not d.getVar('NORTHBRIDGE_PKG_DIR'):
        bb.warn("NORTHBRIDGE_PKG_DIR is empty: the image carries NO "
                "Northbridge firmware and the Hub will not update the NB by "
                "itself. See system/README.md, 'Northbridge firmware in the "
                "image'.")
        return

    src = d.getVar('UNPACKDIR')
    with open(os.path.join(src, 'northbridge.bin'), 'rb') as fh:
        image = fh.read()
    m = nb_manifest(d)

    def as_int(value):
        return int(value, 0) if isinstance(value, str) else value

    errors = []

    def expect(what, got, want):
        if got != want:
            errors.append("%s: manifest says %r, expected %r" % (what, got, want))

    sha256 = hashlib.sha256(image).hexdigest()
    crc32 = zlib.crc32(image) & 0xffffffff
    max_len = int(d.getVar('NB_MAX_IMAGE_LEN'))

    expect('format', m.get('format'), int(d.getVar('NB_MANIFEST_FORMAT')))
    expect('board', m.get('board'), d.getVar('NB_BOARD'))
    expect('load_addr', as_int(m.get('load_addr')), int(d.getVar('NB_LOAD_ADDR'), 0))
    expect('sha256', str(m.get('sha256', '')).lower(), sha256)
    expect('image_len', m.get('image_len'), len(image))
    try:
        expect('crc32', as_int(m.get('crc32')), crc32)
    except (TypeError, ValueError):
        errors.append("crc32: %r is not a number" % (m.get('crc32'),))
    if not 0 < len(image) <= max_len or len(image) % 8:
        errors.append("northbridge.bin is %d B; it must be in (0, %d] and a "
                      "multiple of 8" % (len(image), max_len))
    if not re.fullmatch(r'\d+\.\d+\.\d+', str(m.get('fw_version', ''))):
        errors.append("fw_version %r is not X.Y.Z" % (m.get('fw_version'),))
    if not re.fullmatch(r'[0-9a-f]{8}', str(m.get('git_sha', ''))):
        errors.append("git_sha %r is not 8 lowercase hex digits" % (m.get('git_sha'),))

    if errors:
        bb.fatal("northbridge-firmware: %s does not match its manifest:\n  %s"
                 % (d.getVar('NORTHBRIDGE_PKG_DIR'), "\n  ".join(errors)))
    bb.note("northbridge-firmware: NB %s (git %s), %d B, crc32 0x%08x, sha256 %s"
            % (m['fw_version'], m['git_sha'], len(image), crc32, sha256))
}

do_install() {
    if [ -n "${NORTHBRIDGE_PKG_DIR}" ]; then
        install -d ${D}${NB_FW_DIR}
        install -m 0644 ${UNPACKDIR}/northbridge.bin  ${D}${NB_FW_DIR}/northbridge.bin
        install -m 0644 ${UNPACKDIR}/northbridge.json ${D}${NB_FW_DIR}/northbridge.json
    fi
}

FILES:${PN} = "${NB_FW_DIR}"
# The package must exist for IMAGE_INSTALL even when there is nothing to put in
# it; on the Hub, a missing northbridge.json is what "no NB image" means.
ALLOW_EMPTY:${PN} = "1"
