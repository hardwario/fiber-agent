# Bench: a Hub on stock Raspberry Pi OS

1. **Flash** Raspberry Pi OS Lite 64-bit (Bookworm) with Raspberry Pi Imager.
   CM4 eMMC: put the carrier in `rpiboot` mode (the provisioning station's
   `rpiboot` on a Linux PC exposes the eMMC as a block device), then flash it
   like an SD card. In the Imager's settings: a user with a password, SSH
   with your public key, Wi-Fi if there is no Ethernet. The hostname does not
   matter; `hostname-serial` renames the Hub `fiber-<serial8>` at boot.
2. **Fetch and stage** on your PC (needs `git-lfs`, a registry `read_api`
   token in `.secrets/gitlab-token` with mode 0600):

   ```sh
   tools/fetch-upstream.sh v1.1.0      # needs git-lfs and the token
   tools/fetch-chirpstack.sh
   make UPSTREAM_TAG=v1.1.0 stage
   ```

3. **Deploy**: `make HUB=pi@<ip> deploy` (the Hub needs `rsync`; sudo may
   ask for the Imager user's password), then `ssh pi@<ip> sudo reboot`.
4. **Check**: `ssh pi@<ip> sudo fiber doctor`, then the steps of the plan's
   Task 11, recording verbatim output in `docs/bench/<date>-bookworm-spike.md`.
