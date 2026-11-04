require proximos-image.bb

SUMMARY = "ProXimos Hub base system - DEVELOPMENT image (interactive login, debug tools)"
DESCRIPTION = "proximos-image plus an empty root password, ssh root login and a \
small set of debugging tools. For benches, hardware bring-up and CI inspection \
only: it is not fleet-deployable and nothing in the release process publishes \
it as one. The production image is proximos-image."

IMAGE_FEATURES += "empty-root-password allow-empty-password allow-root-login post-install-logging"
IMAGE_INSTALL:append = " strace tcpdump htop i2c-tools usbutils libgpiod-tools nano curl mosquitto-clients"
