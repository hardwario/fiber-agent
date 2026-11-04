# ProXimos portal + agent — bitbake recipe (M7).
#
# Lives in the portal component and is consumed by the meta-proximos layer
# (see packaging/README.md) so the service that owns the code also owns its
# packaging inputs — the same reason system/design.md §6.1 makes every service
# ship its own tmpfiles.d fragment.
#
# One recipe builds the whole `portal/` cargo workspace and splits it into three
# runtime packages:
#
#   ${PN}            the portal binary, its UI assets, its own unit, and the /data
#                    migration that re-owns /data/portal to `proximos` (the unit's
#                    User= since plan/portal/monolith.md §15)
#   fiber-app        the imported v1 agent — fiber_app/fiberctl/fiber-factory-reset,
#                    still under its pre-rename names (plan/portal/monolith.md
#                    §11.1-§11.3)
#   proximos-control the radio half: the proximosctl CLI and the proximos-control
#                    daemon (decision #12), their two units, the `proximos` account,
#                    the /data migration that creates its tree, and the renderer's
#                    template set
#
# This is deliberate, not incidental: cargo_bin's do_compile has no --bin/-p
# filter, so one recipe pointed at this workspace root with EXTRA_CARGO_FLAGS =
# "--workspace" (below — portal/Cargo.toml has its own [package], so plain
# `cargo build` would otherwise only build that default member) builds all six
# binaries in one `cargo build` — a second recipe would just rebuild the same
# workspace from scratch a second time, with no sstate to share.
#
# Replaces v1's `fiber-viewer.bb`, whose do_compile ran a networked 5-phase
# pip/npm dance with no hashes (system/design.md §3.5). This is a plain
# cargo_bin build against a committed Cargo.lock, with the frontend reduced to
# files on disk: there is no bundler, no node, and no npm in the picture at all
# (decision #1, plan/viewer/stack.md).

SUMMARY = "ProXimos Portal + Agent (Rust workspace: web UI, sensors/BLE/LoRaWAN/MQTT)"
DESCRIPTION = "Monitoring and control web UI for ProXimos Hubs: live dashboard, \
alarms, history, Node/Beacon management, signed device commands. Packaged \
alongside the device agent (fiber-app) it shares a cargo workspace with."
LICENSE = "CLOSED"

# In the monorepo the portal is a subdirectory of the same checkout the image
# is built from, so there is no separate SRCREV to pin: one repo, one commit,
# one version (decision #2).
#
# PV is NOT the system's PROXIMOS_VERSION (proximos-version.bbclass), which is
# `git describe --tags --always --dirty`: wiring PV to it would make *any*
# dirty file anywhere in the monorepo move PV, and PV is in WORKDIR — so every
# uncommitted edit would relocate the build directory and force a full
# 434-crate cargo rebuild, destroying the asset/code split
# proximos-srctree.bbclass sets up below.
#
# Amended 2026-10-09 (plan/system/design.md §9.1, GitLab #17): the decision this
# note asked for. PROXIMOS_TAG is the release tag, `vX.Y.Z[-pre]`, set only by a
# tag pipeline's build:system and refused by system/scripts/lib/build-env.sh
# unless `git describe` says exactly that tag — so it is never a per-edit value,
# and every other build sees it empty. On a release build PV is the tag without
# its "v", with "-" spelled "~" so that a prerelease sorts below its release
# for opkg, and the version itself (5.1.0, 5.1.0-dev.1) is compiled into the
# binaries (do_compile:prepend below). Elsewhere PV and the binaries stay at the
# crate version: a release pipeline rebuilds this recipe once, and nothing else
# does.
PROXIMOS_TAG ??= ""
PROXIMOS_RELEASE = "${@d.getVar('PROXIMOS_TAG')[1:] if d.getVar('PROXIMOS_TAG').startswith('v') else ''}"
PV = "${@(d.getVar('PROXIMOS_RELEASE') or '0.1.0').replace('-', '~')}"

# Not "${THISDIR}/files" — the default OE FILESPATH already searches
# ${FILE_DIRNAME}/files (${FILE_DIRNAME} being the *shim*'s directory once
# this recipe is `require`d from meta-proximos, not this one), and there is
# no files/ subdirectory here: fiber-portal.service and friends sit flat
# beside this recipe. THISDIR tracks whichever file is currently being
# parsed (unlike FILE_DIRNAME), so it correctly points here even via the
# shim's `require`.
FILESEXTRAPATHS:prepend := "${THISDIR}:"

# Five entries here are not plain flat filenames, and all of them rely on
# documented fetcher behaviour rather than on luck:
#
#   file://migrations/010-radio-init.sh, file://migrations/020-ble-init.sh,
#   file://migrations/030-portal-owner.sh, file://migrations/040-config-init.sh
#       bb.fetch2's file:// unpack recreates every intermediate directory of the
#       URL path at the destination (fetch2/__init__.py: "For file:// entries all
#       intermediate dirs in path must be created at destination"), so these land
#       at ${UNPACKDIR}/migrations/, NOT flat beside the units. do_install below
#       references all four with that subdirectory.
#
#   file://templates
#       A DIRECTORY, and the local fetcher handles one: FILESPATH resolution finds
#       ${THISDIR}/templates, the unpack copies the tree recursively to
#       ${UNPACKDIR}/templates/, and get_file_checksums walks it so every template
#       under it is a task-hash input — editing one re-runs do_install. This is the
#       renderer's input set (defaults.yaml + the four .j2 trees), installed to
#       /usr/share/proximos/templates, which is proximos-render's compiled-in
#       DEFAULT_TEMPLATES_DIR.
#
# Do not confuse ${UNPACKDIR}/templates (these, the renderer's) with ${S}/templates
# (the portal's minijinja UI pages, copied from the source tree further down and
# tracked by PROXIMOS_SRCTREE_ASSETS instead). Two unrelated template sets, two
# unrelated destinations.
SRC_URI = " \
    file://fiber-portal.service \
    file://fiber-portal.sysusers.conf \
    file://fiber-portal.tmpfiles.conf \
    file://fiber.service \
    file://fiber-factory-reset.service \
    file://proximos-board-off.shutdown \
    file://fiber.tmpfiles.conf \
    file://fiber-config-seed.sh \
    file://radio-setup.service \
    file://proximos-control.service \
    file://proximos-first-run.path \
    file://proximos-first-run.service \
    file://proximos-radio.sysusers.conf \
    file://proximos-radio.tmpfiles.conf \
    file://migrations/010-radio-init.sh \
    file://migrations/020-ble-init.sh \
    file://migrations/030-portal-owner.sh \
    file://migrations/040-config-init.sh \
    file://templates \
"

# All 447 crates.io dependencies of the workspace (the other 10 packages in
# Cargo.lock — portal, portal-domain, portal-storage, control-core,
# control-ble, control-radio, fiber-protocol, proximos-control,
# proximos-render, proximosctl — are local path members, already present
# under ${S}), so do_fetch pulls the whole dependency graph and do_compile
# below can build with no network at all. THISDIR here is safe to use
# directly (unlike in FILESEXTRAPATHS above): `require` resolves its argument
# immediately while this exact file is being parsed, it does not wait for
# task-execution time.
require ${THISDIR}/fiber-portal-crates.inc

# Set by the layer to the portal subdirectory of the monorepo checkout.
PORTAL_SRC ?= "${PROXIMOS_SRC}/portal"
S = "${PORTAL_SRC}"

# ${S} is a working tree with no fetcher on it (see the SRC_URI note above:
# only the packaging file:// entries are checksummed), so without this class
# bitbake's task hashes do not see a single line of the code or the UI. They
# did not: a template, stylesheet or script edit left do_install's stamp valid,
# the task never re-ran, and /usr/share/fiber-portal kept whatever the first
# build put there. proximos-srctree.bbclass closes that by declaring the tree's
# inputs as [file-checksums]; read its header for why that rather than a hash
# in a variable.
#
# The two lists are split by which task each input feeds, so that editing a
# page does not pay for a cargo rebuild:
#
#   _ASSETS  packaging inputs only -> do_install
#   _CODE    compile inputs        -> do_compile *and* do_install
#
# branding/ is code too: crates/proximos-brand's build script compiles the
# brand's names, logo, favicon and LCD splash into the binaries (decision #50,
# plan/portal/branding.md). The image builds the default brand.
#
# i18n/ and migrations/ are code despite being .json and .sql: src/i18n.rs
# embeds every catalogue with include_str!, and portal-storage's lib.rs embeds
# the schema with refinery::embed_migrations!. Neither ships as a file, and
# editing either has to invalidate the build, not just the packaging.
#
# assets/css/ and vendor/ are on neither list on purpose: they are inputs to
# scripts/build-css.sh, whose *output* (static/css/app.css) is committed and
# tracked as an asset. The do_install:prepend below is what notices when that
# output has gone stale. tools/ (fetched) and target/ (build output) are
# likewise deliberately untracked.
#
# A new top-level directory under portal/ that ships or compiles in must be
# added to one of these two lists; the class fails the parse on a path that
# does not exist, but it cannot know about one nobody named.
inherit proximos-srctree
PROXIMOS_SRCTREE_CODE = "src crates i18n branding migrations Cargo.toml Cargo.lock rust-toolchain.toml"
PROXIMOS_SRCTREE_ASSETS = "templates static"

inherit cargo_bin cargo_common systemd useradd

# Vendored offline build: the crate:// fetches above run in do_fetch, which
# has network; do_compile does not (v1's fiber.bb left do_compile[network] =
# "1" and let cargo hit crates.io live — unreproducible, which is exactly what
# the crate:// list above avoids).
#
# cargo_bin (meta-rust-bin, chosen so building this doesn't also require
# building the full rust-native toolchain from source) and cargo_common
# (oe-core, the only source of the crates.io-vendoring config.toml) each
# `EXPORT_FUNCTIONS do_configure`, so whichever is inherited last would
# silently win and the other's setup would never run. Calling both here
# explicitly avoids that: cargo_common_do_configure writes
# ${CARGO_HOME}/config.toml with `[source.crates-io] replace-with = "bitbake"`
# pointing at the crate:// fetches' unpack directory (bitbake's crate fetcher
# hardcodes that directory as ${UNPACKDIR}/cargo_home/bitbake — matching
# cargo_bin's CARGO_HOME only because COMPATIBLE_PACKDIR resolves to
# UNPACKDIR on wrynose); cargo_bin_do_configure writes the CC/CXX/linker
# wrapper scripts cargo_bin_do_compile's CARGO_TARGET_*_LINKER env vars point
# at. The env vars win over config.toml's own (self-)generated `[target.*]
# linker = ...` lines regardless of order — cargo always prefers env vars —
# so the two configure steps don't actually fight over anything.
do_configure() {
    cargo_common_do_configure
    cargo_bin_do_configure
}

do_compile[network] = "0"

# The release version, compiled in through option_env!: PROXIMOS_VERSION for the
# portal (/health, the page footer), control-radio (the daemon status,
# proximosctl --version) and control-ble (the BLE device info); FIBER_VERSION for
# control-core, the v1 agent (app_version and FB07). Both are cleared on any
# other build, so that nothing the build host happens to export can leak a
# version into a binary whose task hash does not know about it.
do_compile:prepend() {
    if [ -n "${PROXIMOS_RELEASE}" ]; then
        export PROXIMOS_VERSION="${PROXIMOS_RELEASE}"
        export FIBER_VERSION="${PROXIMOS_RELEASE}"
    else
        unset PROXIMOS_VERSION FIBER_VERSION
    fi
}

# cargo_bin's do_compile has no --bin/-p filter, but portal/Cargo.toml is a
# non-virtual workspace (it has its own [package]), so plain `cargo build`
# only builds cargo's default-members — the root "portal" package — and
# silently skips the control-core, proximosctl and proximos-control members
# entirely (no fiber_app/fiberctl/fiber-factory-reset/proximosctl/
# proximos-control binaries land in CARGO_BINDIR). --workspace overrides that
# default so the one `cargo build` really does produce all six.
EXTRA_CARGO_FLAGS += "--workspace"

# Needed now that control-core (imported from v1's application, roadmap
# §11.1) is part of this workspace build — mirrors v1's
# recipes-core/application/fiber.bb, which needed the same three for the same
# reasons:
#   openssl           -> rusqlite "bundled-sqlcipher" links system OpenSSL;
#                        control-core also vendors OpenSSL itself (rumqttc TLS)
#   dbus              -> bluer (in-app BLE GATT server) links libdbus-1
#   protobuf-native    -> control-core's build.rs calls prost-build, which
#                        needs a native `protoc` at build time
DEPENDS += "openssl dbus protobuf-native"

# libdbus-sys's build.rs (a bluer dependency, see above) finds libdbus-1 via
# pkg-config, not by linking against a path we could just hand it — and
# unlike autotools/cmake/meson recipes, cargo_bin.bbclass never inherits
# pkgconfig.bbclass on its own, so PKG_CONFIG_PATH/PKG_CONFIG_SYSROOT_DIR are
# simply unset for a plain `inherit cargo_bin` recipe (cargo_bin_do_compile
# sets PKG_CONFIG_ALLOW_CROSS=1, which only tells pkg-config-rs cross-compiling
# is fine — it does not point pkg-config at the target sysroot).
inherit pkgconfig
export PROTOC = "${STAGING_BINDIR_NATIVE}/protoc"
export PROTOC_INCLUDE = "${STAGING_INCDIR_NATIVE}"

# Default SYSTEMD_PACKAGES is "${PN}" (systemd.bbclass) — with two more
# packages now shipping units, they have to be listed explicitly or their
# SYSTEMD_SERVICE below is never read and their units never get enabled.
SYSTEMD_PACKAGES = "${PN} fiber-app proximos-control"
SYSTEMD_SERVICE:${PN} = "fiber-portal.service"
SYSTEMD_SERVICE:fiber-app = "fiber.service fiber-factory-reset.service"

# BOTH enabled, and neither is conditional on the radio being fitted or wanted:
# each carries its own runtime gate instead. radio-setup.service is a oneshot that
# treats "no radio" as degraded-green (exit 0, `provisioned` marker withheld), and
# proximos-control.service has ConditionPathExists=/run/proximos/control/config.yaml
# — the renderer's output — so on a Hub the renderer left without a `radio:` section
# it is condition-skipped rather than failed. Same arrangement, same reasoning, as
# the chirpstack pair in system/meta-proximos/recipes-connectivity/chirpstack/.
SYSTEMD_SERVICE:proximos-control = "proximos-control.service radio-setup.service proximos-first-run.path"
# proximos-first-run.path is the enabled half of decision #37's first-run applier; the
# .service it starts has no [Install] section and is not listed, because it runs only when
# the path unit sees the portal's request file. The pair ships in this package because the
# binary they run (proximosctl first-run-apply) and the overlay they write belong to the
# renderer side.
SYSTEMD_AUTO_ENABLE = "enable"

# The accounts are declared in sysusers.d (the authority); these mirror them for
# do_install's chown, since useradd.bbclass runs before the sysusers fragment
# is processed on target. fiber-app has no equivalent: it keeps v1's root
# posture for this transitional package (see fiber.service's own comment).
#
# Both lines declare the same account and must stay identical, in step with
# proximos-radio.sysusers.conf and fiber-portal.sysusers.conf, which carry the
# same `u proximos - "ProXimos control" /var/lib/proximos -`. One account for the
# whole merged component per decision #9. The radio half and the portal run as it
# today (the portal since monolith.md §15, issue #88 — its in-process
# control.radio seam reads what `proximos` owns); the agent follows when
# monolith.md §11.4-§11.6 lands. ${PN} declares it rather than RDEPENDing on
# proximos-control for it, so the portal package is complete on its own;
# useradd.bbclass sees the second declaration of an existing user and skips it.
#
# The `fiber-portal` account this line used to create is gone. Nothing on the
# rootfs was ever owned by it, and what it owned on /data is re-owned by
# migrations/030-portal-owner.sh.
USERADD_PACKAGES = "${PN} proximos-control"
USERADD_PARAM:${PN} = "--system --no-create-home --home /var/lib/proximos \
                       --shell /sbin/nologin --user-group proximos"
USERADD_PARAM:proximos-control = "--system --no-create-home --home /var/lib/proximos \
                                  --shell /sbin/nologin --user-group proximos"

# fiber-app and proximos-control claim first, so their files never fall into the
# ${PN} catch-all — and for proximos-control that is not a tidiness point but the
# whole reason the line names it. FILES:${PN} below only *appends* to bitbake.conf's
# default, which already globs ${bindir}/*; without this prepend both proximosctl
# and proximos-control would be swallowed by that glob into the fiber-portal
# package, and proximos-control would ship two units, a sysusers fragment and a
# migration with no binaries for any of them to run.
PACKAGES =+ "fiber-app proximos-control"

FILES:${PN} += " \
    ${bindir}/fiber-portal \
    ${datadir}/fiber-portal \
    ${systemd_unitdir}/system/fiber-portal.service \
    ${libdir}/sysusers.d/fiber-portal.conf \
    ${libdir}/tmpfiles.d/fiber-portal.conf \
    ${PROXIMOS_MIGRATIONS_DIR}/030-portal-owner.sh \
"

FILES:fiber-app = " \
    ${bindir}/fiber_app \
    ${bindir}/fiberctl \
    ${bindir}/fiber-factory-reset \
    ${datadir}/fiber \
    ${systemd_unitdir}/system/fiber.service \
    ${systemd_unitdir}/system/fiber-factory-reset.service \
    ${systemd_unitdir}/system-shutdown/proximos-board-off \
    ${libdir}/tmpfiles.d/fiber.conf \
"

# ${systemd_system_unitdir} and ${systemd_unitdir}/system are the same directory
# (/usr/lib/systemd/system); the newer spelling is used for the new files because
# it is what the layer's own recipes (chirpstack, proximos-migrate,
# proximos-render) use, and every path below matches a do_install destination
# further down verbatim.
FILES:proximos-control = " \
    ${bindir}/proximosctl \
    ${bindir}/proximos-control \
    ${systemd_system_unitdir}/proximos-control.service \
    ${systemd_system_unitdir}/radio-setup.service \
    ${systemd_system_unitdir}/proximos-first-run.path \
    ${systemd_system_unitdir}/proximos-first-run.service \
    ${libdir}/sysusers.d/proximos-radio.conf \
    ${libdir}/tmpfiles.d/proximos-radio.conf \
    ${PROXIMOS_MIGRATIONS_DIR}/010-radio-init.sh \
    ${PROXIMOS_MIGRATIONS_DIR}/020-ble-init.sh \
    ${PROXIMOS_MIGRATIONS_DIR}/040-config-init.sh \
    ${datadir}/proximos/templates \
"

# Where the migration runner looks. Same expression, same reasoning, as
# chirpstack_4.19.1.bb and proximos-migrate.bb: ${nonarch_libdir} (= /usr/lib),
# not ${libdir}, so a multilib configuration cannot move a shell script out from
# under the runner's compiled-in default. The directory itself is owned by
# proximos-migrate (which creates it empty on purpose); this recipe contributes
# four files into it — 010 for the radio component, 020 for BLE and 040 for the device
# overlay's directory (all in proximos-control), 030 for the portal's /data ownership
# (in ${PN}) — exactly as
# the chirpstack recipe contributes 005. Numbers are global across every recipe
# that contributes: the runner refuses two scripts that share one.
PROXIMOS_MIGRATIONS_DIR = "${nonarch_libdir}/proximos/migrations"

do_install:append() {
    # Reference ${UNPACKDIR}, not ${WORKDIR}, for every file:// source below,
    # or `install` fails with "No such file or directory": on wrynose
    # UNPACKDIR is where SRC_URI's file:// entries land, and oe-core's
    # bitbake.conf sets it to ${WORKDIR}/sources — a directory of its own, not
    # ${WORKDIR} itself. meta-rust-bin's cargo_bin.bbclass follows it for
    # CARGO_HOME/B/WRAPPER_DIR via its COMPATIBLE_PACKDIR, which is why the
    # cargo build tree lives under there too.
    #
    # An earlier version of this comment claimed UNPACKDIR *collapses onto
    # ${S}* because S is an absolute path outside ${WORKDIR}. It does not:
    # bitbake.conf assigns UNPACKDIR ??= "${WORKDIR}/sources" unconditionally,
    # with no reference to S. Worth stating, because that premise also implies
    # do_unpack[cleandirs] = "${UNPACKDIR}" (base.bbclass) would wipe the
    # tracked portal/ tree on every unpack — it does not, and nobody should go
    # looking for a bug that isn't there.
    #
    # cargo_bin_do_install (this class's base do_install) already drops every
    # binary the workspace builds — portal, fiber_app, fiberctl,
    # fiber-factory-reset — into ${D}${bindir} under their real cargo names.
    # fiber_app/fiberctl/fiber-factory-reset need no further handling: their
    # real names are already what FILES:fiber-app above expects. Only the
    # portal binary needs renaming, since fiber-portal.service's ExecStart=
    # (and this recipe's own FILES:${PN}) expect the pre-existing
    # "fiber-portal" identity, not cargo's real binary name "portal" — this
    # was found to be a real mismatch: the previous do_install:append copied
    # from a source path that does not exist ("fiber-portal", not "portal").
    mv ${D}${bindir}/portal ${D}${bindir}/fiber-portal

    # Runtime assets. Templates are loaded at runtime by minijinja (not
    # compiled in — plan/viewer/stack.md rejects Askama for exactly this), and
    # static/ holds the vendored htmx/Alpine/ECharts plus the compiled
    # stylesheet. `static/css/app.css` is a committed build artifact; CI fails
    # if it drifts from the templates (see .gitlab-ci.yml portal:css).
    install -d ${D}${datadir}/fiber-portal
    cp -r ${S}/templates ${D}${datadir}/fiber-portal/
    cp -r ${S}/static ${D}${datadir}/fiber-portal/
    chown -R root:root ${D}${datadir}/fiber-portal

    install -d ${D}${systemd_unitdir}/system
    install -m 0644 ${UNPACKDIR}/fiber-portal.service ${D}${systemd_unitdir}/system/

    install -d ${D}${libdir}/sysusers.d
    install -m 0644 ${UNPACKDIR}/fiber-portal.sysusers.conf ${D}${libdir}/sysusers.d/fiber-portal.conf

    install -d ${D}${libdir}/tmpfiles.d
    install -m 0644 ${UNPACKDIR}/fiber-portal.tmpfiles.conf ${D}${libdir}/tmpfiles.d/fiber-portal.conf

    # fiber-app: default config/scripts, mirroring v1's
    # recipes-core/application/fiber.bb do_install:append exactly (paths
    # unchanged — no rename sweep in this transitional package). These land
    # read-only under /usr/share/fiber/config, updated by every RAUC bundle;
    # fiber-config-seed.sh (installed below, run from fiber.service's
    # ExecStartPre) is what actually gets a copy onto /data/fiber/config on a
    # device that doesn't have one yet — install alone does not put anything
    # on /data, which is the persistent, never-clobbered location the running
    # agent actually reads (config.rs::Config::load_default).
    install -d ${D}${datadir}/fiber/config
    install -d ${D}${datadir}/fiber/scripts
    install -m 0644 ${S}/crates/control-core/fiber.config.yaml ${D}${datadir}/fiber/config/
    install -m 0644 ${S}/crates/control-core/fiber.sensors.config.yaml ${D}${datadir}/fiber/config/
    install -m 0644 ${S}/crates/control-core/authorized_signers.yaml ${D}${datadir}/fiber/config/
    install -m 0755 ${S}/crates/control-core/scripts/init-signer.sh ${D}${datadir}/fiber/scripts/
    install -m 0755 ${UNPACKDIR}/fiber-config-seed.sh ${D}${datadir}/fiber/scripts/

    install -d ${D}${systemd_unitdir}/system
    install -m 0644 ${UNPACKDIR}/fiber.service ${D}${systemd_unitdir}/system/
    install -m 0644 ${UNPACKDIR}/fiber-factory-reset.service ${D}${systemd_unitdir}/system/
    install -d ${D}${systemd_unitdir}/system-shutdown
    install -m 0755 ${UNPACKDIR}/proximos-board-off.shutdown ${D}${systemd_unitdir}/system-shutdown/proximos-board-off

    install -d ${D}${libdir}/tmpfiles.d
    install -m 0644 ${UNPACKDIR}/fiber.tmpfiles.conf ${D}${libdir}/tmpfiles.d/fiber.conf

    # proximos-control: the radio half. cargo_bin_do_install has already dropped
    # ${bindir}/proximosctl and ${bindir}/proximos-control under their real cargo
    # names, which is what FILES:proximos-control expects — neither needs the
    # rename the portal binary above does.
    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/proximos-control.service ${D}${systemd_system_unitdir}/proximos-control.service
    install -m 0644 ${UNPACKDIR}/radio-setup.service ${D}${systemd_system_unitdir}/radio-setup.service
    install -m 0644 ${UNPACKDIR}/proximos-first-run.path ${D}${systemd_system_unitdir}/proximos-first-run.path
    install -m 0644 ${UNPACKDIR}/proximos-first-run.service ${D}${systemd_system_unitdir}/proximos-first-run.service

    install -d ${D}${libdir}/sysusers.d
    install -m 0644 ${UNPACKDIR}/proximos-radio.sysusers.conf ${D}${libdir}/sysusers.d/proximos-radio.conf

    install -d ${D}${libdir}/tmpfiles.d
    install -m 0644 ${UNPACKDIR}/proximos-radio.tmpfiles.conf ${D}${libdir}/tmpfiles.d/proximos-radio.conf

    # 0755, matching chirpstack_4.19.1.bb's 005 exactly, even though the runner
    # does not strictly need it: proximos-data-migrate invokes each migration as
    # `sh -e "$script"` rather than executing the file, deliberately, so that a
    # migration that lost its executable bit still runs. The mode is here for the
    # operator who reads the directory, not for the runner. The subdirectory in the
    # source path is the fetcher's doing, not ours — see the SRC_URI comment.
    #
    # These run on target, where the userland is busybox: a migration may use only
    # applets every busybox build has. `install` is NOT one of them — this image's is
    # built without that applet, and 010 shipping with `install -d` in it failed
    # proximos-data-migrate.service and, through its Requires= chain, the renderer and
    # the portal. The scripts' own headers carry the detail; this note is here because
    # this recipe is where a new migration gets added.
    install -d ${D}${PROXIMOS_MIGRATIONS_DIR}
    install -m 0755 ${UNPACKDIR}/migrations/010-radio-init.sh \
        ${D}${PROXIMOS_MIGRATIONS_DIR}/010-radio-init.sh
    install -m 0755 ${UNPACKDIR}/migrations/020-ble-init.sh \
        ${D}${PROXIMOS_MIGRATIONS_DIR}/020-ble-init.sh
    # ${PN}'s, not proximos-control's: it moves the portal's own tree to the
    # portal unit's User=, so it has to arrive with that unit (FILES:${PN}).
    install -m 0755 ${UNPACKDIR}/migrations/030-portal-owner.sh \
        ${D}${PROXIMOS_MIGRATIONS_DIR}/030-portal-owner.sh
    # proximos-control's, with the first-run applier it prepares for (decision #37).
    install -m 0755 ${UNPACKDIR}/migrations/040-config-init.sh \
        ${D}${PROXIMOS_MIGRATIONS_DIR}/040-config-init.sh

    # The renderer's template set, tree and all: /usr/share/proximos/templates is
    # proximos-render's DEFAULT_TEMPLATES_DIR (crates/proximos-render/src/lib.rs),
    # overridable only by the PROXIMOS_TEMPLATES env var that nothing on the device
    # sets. `cp -r <dir>/.` copies the *contents*, so the subdirectories
    # (portal/, control/, chirpstack/, chirpstack-mqtt-forwarder/) are preserved
    # without nesting a second `templates/` inside the destination.
    #
    # Modes are normalised rather than inherited from the checkout: `cp -r`
    # preserves whatever the working tree happened to have, and these files are
    # read by a root oneshot and by nothing else. There is no secret in any of
    # them — the secrets are read from /data at render time and injected, never
    # templated in (defaults.yaml's header says so at length).
    install -d ${D}${datadir}/proximos/templates
    cp -r ${UNPACKDIR}/templates/. ${D}${datadir}/proximos/templates/
    chown -R root:root ${D}${datadir}/proximos/templates
    find ${D}${datadir}/proximos/templates -type d -exec chmod 0755 {} +
    find ${D}${datadir}/proximos/templates -type f -exec chmod 0644 {} +
}

# static/css/app.css is a committed build artifact (see do_install:append),
# so a template that introduces a Tailwind utility ships unstyled unless
# scripts/build-css.sh was re-run. M0's stylesheet shipped for six milestones
# missing every alert-*/badge-* utility exactly this way, with all tests green.
#
# A warning, not a fatal: CI's portal:css job (.gitlab-ci.yml) rebuilds the
# stylesheet and fails on any difference, which is the real gate, and mtimes
# after a fresh clone or a `git checkout` are arbitrary enough that failing a
# local build on them would cost more than it catches. This is here to put the
# fact in the build log of the one task that packages the file.
#
# `find -newer` on the committed output vs its inputs, and note the plain $var
# below: ${...} in a bitbake shell function is expanded by bitbake at parse
# time, not by the shell.
do_install:prepend() {
    stale="$(find ${S}/templates ${S}/assets/css ${S}/vendor -type f \
                  -newer ${S}/static/css/app.css -print -quit 2>/dev/null || true)"
    if [ -n "$stale" ]; then
        bbwarn "static/css/app.css is older than $stale — the packaged UI may be" \
               "missing utilities the templates use. Run portal/scripts/build-css.sh" \
               "(or system/scripts/rebuild-portal.sh, which does it first) and commit" \
               "the result."
    fi
}

# The portal binary self-confines with Landlock (src/sandbox.rs). Nothing is
# required of the image for that beyond a kernel with Landlock in its LSM
# list — but note that adding AppArmor to CONFIG_LSM without naming landlock
# silently drops it (security/README.md spike S3). The image smoke test
# asserts the journal says "landlock: confinement enforced" (an anonymous
# /health no longer carries sandbox.state, GitLab #104). control-core has no
# equivalent self-confinement yet (roadmap §11.6, not started).
#
# proximos-migrate: the runner of 030-portal-owner.sh, which this package ships.
# Without it the migration is a file nobody executes, and on a unit upgraded from
# the `fiber-portal` era the portal — now `proximos` — cannot open a database
# still owned by the old account. The same reasoning proximos-control gives below
# for 010/020.
RDEPENDS:${PN} += "nginx proximos-migrate"

# fiber-app: mirrors v1's recipes-core/application/fiber.bb RDEPENDS — bash
# for init-signer.sh's shebang (file-rdeps QA otherwise has no provider for
# it), bluez5 for the in-app BLE GATT server, networkmanager for nmcli-based
# Wi-Fi provisioning.
RDEPENDS:fiber-app += "bash bluez5 networkmanager"

# proximos-control:
#
#   mosquitto        radio-setup.service Requires= it (not Wants=): gateway EUI
#                    discovery is a retained-topic read, so with no broker the run
#                    reports "no radio" for the wrong reason. Named here and not
#                    only in the unit, because a Requires= on a unit that is not
#                    installed is a start failure at runtime whereas this is a
#                    packaging fact — the same rule the chirpstack recipes state.
#   proximos-migrate owns /usr/lib/proximos/migrations and the unit that runs what
#                    is in it. Without it 010-radio-init.sh is a file nobody
#                    executes, /data/proximos/radio never exists, and both units
#                    fail on their first write. Exactly chirpstack_4.19.1.bb's
#                    reasoning for 005.
#
# proximos-render is deliberately NOT here even though proximos-control.service
# Requires= it. The edge runs the other way: proximos-render.bb RDEPENDS on THIS
# package, because /usr/bin/proximosctl is its ExecStart and this package ships the
# templates it reads. Naming it back would make that a cycle. Every image that has
# a renderer therefore has this package, and proximos-image.bb installs both by
# name rather than leaving either to a transitive edge.
RDEPENDS:proximos-control = "mosquitto proximos-migrate"

# Same root cause v1's recipes-core/application/fiber.bb hit: control-core's
# build.rs (prost-build) and its vendored-OpenSSL build script bake the
# build-time path into generated code as plain string constants, not debug
# info, so `--remap-path-prefix` (which only rewrites rustc's own compile-unit
# and panic-location paths) leaves it in the *stripped* fiber_app binary
# itself, not just its debug symbols. Needs skipping on both:
#   fiber-app   — the actual release binary (this QA hit)
#   ${PN}-dbg   — debug symbols for every binary the workspace builds (portal's
#                 own and fiber-app's alike share this one recipe-wide debug
#                 package, regardless of which runtime package their
#                 non-debug files end up in), a different QA hit than v1 ever
#                 saw because v1 set INHIBIT_PACKAGE_DEBUG_SPLIT and this
#                 recipe doesn't (systemd/useradd packaging conventions expect
#                 a normal -dbg).
#   proximos-control — the same defect, a different generator. Both binaries in
#                 this package link control-radio, which takes chirpstack_api;
#                 that crate's build.rs runs prost/tonic codegen into OUT_DIR and
#                 the generated modules are pulled in with include!, so the
#                 absolute OUT_DIR path lands in the binary as a string constant.
#                 Verified rather than assumed: `strings` on a locally built
#                 proximosctl reports eight distinct
#                 .../target/<profile>/build/chirpstack_api-<hash>/out/... paths,
#                 and under bitbake that prefix is WORKDIR — which is precisely
#                 what the buildpaths check looks for.
INSANE_SKIP:fiber-app += "buildpaths"
INSANE_SKIP:${PN}-dbg += "buildpaths"
INSANE_SKIP:proximos-control += "buildpaths"
