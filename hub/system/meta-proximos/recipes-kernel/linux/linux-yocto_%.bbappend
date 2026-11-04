# The kernel of the smoke-test machine (system/kas/kas-qemu.yml,
# plan/system/design.md §9.3). qemuarm64 builds linux-yocto, not
# linux-raspberrypi, so the fragments the CM4 kernel carries for the software
# stack are applied here too: Landlock for the Portal's sandbox, nf_tables for
# the firewall the health gate checks, and the RAUC set (dm-verity, squashfs,
# loop, nbd) for `rauc install`. The board fragments (w1, the RAK5146's
# cdc-acm) stay CM4-only. Every line names qemuarm64: linux-yocto is not the
# CM4's kernel, and nothing here may reach a machine that has not asked for it.
FILESEXTRAPATHS:prepend:qemuarm64 := "${THISDIR}/files:"

SRC_URI:append:qemuarm64 = " file://landlock.cfg file://nftables.cfg file://rauc.cfg"

# Same refusal as linux-raspberrypi_%.bbappend: without Landlock the Portal
# runs unconfined (#100), and the smoke test exists to notice that.
do_configure:append:qemuarm64() {
    grep -qx 'CONFIG_SECURITY_LANDLOCK=y' ${B}/.config \
        || bbfatal "CONFIG_SECURITY_LANDLOCK=y is not in the kernel config (landlock.cfg did not apply)"
    lsm=$(sed -n 's/^CONFIG_LSM="\(.*\)"$/\1/p' ${B}/.config)
    case ",$lsm," in
        *,landlock,*) ;;
        *) bbfatal "CONFIG_LSM=\"$lsm\" does not name landlock, so the kernel would not start it" ;;
    esac
}
