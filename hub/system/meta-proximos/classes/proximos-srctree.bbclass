PROXIMOS_SRCTREE ?= "${S}"
PROXIMOS_SRCTREE_CODE ?= ""
PROXIMOS_SRCTREE_ASSETS ?= ""

def proximos_srctree_checksums(d, var):
    root = (d.getVar('PROXIMOS_SRCTREE') or '').rstrip('/')
    paths = (d.getVar(var) or '').split()
    return ' '.join('%s/%s:True' % (root, p) for p in paths)

PROXIMOS_SRCTREE_CODE_CHECKSUMS = "${@proximos_srctree_checksums(d, 'PROXIMOS_SRCTREE_CODE')}"
PROXIMOS_SRCTREE_ASSETS_CHECKSUMS = "${@proximos_srctree_checksums(d, 'PROXIMOS_SRCTREE_ASSETS')}"

do_compile[file-checksums] += "${PROXIMOS_SRCTREE_CODE_CHECKSUMS}"
do_install[file-checksums] += "${PROXIMOS_SRCTREE_CODE_CHECKSUMS} ${PROXIMOS_SRCTREE_ASSETS_CHECKSUMS}"

python () {
    import os

    root = (d.getVar('PROXIMOS_SRCTREE') or '').rstrip('/')
    if not root:
        bb.fatal("proximos-srctree: PROXIMOS_SRCTREE is empty. It defaults to "
                 "${S}; set it, or inherit this class after S is assigned.")

    code = (d.getVar('PROXIMOS_SRCTREE_CODE') or '').split()
    if not code:
        bb.fatal("proximos-srctree: PROXIMOS_SRCTREE_CODE is empty, so no "
                 "source file feeds do_compile's hash and edits under %s will "
                 "not rebuild. Name the compile inputs, relative to it."
                 % root)

    missing = [p for p in code + (d.getVar('PROXIMOS_SRCTREE_ASSETS') or '').split()
               if not os.path.exists(os.path.join(root, p))]
    if missing:
        bb.fatal("proximos-srctree: %s lists paths that do not exist under "
                 "%s: %s. A misspelt entry is hashed as nothing, which is the "
                 "silent-staleness bug this class prevents."
                 % (d.getVar('PN'), root, ' '.join(missing)))
}
