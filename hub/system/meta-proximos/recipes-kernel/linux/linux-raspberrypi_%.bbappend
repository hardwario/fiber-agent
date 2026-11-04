inherit rauc-integration

FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI:append:rauc-integration = " file://rauc.cfg"
SRC_URI:append = " file://nftables.cfg"
SRC_URI:append = " file://w1.cfg"
SRC_URI:append = " file://rak5146.cfg"
SRC_URI:append = " file://landlock.cfg"

# The fragment audit only warns when an option does not survive into the final
# config. Without Landlock the Portal runs unconfined (#100), so a kernel that
# lost it is not built at all.
do_configure:append() {
    grep -qx 'CONFIG_SECURITY_LANDLOCK=y' ${B}/.config \
        || bbfatal "CONFIG_SECURITY_LANDLOCK=y is not in the kernel config (landlock.cfg did not apply)"
    lsm=$(sed -n 's/^CONFIG_LSM="\(.*\)"$/\1/p' ${B}/.config)
    case ",$lsm," in
        *,landlock,*) ;;
        *) bbfatal "CONFIG_LSM=\"$lsm\" does not name landlock, so the kernel would not start it" ;;
    esac
}
