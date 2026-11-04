require proximos-bundle.bb

SUMMARY = "ProXimos Hub RAUC update bundle - DEVELOPMENT image"
DESCRIPTION = "proximos-bundle with proximos-image-dev in the rootfs slot: \
an update that keeps a bench Hub reachable (empty root password, ssh root \
login) instead of turning it into a fleet Hub nobody can log into. Never \
install it on a fleet or patient-facing device."

RAUC_BUNDLE_DESCRIPTION = "ProXimos Hub system bundle - DEVELOPMENT image"
RAUC_SLOT_rootfs = "proximos-image-dev"
