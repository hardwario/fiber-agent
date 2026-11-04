PROXIMOS_RAUC_KEYRING_DIR ?= "${TOPDIR}/keys/rauc"

FILESEXTRAPATHS:prepend := "${THISDIR}/files:"
FILESEXTRAPATHS:prepend := "${PROXIMOS_RAUC_KEYRING_DIR}:"

PROXIMOS_RAUC_KEYRING = "${PROXIMOS_RAUC_KEYRING_DIR}/${RAUC_KEYRING_FILE}"
BB_DONT_CACHE = "1"

python __anonymous() {
    import os

    keyring = d.getVar('PROXIMOS_RAUC_KEYRING')
    if not os.path.isfile(keyring):
        bb.fatal("RAUC dev keys missing — run system/scripts/gen-dev-keys.sh "
                 "locally or set RAUC_DEV_* CI variables\n"
                 "  expected keyring: %s\n"
                 "  locally:  system/scripts/gen-dev-keys.sh writes it to "
                 "build/keys/rauc/ca.cert.pem\n"
                 "  in CI:    set the file variables RAUC_DEV_CA_CERT, "
                 "RAUC_DEV_SIGNING_KEY and RAUC_DEV_SIGNING_CERT on the project; "
                 "system/scripts/install-rauc-keys.sh installs them into the "
                 "build directory before the build:system job starts kas"
                 % keyring)

    # Every system.conf this layer ships - the CM4 one and each per-machine
    # override under files/<machine>/ (files/qemuarm64/ for the smoke-test
    # image) - must point RAUC at the keyring the image installs; whichever
    # one FILESOVERRIDES picks is the one the image carries.
    import glob
    installed = os.path.join(d.getVar('nonarch_libdir'), 'rauc',
                             d.getVar('RAUC_KEYRING_FILE'))
    files = os.path.join(d.getVar('PROXIMOS_LAYERDIR') or '',
                         'recipes-core', 'rauc', 'files')
    confs = [os.path.join(files, 'system.conf')]
    confs += sorted(glob.glob(os.path.join(files, '*', 'system.conf')))
    for conf in confs:
        declared = None
        if os.path.isfile(conf):
            section = None
            with open(conf) as fh:
                for line in fh:
                    line = line.strip()
                    if line.startswith('[') and line.endswith(']'):
                        section = line[1:-1]
                    elif section == 'keyring' and line.startswith('path='):
                        declared = line.split('=', 1)[1].strip()
                        break
        if declared is None:
            bb.fatal("rauc-conf: no '[keyring] path=' line found in %s - the image "
                     "would ship a RAUC configuration with no trust anchor." % conf)
        if declared != installed:
            bb.fatal("rauc-conf: keyring path mismatch. The image will contain the "
                     "keyring at '%s' while %s tells RAUC to look at '%s', so no "
                     "bundle would ever verify. Change both or neither."
                     % (installed, conf, declared))
}
