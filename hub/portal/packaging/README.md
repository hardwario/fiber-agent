# Portal packaging

*Type: design · Status: draft for review · Date: 2026-08-19*

Everything the image needs to ship the portal. These files live with the code
rather than in the Yocto layer for the same reason `system/design.md` §6.1 makes
every service ship its own `tmpfiles.d` fragment: the component that knows what
it touches should be the component that declares it.

| File | Installed as | Owns |
|---|---|---|
| `fiber-portal.bb` | — (recipe) | `cargo_bin` build of the whole `portal/` workspace, split into three packages: `fiber-portal` (this table), `fiber-app` (the imported v1 agent) and `proximos-control` (the radio half) — see below for both |
| `fiber-portal.service` | `/usr/lib/systemd/system/` | unit: `User=proximos` (*2026-09-23*, was `fiber-portal` — [monolith.md §15](../../plan/portal/monolith.md), #88), loopback bind, hardening baseline |
| `fiber-portal.sysusers.conf` | `/usr/lib/sysusers.d/fiber-portal.conf` | the service account — `proximos`, the same line `proximos-radio.sysusers.conf` carries |
| `fiber-portal.tmpfiles.conf` | `/usr/lib/tmpfiles.d/fiber-portal.conf` | `/data/portal{,/exports,/calibrations,/tmp}`, owner `proximos` |
| `migrations/030-portal-owner.sh` | `/usr/lib/proximos/migrations/` | re-owns an existing `/data/portal` — database, `db.key`, exports, calibrations — from the retired `fiber-portal` account to `proximos`, run by `proximos-data-migrate.service` before the renderer and the portal start. Idempotent; also the documented roll-forward step after a rollback (`system/README.md`) |
| `fiber-portal-nginx.conf` | included from `nginx.conf`'s `http {}` | 443 termination, SSE proxying, 80→443 |

The same recipe also produces `fiber-app` — `fiber_app`/`fiberctl`/`fiber-factory-reset`,
imported byte-for-byte from v1's `application` (roadmap §11.1) and still under
its pre-rename identity (root user, no sysusers.d — v1's own posture, carried
forward deliberately; the non-root/unified-profile target is roadmap §11.6,
separate work):

| File | Installed as | Owns |
|---|---|---|
| `fiber.service` | `/usr/lib/systemd/system/` | unit: `User=root`, adapted from v1's `application/files/fiber.service` |
| `fiber-factory-reset.service` | `/usr/lib/systemd/system/` | oneshot, unsandboxed, runs before `fiber.service` — new, no v1 equivalent exists |
| `fiber.tmpfiles.conf` | `/usr/lib/tmpfiles.d/fiber.conf` | `/data/fiber`, `/data/fiber/config`, `/data/fiber/keys` |
| `fiber-config-seed.sh` | `/usr/share/fiber/scripts/` | run from `fiber.service`'s `ExecStartPre`: copies `control-core`'s `fiber.config.yaml`/`fiber.sensors.config.yaml` from `/usr/share/fiber/config` (installed read-only below, refreshed by every RAUC bundle) onto `/data/fiber/config` — only the files that aren't already there, so on-device edits and provisioned credentials survive updates. v1's equivalent lived in `fiber-firstboot.sh`; there is no v2 firstboot unit yet, so this runs inline like `init-signer.sh` beside it |

The same recipe also produces `proximos-control` — the radio half, `proximosctl`
(the CLI, including its `render` subcommand) and `proximos-control` (the resident
daemon of decision #12). One `proximos` account serves both, and it is the
account the whole merged component will run as once `monolith.md` §11.4-§11.6
lands — the portal already does, since `monolith.md` §15:

| File | Installed as | Owns |
|---|---|---|
| `radio-setup.service` | `/usr/lib/systemd/system/` | boot oneshot, `ExecStart=proximosctl control.radio create`: mints the tenant/read-only API keys, converges the ChirpStack objects, discovers the gateway EUI. Degraded-green (exit 0, `provisioned` marker withheld) on a Hub with no radio |
| `proximos-control.service` | `/usr/lib/systemd/system/` | the resident daemon: uplink decode → northbound publish, 60 s gateway health poll, `node-get` cache. `ConditionPathExists=/run/proximos/control/config.yaml` |
| `proximos-radio.sysusers.conf` | `/usr/lib/sysusers.d/proximos-radio.conf` | the `proximos` account |
| `proximos-radio.tmpfiles.conf` | `/usr/lib/tmpfiles.d/proximos-radio.conf` | `/run/proximos`, `/run/proximos/radio` |
| `migrations/010-radio-init.sh` | `/usr/lib/proximos/migrations/` | `/data/proximos/radio/{,lorawan,p2p}`, run by `proximos-data-migrate.service`. Numbered after chirpstack's `005-chirpstack-seed.sh` |
| `proximos-first-run.path` | `/usr/lib/systemd/system/` | *2026-09-26*, [decision #37](../../plan/decisions.md): `PathExists=/data/portal/requested-radio-mode`, the request the portal's `/setup` writes (`p2p` or `lorawan`). Enabled; `RequiresMountsFor=/data/portal` |
| `proximos-first-run.service` | `/usr/lib/systemd/system/` | root oneshot started by the path unit, `Before=proximos-render.service`: `proximosctl first-run-apply` writes `radio.mode` into `/data/proximos/config/proximos.config.yaml` only while that sets none, removes the request, and reboots on exit 3 (the rendered mode differs) |
| `migrations/040-config-init.sh` | `/usr/lib/proximos/migrations/` | `/data/proximos/config` (`root:root 0750`, the overlay's directory, owed by `system/design.md` §6.1); on a Hub that already has `/data/portal/portal.db` and no overlay, pins `radio.mode: lorawan` so the first-run window stays closed there |
| `templates/` | `/usr/share/proximos/templates/` | the renderer's entire input set: `defaults.yaml` plus the `portal/`, `control/`, `chirpstack/` and `chirpstack-mqtt-forwarder/` `.j2` trees. This path is `proximos-render`'s compiled-in `DEFAULT_TEMPLATES_DIR` |

Two things about that last row are worth stating explicitly, because both have
bitten:

- **`packaging/templates/` is not `portal/templates/`.** The first is the
  renderer's `.j2` set installed to `/usr/share/proximos/templates/`; the second
  is the portal's minijinja UI pages installed to
  `/usr/share/fiber-portal/templates/`. They are unrelated, tracked by different
  mechanisms (a `file://` `SRC_URI` entry vs. `PROXIMOS_SRCTREE_ASSETS`) and
  copied from different source directories in the same `do_install`.
- **`PACKAGES =+ "fiber-app proximos-control"` is load-bearing.** `FILES:${PN}`
  only *appends* to bitbake's default, which globs `${bindir}/*`; without the
  prepend both new binaries fall into the `fiber-portal` package and
  `proximos-control` ships units with nothing to run.

The layer's `recipes-core/proximos-render/` recipe now ships only
`proximos-render.service` and `RDEPENDS` on `proximos-control` for the binary and
the templates. The reverse edge does not exist and must not be added:
`proximos-control` does not `RDEPEND` on `proximos-render`, or the two would form
a cycle. `proximos-image.bb` installs both by name.

## How meta-fiber consumes this

`recipes-core/portal/fiber-portal.bb` in the layer is a two-line shim: set
`PORTAL_SRC` to the monorepo's `portal/` directory and `require` this recipe.
The layer keeps the `files/` symlinks (or `FILESEXTRAPATHS`) pointing here so
the unit and the fragments are never copied — a copy is a second source of
truth that drifts.

The nginx snippet is installed by
`recipes-connectivity/nginx/fiber-portal-nginx.bb` (F6) — v1's equivalent was
`fiber-nginx-conf`, pointed at its own colocated `fiber-nginx.conf`; this one
points `FILESEXTRAPATHS` back at `portal/packaging/` instead, so the file
stays owned by this component rather than being copied into the layer.

## What bitbake is told the build reads

The shim points `S` at this checkout's `portal/` directory, and there is no
fetcher on it — no git `SRC_URI`, no `SRCREV`. Only the packaging `file://`
entries above are checksummed, so nothing else under `portal/` reaches a task
hash unless the recipe says so. It did not, and the consequence was not
theoretical: an edited template left `do_install`'s stamp valid, the task never
re-ran, and `/usr/share/fiber-portal/` kept whatever the first build put there.
A committed UI change, a green pipeline, and a stale rootfs.

`fiber-portal.bb` now inherits `proximos-srctree` (in `meta-proximos/classes/`,
where its header explains why `[file-checksums]` rather than a hash in a
variable) and names the tree's inputs in two lists, split by which task each
one feeds:

| list | paths | invalidates |
|---|---|---|
| `PROXIMOS_SRCTREE_ASSETS` | `templates` `static` | `do_install` |
| `PROXIMOS_SRCTREE_CODE` | `src` `crates` `i18n` `migrations` `Cargo.toml` `Cargo.lock` `rust-toolchain.toml` | `do_compile` **and** `do_install` |

So an `.html`/`.css`/`.js` edit repackages in minutes and reuses the cargo
build; a `.rs` edit pays for the workspace compile it actually needs.

`i18n/` and `migrations/` sit on the **code** side despite being `.json` and
`.sql`: `src/i18n.rs` pulls every catalogue in with `include_str!` and
`portal-storage` embeds the schema with `refinery::embed_migrations!`. Neither
ships as a file, and editing either has to invalidate the binary.

Two obligations this places on anyone working in `portal/`:

1. **A new top-level directory that ships or compiles in must be added to one
   of the two lists.** The class fatals at parse on a path that does not exist
   and CI's `lint:recipes` block (d) checks the same, but neither can know
   about a directory nobody named — and the failure mode is silence, not an
   error.
2. **`static/css/app.css` is committed build output.** A template that
   introduces a Tailwind utility needs `scripts/build-css.sh` re-run and the
   result committed. `do_install` warns when the stylesheet is older than its
   inputs, CI's `portal:css` job fails on any difference, and
   `system/scripts/rebuild-portal.sh` runs the rebuild first so the local loop
   cannot go stale.

`system/scripts/rebuild-portal.sh` is the loop: CSS, then
`bitbake fiber-portal` plus the image, then a `diff -r` of the packaged and
rootfs copies of `templates/` and `static/` against this tree. It exits
non-zero when they differ, which is the regression test for everything above.

## Layout on target

```
/usr/bin/fiber-portal                     the binary (self-confining, §sandbox)
/usr/share/fiber-portal/templates/        minijinja templates, read at runtime
/usr/share/fiber-portal/static/           htmx, Alpine, ECharts, app.css
/run/proximos/portal/config.yaml          rendered by `proximosctl render`, 0640 root:proximos
/data/portal/                             SQLCipher db, exports, calibrations; owner proximos
/usr/lib/proximos/migrations/030-portal-owner.sh

/usr/bin/proximosctl                      the CLI, incl. `render` and `control.radio`
/usr/bin/proximos-control                 the resident daemon (decision #12)
/usr/share/proximos/templates/            the renderer's .j2 set + defaults.yaml
/usr/lib/proximos/migrations/010-radio-init.sh
/run/proximos/control/config.yaml         rendered, 0640 root:proximos
/run/proximos/radio/                      proximos-control's own RuntimeDirectory
/data/proximos/radio/{lorawan,p2p}/       tokens, nodes.json, node-get cache
```

`/run/proximos/control/` is the **renderer's** RuntimeDirectory, `/run/proximos/radio/`
is `proximos-control.service`'s own. Neither unit may claim the other's: systemd removes
a `RuntimeDirectory=` when its owning unit stops, so a renderer that claimed
`proximos/radio` would delete the daemon's heartbeat file on every re-render, and a
daemon that claimed `proximos/control` would delete the config its own
`ConditionPathExists=` tests. Both unit files carry the warning.

Nothing mutable on the rootfs — v1 lost
`/opt/fiber-viewer/backend/data/calibrations` on every RAUC update.

## Obligations this places on other components

1. **`fiberctl render` must emit `/run/proximos/portal/config.yaml`** with the
   keys in `portal/dev/config.yaml` (bind_addr, mqtt_*, topic_prefix, data_dir,
   templates_dir, static_dir, db_key, cookie_secure). Owner:
   `application/design.md`.
2. **The renderer decides whether the portal runs at all.** The unit's
   `ConditionPathExists=/run/proximos/portal/config.yaml` replaces v1's
   `.client_mode` marker: on a client-role Hub the renderer emits no portal
   config and the unit stays inactive, which is how decision #5 (client Hubs
   are headless) gets enforced without the portal reading device role.
3. **`db_key` must reach the rendered config.** Without it the portal opens a
   plain-SQLite database and logs a warning on every start — acceptable in dev,
   not on a device holding patient-adjacent measurements and signing keys.
4. **`/data/tls` is `0711 root`** (`data-dirs.conf`): nginx's master process
   reads `server.key` as root, and the portal — user `proximos` — opens
   `ca.crt` (0644) by name for `GET /connection/ca` without being able to list
   the directory. `ca.key` stays 0600 root; `server.key` is 0640
   root:mosquitto, because the Server's 8883 listener (cluster.md §5) runs as
   `mosquitto` and the group has no members. The portal's Landlock table
   declares `/data/tls` read-only and tolerates its absence.
5. **Kernel LSM list must name `landlock`.** Adding AppArmor to `CONFIG_LSM`
   without it silently disables self-confinement (spike S3). The image smoke
   test should assert the startup journal line,
   `journalctl -b -u fiber-portal | grep -q 'landlock: confinement enforced'`.
   An anonymous `/health` carries no `sandbox` (GitLab #104), so a `curl` of it
   proves only liveness.

## Proposed replacement for `system/design.md` §3.5 (currently STALE)

> ### 3.5 Reproducible portal build
>
> The portal is a Rust binary plus runtime assets, built in-bitbake with
> `cargo_bin` against a committed `Cargo.lock` — the same path the agent
> already uses, and reproducible for the same reason. v1's networked 5-phase
> pip/npm `do_compile` is gone entirely; so is npm, node and any bundler
> (decision #1).
>
> - `do_compile[network] = "0"`: crates are vendored/fetched in the fetch task.
> - `templates/` and `static/` install as plain files under
>   `/usr/share/fiber-portal/`. Templates are read at runtime (minijinja), so a
>   UI fix is a file change, not a rebuild of the binary.
> - `static/css/app.css` is a committed Tailwind/daisyUI build artifact. CI
>   (`portal:css`) rebuilds it with the pinned standalone Tailwind binary and
>   fails on any difference, which is what keeps committed build output honest.
> - No `portal-artifact` CI stage and no tarball: there is nothing left to
>   pre-build outside bitbake.

## Proposed replacement for `system/design.md` §8.1's portal line (currently STALE)

> **Superseded 2026-08-21** by the monolith (`plan/portal/monolith.md` §9): there is
> no separate portal unit to name. One `proximos-control.service` runs the agent and
> serves the portal, the `portal:` section of the one rendered config decides whether
> a listener is bound at all, and the bind moved off `8080` (it collided with v1's
> non-configurable ChirpStack gRPC address) to `127.0.0.1:8088`. Kept below as the
> shape that was proposed while the two were separate processes.

> ```
> fiber-portal.service (Rust binary, 127.0.0.1:8080, User=fiber-portal,
>                       self-confined with Landlock; inactive unless the
>                       renderer emitted its config — i.e. server role only)
> nginx (443/80 → 8080, SSE proxying unbuffered on /events)
> ```
>
> Boot order gains one edge: `Requires=proximos-render.service`, because the
> unit's config *and* its start condition both come from the renderer.

## Not addressed here

- **AppArmor profile** for `fiber-portal` — belongs to `security/apparmor.md`
  (§4 profile resources, `PENDING S3/S4/S5`). This document's resource set is
  the one that profile must agree with (`security/README.md` acceptance
  criteria).
- **RAUC health gate** — whether `sandbox.state != "enforced"` should fail the
  gate is left open in `security/landlock.md` §8 and settled in
  `application/design.md`. The portal only reports; it does not decide.

## Cross-component fallout from the viewer → portal rename (2026-08-19)

**Closed 2026-08-21 by the monolith (`plan/portal/monolith.md` §12.1, decisions
#9-#12).** All five items are discharged or superseded, and the resolutions are
recorded here rather than deleted so the trail survives:

1. **Security docs** — superseded. Two AppArmor profiles become one
   `proximos-control` profile, and the two Landlock tables become one union
   table (`monolith.md` §8). There is no `fiber-viewer`/`fiber-portal` name left
   to align; CP-8 now reviews one table instead of two.
2. **`plan/system/design.md`** — superseded. One service line, one `/data`
   subtree (`/data/proximos/portal/` under the agent's `/data/proximos/`), and
   the §3.5/§8.1 replacement blocks below are folded into that change.
3. **The renderer's config path** — superseded, and the mechanism it was buying
   moved. There is one rendered config, `/run/proximos/control/config.yaml`,
   carrying a `portal:` section (`monolith.md` §9.1). The property survives one
   level down: the renderer emits **no `portal:` section** on a client-role Hub,
   so no router is built and no listener bound, and the process never reads
   `role`. `ConditionPathExists=` is gone, because the unit can no longer be
   conditional — it is the device agent (decision #11).
4. **The MQTT `client_id` question** — disposed of. One process means one
   mosquitto account, so there are no two client ids for an ACL to disagree
   about (`monolith.md` §5). `control-radio`'s own account folds into it too.
5. **`plan/viewer/` and `decisions.md` #1** — done. The directory is
   `plan/portal/`, and #1's stack survives with only its "own workspace, own
   unit, own `fiber-viewer` user" clause superseded by #9.

Still ours, from the same section (`monolith.md` §12.1 item 3): packaging
collapses to **one** `.bb`, one `.service`, one `sysusers` and one `tmpfiles`
identity. Not done yet — it waits on the branch order in `monolith.md` §11.0.

## Pending packaging, as of 2026-08-26 — `control-ble`

`control-ble` landed (`plan/control/ble/*.md`, MR !15's design set) and shipped three
fragments here. None of them is referenced by a recipe yet, exactly as `control-radio`'s
are not: they join the "packaging collapses to one `.bb`" item above.

| File | What it is |
|---|---|
| `ble-setup.service` | the boot oneshot, `proximosctl control.ble create` |
| `proximos-ble.tmpfiles.conf` | `/run/proximos/ble` only — the `/data` tree is the migration's |
| `migrations/020-ble-init.sh` | `/data/proximos/ble/{central,central/archive,peripheral}`, directories only |

`proximosctl` itself is still unpackaged, and now carries **two** dispatch groups rather
than one, so the recipe that eventually installs it covers both.

### Two things this component needs that the radio half does not

Both belong in `security/landlock.md` §5.1 and `security/apparmor.md` (Strnad's files —
**proposed here, not edited there**):

| Resource | Why |
|---|---|
| `/data/proximos/ble/` **rw** | the provisioning ledger and the archive segments |
| `/data/proximos/config/` **rw** | the `eye:` fleet section is read *and written* through the host's config store |
| `/dev/rfkill` **rw** | rung 0 of the scan-stall recovery ladder writes the kernel ABI directly (central.md §7). This is what replaced v1's `sh -c "rfkill block bluetooth; …"`, so denying it puts the shell back |
| `AF_BLUETOOTH` in `RestrictAddressFamilies` | the EN12830 archive path opens a raw L2CAP socket on the one fixed CID the kernel permits userspace (beacon-btsmp1.md §3) |

The D-Bus surface for `apparmor.md` is exactly `org.bluez` — `Adapter1`, `Device1`,
`GattManager1`, `LEAdvertisingManager1` — and nothing else. The BLE terminal's `script`
and the recovery ladder's `sh -c`/`systemctl` are gone (CP-1, CP-2), so there is no exec
surface in this component at all.

### One defect found while writing `ble-setup.service`

`radio-setup.service` puts `StartLimitBurst` and `StartLimitIntervalSec` under
`[Service]`. `systemd-analyze verify` reports
`Unknown key name 'StartLimitIntervalSec' in section 'Service', ignoring` — they are
`[Unit]` directives, so the interval silently falls back to systemd's 10 s default and the
policy is "6 starts in 10 seconds", not the "6 in 5 minutes" the surrounding comment
describes. `ble-setup.service` puts them in `[Unit]`; `radio-setup.service` is Hynek's
file and is left alone, raised rather than changed.
