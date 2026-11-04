SUMMARY = "ProXimos Hub base system"
DESCRIPTION = "Locked-down base system for the ProXimos Hub (Raspberry Pi CM4): \
kernel, systemd, networking and ssh, plus the platform services that make it a \
Hub - the RAUC userland and the health gate that closes the update loop, the \
packet filter, bluetooth, the fleet agent and the clock - and the portal/agent \
workspace: the web UI and the sensor/BLE/LoRaWAN/MQTT device agent. The \
developer variant with debug tooling is a separate image recipe."
LICENSE = "MIT"

inherit core-image
IMAGE_FEATURES += "ssh-server-openssh"

RPI_EXTRA_IMAGE_BOOT_FILES:remove = "${KERNEL_IMAGETYPE}"

IMAGE_INSTALL = "\
    packagegroup-core-boot \
    kernel-image \
    kernel-devicetree \
    kernel-modules \
    rauc \
    rauc-conf \
    libubootenv-bin \
    proximos-config \
    proximos-health \
    proximos-migrate \
    proximos-render \
    fiber-portal \
    fiber-portal-nginx \
    proximos-tls-cert \
    fiber-app \
    proximos-control \
    northbridge-firmware \
    southbridge-firmware \
    mosquitto \
    chirpstack-concentratord \
    chirpstack-mqtt-forwarder \
    chirpstack \
    redis \
    networkmanager \
    networkmanager-nmcli \
    networkmanager-persistent \
    wpa-supplicant \
    iw \
    iproute2 \
    iputils \
    procps \
    e2fsprogs-e2fsck \
    ca-certificates \
    tzdata \
    avahi-daemon \
    nftables \
    proximos-firewall \
    hostname-serial \
    rtc-sync \
    util-linux-hwclock \
    proximos-dtbo \
    bluez5 \
    bluez5-noinst-tools \
    pi-bluetooth \
    linux-firmware-rpidistro-bcm43455 \
    bluez-firmware-rpidistro-bcm4345c0-hcd \
    wireless-regdb-static \
    qbee-agent \
    proximos-provisioning-access \
"

# The smoke-test machine (system/kas/kas-qemu.yml, plan/system/design.md §9.3) has
# none of the CM4 board: no RAK5146 for concentratord and the forwarder it starts,
# no Northbridge to flash, no BCM4345 radio for pi-bluetooth and the rpidistro
# firmware, no RPi device tree for proximos.dtbo. Nor any device tree at all:
# qemuarm64 sets no KERNEL_DEVICETREE (qemu generates the DT for its virt board),
# so kernel-devicetree is never packaged and do_rootfs would fail asking for it.
# A removal naming only qemuarm64, so the CM4 image above is untouched.
IMAGE_INSTALL:remove:qemuarm64 = "\
    kernel-devicetree \
    chirpstack-concentratord \
    chirpstack-mqtt-forwarder \
    northbridge-firmware \
    pi-bluetooth \
    linux-firmware-rpidistro-bcm43455 \
    bluez-firmware-rpidistro-bcm4345c0-hcd \
    proximos-dtbo \
"

BAD_RECOMMENDATIONS += "rauc-mark-good"
BAD_RECOMMENDATIONS += "dnsmasq"
