# Bring up another FIBER Hub

*Type: how-to · Status: tested on fiber-a02ecc90 (CM4 Rev 1.1) · Date: 2026-11-04*

This turns a FIBER Hub board into a working FIBER Hub on stock Raspberry Pi OS:
FIBER-branded portal over HTTPS, the agent (probes, display, BLE, LoRaWAN, P2P),
and both STM32s on the firmware that matches the binaries. Until the apt
repository exists, everything is built on a Linux PC and copied to the Hub.

Times are for a PC that has done it once; the first run pulls ~25 GB of
container images and crates.

## The short way: one command

Once a release is published (section 8), a new Hub needs only:

1. Raspberry Pi OS Lite (64-bit) on the eMMC with Raspberry Pi Imager (section 2).
2. On the Hub, with internet:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/hardwario/fiber-agent/main/bootstrap.sh | sudo bash
   ```

   It downloads `fiber-agent-arm64.tar.gz` from the latest GitHub release, checks
   its sha256, unpacks it, runs `fiber setup` and reboots. Tested on
   fiber-a02ecc90 on 2026-11-04: after the reboot all health checks pass, the
   Southbridge and Northbridge run the bundled firmware, the portal answers.
3. Only for a Northbridge without nbboot (old bench firmware): section 5, case B.

Sections 0–4 below are how a release is built, and the manual path.

## 0. What you need

On the PC (Linux):

| Item | Why | Check |
|---|---|---|
| `git`, `git-lfs` | the Hub monorepo has LFS assets | `git lfs version` |
| `docker` | the aarch64 cross build and the Zephyr build run in CI's images | `docker info` |
| `bats`, `shellcheck`, `device-tree-compiler`, `rsync`, `sshpass` (optional) | this repo's tools | `make check` |
| this repository: `hub/` holds the source of the portal, the agent and the STM32 firmware | the source of every binary | `ls hub/portal/branding/fiber` |
| `rpiboot` (usbboot) and Raspberry Pi Imager | to flash the CM4 eMMC | |
| ~40 GB free disk | images and build trees | `df -h` |

For the Hub: the board, 12 V supply, a **charged battery** (the mPCIe slot that
feeds the RAK5146 runs from BAT+; below ~3.5 V the LoRaWAN card is off USB),
Ethernet or Wi-Fi.

A J-Link or ST-Link is **not** needed. Keep one around for recovery only.

## 1. Build on the PC (once per Hub software version)

From the root of this repository:

```sh
# 1a. the six Hub binaries, FIBER brand (bookworm toolchain, glibc <= 2.36)
(cd hub/portal && make cross PROXIMOS_BRAND=fiber)            # ~10 min warm, longer cold

# 1b. Northbridge (with nbboot) and Southbridge firmware, as CI builds them
tools/build-firmware.sh                                          # -> build/firmware/{nb,sb}-package

# 1c. take the local binaries as upstream "vlocal"; ChirpStack from upstream, pinned
tools/use-local-build.sh hub vlocal
tools/fetch-chirpstack.sh

# 1d. the on-device tree
FIBER_FIRMWARE_DIR=build/firmware make stage UPSTREAM_TAG=vlocal  # -> build/root
make check                                                        # all tests green
```

Once the internal package registry carries `fiber-portal-aarch64/<version>`,
1a and 1c become `tools/fetch-upstream.sh <tag>` (needs a `read_api` token in
`.secrets/gitlab-token`, mode 0600).

## 2. Flash Raspberry Pi OS

1. Put the CM4 in USB boot mode on its carrier and run `sudo rpiboot`; the eMMC
   appears as a disk.
2. Raspberry Pi Imager → *Raspberry Pi OS Lite (64-bit)* (today that is Debian 13
   "Trixie"; the stack runs on it unchanged) → the eMMC disk.
3. Imager settings: a user and password, SSH enabled (add your public key),
   Wi-Fi if there is no Ethernet. The hostname does not matter: the Hub renames
   itself `fiber-<last 8 hex of the CM4 serial>`.
4. Boot the Hub, find its address (router, or `ping <imager-hostname>.local`).

## 3. Install

```sh
make HUB=<user>@<hub-ip> deploy     # rsync build/root, then `sudo fiber setup` on the Hub
ssh <user>@<hub-ip> sudo reboot
```

`fiber setup` installs the apt packages (the Hub needs internet for this), edits
`config.txt` (overlay `fiber`, UARTs, I2C), creates `/data`, the service users
and directories, replaces Debian's `mosquitto.conf` and nginx default site
(backups kept as `*.fiber-backup`), turns cloud-init off for `/etc/hosts` and the
hostname, disables `fake-hwclock`, enables the firewall (only after checking it
keeps SSH 22 open) and the units. It is idempotent: run it again after any change.

## 4. Check

```sh
ssh <user>@<hub-ip> sudo fiber doctor
```

Expected:

```
PASS 10-data-dir.sh   PASS 20-clock.sh   PASS 40-firewall.sh
PASS 60-services.sh   PASS 65-radio-setup.sh
i2c: 18(bound) 19 51(bound)          # DS2482, accelerometer, RTC
uart: ttyAMA3 ttyAMA4
rak5146: present                      # absent = battery low or no card
rtc: rtc0 present
bt: yes
landlock: capability                  # the Raspberry Pi kernel has no Landlock; expected
```

Then open `https://<hub-ip>` (accept the warning: the CA is the Hub's own),
create the administrator on `/setup`, and check the certificate says
`O=FIBER`. If it shows another organisation (a Hub installed before the FIBER build):
`sudo rm /data/tls/*.crt /data/tls/*.key && sudo systemctl restart proximos-tls-cert nginx`.

## Test MQTT

On the Hub (over SSH), no password. Listen to everything:

```sh
mosquitto_sub -t '#' -v
```

Within seconds the Hub's own messages appear (`proximos/fiber-<serial8>/radio/lorawan`,
`.../radio/northbridge`, sensors, ...). To see one of yours, publish from a
second terminal; it shows up in the first as `test/hello it works`:

```sh
mosquitto_pub -t test/hello -m "it works"
```

## MQTT from your laptop (development)

The broker listens on `127.0.0.1:1883` only, for the portal, the agent and
ChirpStack. To reach it from the LAN:

```sh
sudo fiber mqtt enable-lan --user dev            # on the Hub; prints a generated password
# or: sudo fiber mqtt enable-lan --user dev --password <yours>
# on the laptop (sudo apt install mosquitto-clients):
mosquitto_sub -h fiber-<serial8>.local -p 1884 -u dev -P <password> -t '#' -v
sudo fiber mqtt disable-lan                      # local-only again
```

Port 1884, password required; 1883 stays closed to the network. Use the Hub's
IP address instead of `fiber-<serial8>.local` if mDNS does not resolve on your network.

## 5. STM32 firmware

The firmware packages are installed by step 3 under
`/usr/share/proximos/{southbridge,northbridge}/`; the tools under
`/usr/share/fiber/stm32/`.

### Southbridge (STM32C031, `/dev/ttyAMA4`)

Automatic: at every start `fiber_app` compares the SB's `VERSION` with the
package and reflashes it over UART (BOOT0 = GPIO26, NRST = GPIO7). Check:

```sh
sudo journalctl -u fiber | grep -E 'sb-update|<< VERSION' | tail -3
# [sb-update] attempt 1: SB now runs south-zephyr 1.1.0 g...
```

It needs the SB option byte `nBOOT_SEL = 0`. If the log says the bootloader
never answered, probe it:

```sh
sudo systemctl stop fiber
sudo /usr/share/fiber/stm32/sb_probe.sh        # want: sync 79, pid 0453
sudo systemctl start fiber
```

No answer means `nBOOT_SEL = 1` (factory default): that one bit needs SWD once
(`hub/firmware/fiber-southbridge/tools/production_provision.jlink`).

### Northbridge (STM32WL55, `/dev/ttyAMA3`)

First see what it runs. **Stop `proximos-control` for every step below**: its
HDLC traffic locks the ROM bootloader's baud detection.

```sh
cd /usr/share/fiber/stm32
sudo systemctl stop proximos-control
sudo python3 nb_uart_flash.py status
```

| `status` says | Case | Do |
|---|---|---|
| `no shell and no ROM bootloader (HDLC app, or nothing)` and `proximos-control` logs `GET_INFO ok` | A: production firmware with nbboot | nothing: `proximos-control` updates it by itself |
| `Zephyr shell (application firmware ...)` | B: old bench firmware, no nbboot | B1–B5 |
| `ROM bootloader, GET_ID 0x497` | C: blank chip (it enters the bootloader at power-on) | B3–B5 |

B1. Program the option bytes to boot the ROM bootloader (from the bench shell):

```sh
sudo python3 nb_uart_flash.py enter-rom --dry-run   # shows OPTR 0x3ffff0aa -> 0x33fff0aa
sudo python3 nb_uart_flash.py enter-rom
```

B2. **Power-cycle the Hub**: 12 V *and* battery out for a few seconds (a Linux
reboot does not reset the Northbridge). After boot, before anything else:
`sudo systemctl stop proximos-control`, then `status` must say `ROM bootloader`.

B3. Save the option bytes, write nbboot + app + `app_valid`, read every byte back:

```sh
sudo python3 nb_uart_flash.py ob-read --out ~/nb-option-bytes.bin
sudo python3 nb_uart_flash.py flash northbridge_jlink.hex
# erased pages 0..119 ... wrote 406 blocks ... read back ...: OK
```

B4. Boot from flash again (skip on a case C chip whose option bytes were never changed:
power-cycle instead):

```sh
sudo python3 nb_uart_flash.py ob-restore ~/nb-option-bytes.bin   # OPTR 0x33fff0aa -> 0x3ffff0aa
```

B5. `sudo systemctl start proximos-control`, then:

```sh
sudo journalctl -u proximos-control | grep -E 'northbridge update|GET_INFO|central running' | tail -3
# northbridge update: done — the Northbridge already runs 0.4.2+...
# modem GET_INFO ok: fw=0.4.2 ...
# P2P central running
```

From here on the Northbridge is updated over UART automatically, like the Southbridge.

**Recovery with a J-Link** (PC side, `JLinkExe`, device `STM32WL55JC_M4`):
`connect`, `halt`, `loadfile northbridge_jlink.hex`, `r`, `go`. That alone
reaches case A.

## 6. Known issues

| Symptom | Cause | Fix |
|---|---|---|
| `rak5146: absent`, no LoRaWAN | battery below ~3.5 V (`journalctl -u fiber \| grep VBAT=`) | charge or replace the battery |
| `fiber_app` logs `database is locked` once on the first boot | first-start race while it creates its key | none needed; check it does not repeat after `systemctl restart fiber` |
| a portal warning `integer 18446744073709551303` once after boot | the clock stepped back when NTP corrected the RTC | none needed |
| `landlock: capability`, portal sandbox `degraded` | the Raspberry Pi kernel is built without Landlock | accepted (needs a custom kernel) |
| `sudo` warns `unable to resolve host` right after `fiber setup` | `/etc/hosts` is now a link into `/run`; created by setup, refreshed at boot | reboot |

## 8. Publish a release

After section 1:

```sh
tools/make-release.sh build/root dist      # dist/fiber-agent-arm64.tar.gz + .sha256
```

Create a GitHub release in `hardwario/fiber-agent` and attach both files.
`bootstrap.sh` always takes the latest release
(`releases/latest/download/fiber-agent-arm64.tar.gz`). To test before
publishing, serve `dist/` and `bootstrap.sh` from the PC and run on the Hub:
`curl -fsSL http://<pc>:8000/bootstrap.sh -o b.sh && sudo FIBER_RELEASE_URL=http://<pc>:8000 bash b.sh`.

## 7. What changes later

- **apt**: the release tarball becomes Debian packages, so updates are `apt upgrade`.
- **CLI**: `fiber stm32 nb provision` will wrap section 5 (stop the service,
  `status`, the right case, restart).
- **Production**: a board that leaves production with nbboot on the Northbridge
  and `nBOOT_SEL = 0` on the Southbridge needs nothing from section 5.
