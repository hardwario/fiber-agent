SUMMARY = "ProXimos Hub RAUC update bundle"
DESCRIPTION = "The signed, verity-protected artifact that updates a ProXimos \
Hub in the field: the production rootfs image plus a manifest, installed by \
RAUC into whichever A/B slot is not running."

LICENSE = "MIT"
# The recipe fetches bundle-hook.sh (SRC_URI below), so do_populate_lic needs a
# license file; without it the build fails QA [license-checksum].
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

inherit bundle proximos-version

RAUC_BUNDLE_COMPATIBLE = "proximos-hub-cm4"

RAUC_BUNDLE_VERSION = "${PROXIMOS_VERSION}"
RAUC_BUNDLE_DESCRIPTION = "ProXimos Hub system bundle"
RAUC_BUNDLE_BUILD = "${PROXIMOS_VERSION}"
RAUC_BUNDLE_FORMAT = "verity"
RAUC_BUNDLE_SLOTS = "rootfs"
RAUC_SLOT_rootfs = "proximos-image"
RAUC_SLOT_rootfs[fstype] = "ext4"

# Carries the running system's machine-id into the new slot, so an update does
# not start a new journal directory on /data (#127). See bundle-hook.sh.
FILESEXTRAPATHS:prepend := "${THISDIR}/files:"
SRC_URI += "file://bundle-hook.sh"
RAUC_BUNDLE_HOOKS[file] = "bundle-hook.sh"
RAUC_SLOT_rootfs[hooks] = "post-install"

RAUC_KEY_FILE ?= "${TOPDIR}/keys/rauc/signing.key.pem"
RAUC_CERT_FILE ?= "${TOPDIR}/keys/rauc/signing.cert.pem"

do_bundle[file-checksums] += "${RAUC_KEY_FILE}:True ${RAUC_CERT_FILE}:True"

BUNDLE_NAME = "${BUNDLE_BASENAME}-${MACHINE}-${PROXIMOS_VERSION}"

python __anonymous() {
    import os

    layerdir = d.getVar('PROXIMOS_LAYERDIR')
    if not layerdir:
        bb.fatal("proximos-bundle: PROXIMOS_LAYERDIR is unset. It is defined by "
                 "meta-proximos/conf/layer.conf - is the layer in BBLAYERS?")

    conf = os.path.join(layerdir, 'recipes-core', 'rauc', 'files', 'system.conf')
    if not os.path.isfile(conf):
        bb.fatal("proximos-bundle: cannot check the compatible string, %s is "
                 "missing. It is the RAUC system configuration this bundle has "
                 "to agree with." % conf)

    declared = None
    with open(conf) as fh:
        for line in fh:
            line = line.strip()
            if line.startswith('compatible='):
                declared = line.split('=', 1)[1].strip()
                break

    wanted = d.getVar('RAUC_BUNDLE_COMPATIBLE')
    if declared is None:
        bb.fatal("proximos-bundle: no 'compatible=' line in %s - a RAUC system "
                 "configuration without one is invalid, and this bundle would "
                 "be built for a device that cannot state what it is." % conf)
    if declared != wanted:
        bb.fatal("proximos-bundle: compatible string mismatch. The bundle would "
                 "be built as '%s' while the image ships '%s' (%s), so no device "
                 "built from this tree would accept it. Change both or neither."
                 % (wanted, declared, conf))
}
