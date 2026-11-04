SUMMARY = "ProXimos Hub device tree overlay"
DESCRIPTION = "Compiles files/proximos.dtso into /boot/proximos.dtbo in the \
rootfs, where boot.scr stage 2 loads it from the active RAUC slot and applies \
it to the slot's own base DTB. It carries the board deltas that config.txt \
used to carry - the PCF85063A on the muxed i2c bus, the two UARTs the RAK5146 \
concentrator and the southbridge MCU link need, i2c1 for the accelerometer, \
and spi6 for the ST7920 LCD panel (buzzer/backlight PWM stays reserved - both \
are rppal software PWM over plain GPIO, with no kernel PWM consumer) - plus \
the Bluetooth HCI UART's CTS/RTS pins on uart0, which no config.txt line ever \
named because the GPU firmware muxes them itself on a stock boot and a stage-2 \
boot never sees that - so that a device tree change ships in a bundle instead \
of requiring the card to be pulled and re-flashed."

LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302"

SRC_URI = "file://proximos.dtso"

S = "${UNPACKDIR}"
B = "${WORKDIR}/build"

DEPENDS = "dtc-native"
INHIBIT_DEFAULT_DEPS = "1"

PACKAGE_ARCH = "${MACHINE_ARCH}"

do_compile() {
    dtc -@ -I dts -O dtb -o ${B}/proximos.dtbo ${UNPACKDIR}/proximos.dtso
}

do_install() {
    install -d ${D}/boot
    install -m 0644 ${B}/proximos.dtbo ${D}/boot/proximos.dtbo
}

FILES:${PN} = "/boot/proximos.dtbo"
