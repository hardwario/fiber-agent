# ProXimos Portal

Monitoring and control web UI for a ProXimos Hub: live probe/Node/Beacon
dashboard, alarm lifecycle, history and charts, signed device commands.

Rust + htmx + daisyUI, server-rendered. No bundler, no npm, no node — anywhere.
Design and rationale: [`../plan/portal/design.md`](../plan/portal/design.md);
stack decisions: [`../plan/portal/stack.md`](../plan/portal/stack.md).

This directory keeps the name `portal/`, but the binary it builds is becoming
`proximos-control`: decision #9 merges the device agent and this web surface into
one process and one Cargo workspace, with these sources landing in a
`portal-http` crate. Shape and roadmap:
[`../plan/portal/monolith.md`](../plan/portal/monolith.md).

## Dev loop

```bash
# broker + a simulated Hub (8 probes, 2 Nodes, Beacon tags, signed commands)
# and the portal itself
make up                                                      # http://localhost:8088

# stylesheet — only needed after touching templates/ (see "Styling")
make css                                                     # or: make css-watch
```

`make up` starts the containers, then builds the portal and starts it detached, so
closing the terminal does not leave a process holding the port; `make down` stops
both (portal by SIGTERM, then SIGKILL only if it refuses). The halves are
`portal-up` / `portal-down` (what you want when pointing the portal at a real Hub)
and `mock-up` / `mock-down`; `restart` restarts only the portal. `make logs`
follows the log, `make status` says whether it runs and who holds the port.
`make run` is plain `cargo run` in the foreground when you want it there.
`make help` lists the rest.

First visit lands on `/setup` to create the administrator account. Then pair with
the mock Hub on the **Device** page — no code needed: `dev/config.yaml` sets
`local_hostname` to the mock's hostname, so the portal treats it as the hub it
runs on, exactly as on real hardware. Comment that key out to exercise the remote
flow instead, and pair with the mock's `--pairing-code` (default `ABC123`, printed
at startup). Details and flags: [`dev/README.md`](dev/README.md),
[`dev/mock/README.md`](dev/mock/README.md).

If you have an existing local database from before the viewer→portal rename,
either move it (`mv <data_dir>/viewer.db <data_dir>/portal.db`, default
`data_dir` is `dev-data/`) or just re-run first-time setup against a fresh one.

If port 1883 is taken, put the broker somewhere else and point the portal at it:

```bash
printf 'services:\n  mosquitto:\n    ports: !override\n      - "1884:1883"\n' > /tmp/o.yml
docker compose -f dev/docker-compose.yml -f /tmp/o.yml --profile mock up -d
sed 's/mqtt_port: 1883/mqtt_port: 1884/' dev/config.yaml > /tmp/cfg.yaml
PORTAL_CONFIG=/tmp/cfg.yaml cargo run
```

## Layout

```
src/
  main.rs        startup order is load-bearing: log → config → landlock → runtime
  sandbox.rs     Landlock self-confinement (Linux only; dev hosts run unconfined)
  mqtt/          ingest: subscribe order, routing, reconnect
  state/         in-memory device state + change events (SSE fan-out source)
  persist.rs     the DB's only writer thread; hosts the alarm engine
  alarms/        alarm read/ack API + notifier task (email, webhook)
  notify/        transports and payloads (v1-parity schemas)
  relay.rs       signed-command relay: request → challenge → confirm
  keys/          device CA custody + per-user signing keys
  backfill.rs    Node history backfill: jobs, drain, reconciler, gap detection
  retention.rs   slow sweep of telemetry tiers
  routes/        thin axum handlers, one module per area
  render.rs      state → template context (all timestamp/casing normalisation)
  sse.rs         one stream per tab; renders partials as named SSE events
crates/
  fiber-protocol   the device contract: topics, payloads, signed envelope,
                   field registries, config-key validation
  portal-domain    pure alarm engine — no axum, no sqlite, no tokio
  portal-storage   SQLCipher repositories + migrations
templates/  static/  packaging/  dev/  migrations/
```

**Migrations are append-only, comments included.** refinery checksums the file
contents of every migration it has already applied, so editing a shipped
migration — even to fix a stale comment — makes an existing database refuse to
open. That is why some of them still say "viewer" after the rename; correct the
wording in a new migration if it ever matters, never in place.

## Styling

`static/css/app.css` is a **committed build artifact**. Tailwind scans
`templates/`, so a template that introduces a new utility needs the stylesheet
regenerated in the same commit — CI (`portal:css`) rebuilds and fails on any
difference. It exists as a committed file because the bitbake recipe installs it
as-is and the dev loop should not require a build step to look right.

Which classes to reach for: daisyUI components for generic chrome, raw Tailwind
only where we deliberately keep the familiar look (thermometer cards, chart
containers, alarm rows). That rule is in
[`../plan/portal/stack.md`](../plan/portal/stack.md) and it is not a preference —
mixing them freely is how the two idioms stop being distinguishable.

## Tests

```bash
make test                       # domain, storage, routing, handler + HTML snapshots
make lint                       # clippy --workspace --all-targets -D warnings
make check                      # fmt + lint + test + css-check, i.e. the pipeline
```

**Prerequisite: `git-lfs`.** `static/doc-images/` and the two documentation PDFs
under `static/` are Git LFS objects (root `.gitattributes`, since 2026-09-16), and so is
each brand's `branding/<brand>/favicon.ico`, which the build compiles in (and refuses to, with
a message saying so, while it is still a pointer file).
Install `git-lfs` and run `git lfs install` once per machine before cloning or
pulling; otherwise the working tree holds 130-byte pointer files where the
pictures should be, `/docs` renders broken images, and — because the bitbake
recipe copies `static/` as-is — so does a device built from that tree.
`file static/doc-images/*` should name an image format (`SVG Scalable Vector Graphics
image`, `PNG image data`), not plain `ASCII text`, which is a pointer file.

**Prerequisite: `protoc` *and* the well-known types.** `control-radio` depends
on the pinned `chirpstack_api` crate, whose build script generates code from
protos on every build, so without them every `cargo` command — and so every
`make` target that wraps one — dies in that build script before compiling any of
our own code. On Debian/Ubuntu both come from `apt-get install protobuf-compiler
libprotobuf-dev`; the compiler alone is not enough, because the crate vendors
only `google/api/*.proto` while ChirpStack's `common.proto` imports
`google/protobuf/timestamp.proto`, which protoc resolves out of `/usr/include`.
`make check-linux` installs both inside the container itself, so only a host run
needs this. CI installs the same two packages (`.gitlab-ci.yml`'s
`.native_deps`); the on-device counterpart is `DEPENDS += "protobuf-native"` in
the Yocto recipe.

**A green run on macOS says nothing about `src/sandbox.rs`.** Its ~300 lines of
Landlock code sit behind `#[cfg(target_os = "linux")]`, so a macOS `cargo
clippy` never compiles them — CI is the first place they are type-checked, and
they once reached it not compiling at all. The test count gives it away: it is
one higher in the container, and the extra test is the confinement itself.
Before pushing anything that touches confinement:

```bash
make check-linux                # fmt + lint + test in CI's own rust image
```

That target reads the image tag out of `.gitlab-ci.yml` rather than pinning its
own, so a CI bump cannot leave it checking against an image CI has left behind.
The Rust version itself comes from `rust-toolchain.toml`.

Two things `make` does differently from CI, both because of the imported agent:

- **`test`/`lint` skip `crates/control-core`** (`--workspace --exclude
  control-core`). It depends on `bluer`, i.e. BlueZ over D-Bus, which does not
  exist off Linux — so a plain `--workspace` on a macOS host dies in
  `libdbus-sys`' build script rather than telling you anything about the portal.
  `make check-linux` and CI run the whole workspace, where it builds.
- **`check` runs `cargo fmt --check`, never `cargo fmt`.** `control-core` is
  copy-and-edit from v1 (decision #3), so reformatting it would bury the
  import's provenance under a formatting diff. `make fmt` still rewrites, on
  purpose.

Two things the suite deliberately cannot cover:

- **Landlock** is process-wide and irrevocable, so its tests fork and confine a
  child (`src/sandbox.rs`). They are Linux-only and unverified on the *target* —
  per `../plan/security/README.md`'s evidence rule, only the real kernel counts.
  They do pass under the container above, which proves the mechanism works but
  not that it works on `linux-raspberrypi`.
- **Visual rendering.** Handler tests assert HTML content, not appearance. The
  stale-stylesheet bug that shipped through six milestones passed every test;
  the CI guard above is the fix, and looking at the page is still the check.

## Cross build for a Hub

```bash
make cross                      # the six Hub binaries, aarch64, in CI's toolchain
../system/scripts/dev-sync.py <hub> --what bin
```

`make cross` builds the binaries a Hub runs (`portal`, which the image installs as
`fiber-portal`, plus `fiber_app`, `fiberctl`, `fiber-factory-reset`,
`proximos-control` and `proximosctl`) into `target/aarch64-unknown-linux-gnu/release/`.
That is where `system/scripts/dev-sync.py` looks for them. Docker is the only thing it
needs on the host. It runs `ci/build-aarch64.sh`, the same script CI's
`portal:build-aarch64` runs, inside an image built from `ci/Dockerfile`. The toolchain is
therefore CI's: Debian bookworm's aarch64 cross gcc with bookworm's glibc 2.36, whose
binaries load on the image's newer glibc (2.43).

A host toolchain does not give you that. A cross gcc from a current distribution links
against that distribution's glibc, and on the Hub the binary then dies at start with
``version `GLIBC_2.xx' not found``. A wrong interpreter or architecture fails even
earlier, as systemd's `status=203/EXEC`. So the build ends with `ci/check-glibc.sh`,
which reads every binary's ELF headers and fails on any `GLIBC_` version past 2.36, on a
foreign interpreter, and on a shared library the image does not ship.
`../plan/system/design.md` §11 has the reasoning. The Makefile comment above `cross`
covers the details: the separate `target/cross/` cargo directory, running as your own
user, and using CI's registry image (`CROSS_IMAGE=`) instead of a local build.

`make cross PROXIMOS_BRAND=fiber` builds the same six binaries as FIBER (see "Brands"); a
`BRAND` file beside `SHA256SUMS` says which brand the directory holds.

A version CI has already built is in the package registry, so you do not need to build
it yourself. `main`, `dev` and version-tag pipelines publish the same six binaries,
checked the same way, as `proximos-portal-aarch64/<version>`. `<version>` is the
version string of the image from the same commit, which is `VERSION=` in a Hub's
`/etc/os-release`. See `../system/README.md`, "Pushing a change onto a running Hub".
CI publishes the default brand only. The FIBER set, when CI builds it, is meant to go to
`fiber-portal-aarch64/<version>` from the same commit (fiber-agent's input); that job does
not exist yet.

## Brands

The same workspace builds as **ProXimos** (the default) or as **FIBER**, the build
fiber-agent packages for Raspberry Pi OS. `PROXIMOS_BRAND` picks a directory under
`branding/` at build time; decision #50 and
[`../plan/portal/branding.md`](../plan/portal/branding.md) have the design.

```bash
PROXIMOS_BRAND=fiber CARGO_TARGET_DIR=target/fiber cargo run   # the portal as FIBER
PROXIMOS_BRAND=fiber CARGO_TARGET_DIR=target/fiber cargo test  # the suite as FIBER
make cross PROXIMOS_BRAND=fiber                                # FIBER Hub binaries
```

A separate `CARGO_TARGET_DIR` per brand is optional but saves a rebuild of everything that
depends on `proximos-brand` each time you switch.

- `branding/<brand>/brand.toml` holds the names (product, Hub/Node/Beacon, the mobile app),
  the hostname prefix, the certificate organisation, the support URL, the daisyUI theme and the
  documentation PDFs; `brand.env` repeats what the boot scripts need and is checked against it.
  `logo.svg`, `favicon.ico` (Git LFS) and `splash.pbm` (the 128×64 LCD boot splash, plain PBM
  text) sit beside it; `i18n/<lang>.json` may override whole catalogue strings.
- `crates/proximos-brand` compiles that into constants. Templates read the `brand` global, i18n
  and `docs/*.json` say `{brand}`, `{mobile_app}`, `{hostname_prefix}`, `{brand_id}` and
  `{support_email}` instead of the literals, and `/static/logo.svg` and `/static/favicon.ico` are
  served from the binary.
- Every brand's theme is in the one `static/css/app.css`, so `make css` and `css-check` do not
  depend on the brand.
- The FIBER raster assets are generated: `python3 branding/scripts/gen-fiber-assets.py`.

## On device

Packaging, the systemd unit, the nginx snippet and the obligations this places
on the renderer: [`packaging/README.md`](packaging/README.md). The binary binds
loopback only and confines itself at startup; `GET /health` with a session reports whether
that confinement actually took (`sandbox.state`), and so does the startup journal line
(`landlock: confinement enforced`). Without a session `/health` answers liveness only.
