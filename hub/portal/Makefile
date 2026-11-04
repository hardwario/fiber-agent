# Dev loop shortcuts. `make up` / `make down` start and stop the portal
# detached, so closing the terminal does not leave a process holding the port
# (which is otherwise the usual reason `make up` fails on the next run).
#
# The PID file holds the server binary's own PID, not cargo's: `cargo run`
# forks the binary as a child, and killing cargo orphans it still listening.

BIN      := target/debug/portal
PIDFILE  := dev-data/portal.pid
LOGFILE  := dev-data/portal.log
PORT     := $(shell sed -n 's/^bind_addr:.*:\([0-9]*\)".*/\1/p' dev/config.yaml)

# crates/control-core and crates/control-ble both depend on `bluer` (BlueZ over
# D-Bus), which does not exist off Linux — so neither the imported agent nor the
# BLE component can build on a macOS host at all, and a plain `--workspace`
# there dies in libdbus-sys' build script. proximosctl is on the list for the
# same reason at one remove: it depends on control-ble. proximos-control builds
# here but its daemon test cannot pass: it sends the binary a SIGTERM and
# expects a clean exit, and on a macOS host the process dies of the signal
# (`ExitStatus(unix_wait_status(15))`) — a platform gap, not a regression, and
# it fails on an unmodified tree. Local targets skip the four; `check-linux`
# and CI run the whole workspace.
WS       := --workspace --exclude control-core --exclude control-ble --exclude proximosctl \
            --exclude proximos-control

# Read from the pipeline rather than pinned here, so a CI image bump cannot
# leave `make check-linux` verifying against an image CI no longer uses.
RUST_IMAGE := $(shell sed -n 's/^ *image: \(rust:[^ ]*\).*/\1/p' ../.gitlab-ci.yml | head -1)

.PHONY: up down portal-up portal-down restart status logs run dev build test lint fmt fmt-check css css-watch \
        css-check check check-linux cross test-ap mock-up mock-down multi-up multi-down joiner-up joiner-down demo demo-seed alarm alarm-clear node beacon hub help

help:
	@echo "up / down            start / stop the whole dev loop: containers + portal"
	@echo "portal-up / -down    only the portal, containers untouched"
	@echo "restart              restart the portal, containers untouched"
	@echo "status               is it running, on which PID and port"
	@echo "logs                 follow $(LOGFILE)"
	@echo "run                  foreground instead (Ctrl-C stops it)"
	@echo "dev                  foreground, rebuilds on Rust edits (needs cargo-watch);"
	@echo "                     templates + i18n reload on their own in debug builds"
	@echo "mock-up / mock-down  broker + the simulated server Hub"
	@echo "multi-up / -down     broker + the whole demo cluster (3 Hubs)"
	@echo "joiner-up / -down    the C5 Client Hub, after Add hub registered it"
	@echo "demo-seed            certificates, Beacon archives, alarms, offline (after /setup)"
	@echo "alarm / alarm-clear  force a mock alarm transition (LINE=1 SEV=critical VALUE=15.7)"
	@echo "node                 drive one mock Node's liveness/history (ARGS=\"silence\");"
	@echo "                     'dev/node.sh' with no args prints the usage"
	@echo "beacon               drive one mock Beacon's liveness/signal (ARGS=\"silence\");"
	@echo "                     'dev/beacon.sh' with no args prints the usage"
	@echo "hub                  drive one mock Hub (ARGS=\"offline truck\");"
	@echo "                     'dev/hub.sh list' names the three"
	@echo
	@echo "test lint fmt build   the usual, minus crates/control-core, which"
	@echo "                      needs BlueZ and so builds on Linux only"
	@echo "css / css-watch       rebuild the stylesheet / rebuild on template edits"
	@echo "css-check             fail if the committed stylesheet is stale (as CI does)"
	@echo "test-ap               the suite with serde_json arbitrary_precision, the"
	@echo "                      feature CI's --workspace run turns on and a host run does not"
	@echo "check                 fmt + lint + test + test-ap + css-check — the pipeline, locally"
	@echo "check-linux           the same inside $(RUST_IMAGE): the only way to compile"
	@echo "                      the Landlock code, which macOS silently skips"
	@echo "cross                 the six Hub binaries for aarch64, in CI's cross-build image,"
	@echo "                      checked against the Hub's glibc; for system/scripts/dev-sync.py"
	@echo "                      (PROXIMOS_BRAND=fiber for the FIBER build)"

build:
	cargo build

# Broker first, so the portal's first MQTT connect succeeds instead of
# retrying. Point the portal at a real Hub with `make portal-up` instead.
up: mock-up portal-up

# Starts detached and waits for the port to answer, so a failed start is
# reported here instead of looking like success with an empty log.
# One shell for the whole thing: `exit 0` on the already-running branch has to
# skip the port check and the start, and a recipe line cannot skip the next one.
portal-up: build
	@mkdir -p dev-data
	@if [ -f $(PIDFILE) ] && kill -0 $$(cat $(PIDFILE)) 2>/dev/null; then \
		echo "already running (PID $$(cat $(PIDFILE)))"; exit 0; fi; \
	if lsof -nP -iTCP:$(PORT) -sTCP:LISTEN >/dev/null 2>&1; then \
		echo "port $(PORT) is already held by something else:"; \
		lsof -nP -iTCP:$(PORT) -sTCP:LISTEN; exit 1; fi; \
	RUST_LOG=$${RUST_LOG:-info} nohup $(BIN) >> $(LOGFILE) 2>&1 & echo $$! > $(PIDFILE); \
	for i in 1 2 3 4 5 6 7 8 9 10; do \
		if curl -fsS -o /dev/null http://127.0.0.1:$(PORT)/health 2>/dev/null; then \
			echo "up on http://127.0.0.1:$(PORT) (PID $$(cat $(PIDFILE)))"; exit 0; fi; \
		if ! kill -0 $$(cat $(PIDFILE)) 2>/dev/null; then \
			echo "it died on startup — last lines of $(LOGFILE):"; \
			tail -15 $(LOGFILE); rm -f $(PIDFILE); exit 1; fi; \
		sleep 0.3; \
	done; \
	echo "started (PID $$(cat $(PIDFILE))) but /health did not answer — see $(LOGFILE)"

# `down` is the one command that leaves nothing of the dev loop running, so it
# tears down the containers too. `portal-down` is the half `restart` needs.
down: portal-down mock-down

# SIGTERM, not SIGKILL: the graceful path closes SSE streams and flushes, and
# `serving_outlives_the_shutdown_grace` is the test that keeps it quick.
portal-down:
	@if [ -f $(PIDFILE) ]; then \
		PID=$$(cat $(PIDFILE)); \
		if kill -0 $$PID 2>/dev/null; then kill $$PID; \
			for i in 1 2 3 4 5 6 7 8 9 10; do \
				kill -0 $$PID 2>/dev/null || break; sleep 0.3; done; \
			kill -0 $$PID 2>/dev/null && { echo "did not stop, sending SIGKILL"; kill -9 $$PID; }; \
			echo "stopped (PID $$PID)"; \
		else echo "not running (stale PID file)"; fi; \
		rm -f $(PIDFILE); \
	else \
		PID=$$(lsof -t -iTCP:$(PORT) -sTCP:LISTEN 2>/dev/null); \
		if [ -n "$$PID" ]; then kill $$PID && echo "stopped PID $$PID (found by port, no PID file)"; \
		else echo "not running"; fi; \
	fi

restart: portal-down portal-up

status:
	@if [ -f $(PIDFILE) ] && kill -0 $$(cat $(PIDFILE)) 2>/dev/null; then \
		echo "running: PID $$(cat $(PIDFILE)) on port $(PORT)"; \
	else echo "not running"; fi
	@lsof -nP -iTCP:$(PORT) -sTCP:LISTEN 2>/dev/null || true

logs:
	@touch $(LOGFILE) && tail -f $(LOGFILE)

run:
	cargo run

# Templates and i18n JSON are re-read per request in debug builds
# (render::Templates, i18n::catalogue), so only Rust needs the watcher.
dev:
	@command -v cargo-watch >/dev/null || { echo "cargo install cargo-watch"; exit 1; }
	cargo watch -w src -w Cargo.toml -x run

mock-up:
	docker compose -f dev/docker-compose.yml --profile mock up -d

# Both profiles on the way down, so `make down` clears the cluster whichever
# half started it — a container left behind keeps publishing as a Hub the
# portal then reports as online.
mock-down:
	@command -v docker >/dev/null || { echo "docker not installed, no containers to stop"; exit 0; }
	@docker compose -f dev/docker-compose.yml --profile mock --profile multi --profile joiner down

# The demo cluster: the server Hub plus two client Hubs on one broker. The
# portal needs no extra config — it subscribes `fiber/+/...` and keys one
# device per hostname. dev/mock/README.md has what each profile covers.
multi-up:
	docker compose -f dev/docker-compose.yml --profile multi up -d

multi-down: mock-down

# C5's Client Hub (dev/mock/README.md, "The joiner"): start it after Add hub
# has registered proximos-c5c1e000, and its first status completes the row.
joiner-up:
	docker compose -f dev/docker-compose.yml --profile joiner up -d

joiner-down:
	docker compose -f dev/docker-compose.yml --profile joiner stop mock-joiner

demo: multi-up portal-up

# What the mock cannot publish, on top of a running `make demo` with the
# admin created through /setup: calibration certificates in every state and
# Beacon archives (dev/seed_demo.py), probe alarms open / escalated / cleared
# (dev/alarm.sh), one silent Node and the truck Hub offline. Re-runnable;
# dev/mock/README.md "Seeding the demo" has the list and how to undo it.
demo-seed:
	python3 dev/seed_demo.py
	dev/alarm.sh raise 5 critical 16.2
	dev/alarm.sh raise 6 warning 9.1
	dev/alarm.sh raise 7 critical 15.0
	sleep 1
	dev/alarm.sh clear 7 4.0
	dev/node.sh silence 70b3d50000000000
	dev/hub.sh offline truck

# Force an alarm transition on the mock Hub without waiting for its random
# walk to cross a threshold — dev/alarm.sh's header documents the payload,
# the defaults, and the one-active-alarm-per-line dedup that makes a re-raise
# of an already-active line a silent no-op.
LINE ?= 1
SEV  ?= critical

alarm:
	@dev/alarm.sh raise $(LINE) $(SEV) $(VALUE)

alarm-clear:
	@dev/alarm.sh clear $(LINE) $(VALUE)

# Silence/resume one mock Node and pick how it answers get_sticker_history —
# the seam devices.backend.md §6 (B3) needs to be walked in the dev loop.
# dev/node.sh's header carries the usage and the walk-through.
ARGS ?=
node:
	@dev/node.sh $(ARGS)

# Silence/resume one mock Beacon and move it between signal bands — the
# seam devices.backend.md §8 (B5) needs to be walked in the dev loop.
# dev/beacon.sh's header carries the usage and the walk-through, including
# how to watch the store-interval gate keep one eye_readings row per
# interval against six advertisements a minute.
beacon:
	@dev/beacon.sh $(ARGS)

# Take one Hub of the demo cluster off the air, park it in standby, or age
# its gateway row — dev/hub.sh's header carries the cluster walk-through.
hub:
	@dev/hub.sh $(ARGS)

test:
	cargo test $(WS)

lint:
	cargo clippy $(WS) --all-targets -- -D warnings

fmt:
	cargo fmt

# `check` must not reformat: crates/control-core is copy-and-edit from v1
# (decision #3, monolith.md §11.8), so rewriting it would bury the import's
# provenance under a formatting diff. `make fmt` still rewrites, on purpose.
fmt-check:
	cargo fmt --check

css:
	./scripts/build-css.sh

css-watch:
	./scripts/build-css.sh watch

# static/css/app.css is committed (the bitbake recipe installs it as a plain
# file), and committed build output drifts: M0's stylesheet shipped for six
# milestones missing every alert-*/badge-* utility later templates introduced,
# with the whole suite green. Same check the portal:css job runs.
css-check:
	@cp static/css/app.css dev-data/app.css.committed 2>/dev/null || \
		{ mkdir -p dev-data && cp static/css/app.css dev-data/app.css.committed; }
	@./scripts/build-css.sh >/dev/null
	@if cmp -s dev-data/app.css.committed static/css/app.css; then \
		echo "css-check: static/css/app.css is up to date"; \
	else \
		echo "css-check: static/css/app.css is STALE — commit the rebuilt file"; exit 1; fi

# The feature CI's `--workspace` run turns on and a host run does not.
#
# control-core enables serde_json's `arbitrary_precision`, Cargo unifies
# features per build, and under it a JSON number SERIALIZES as a one-key map
# — so raw JSON reaching a template renders as `{"$serde_json::private::
# Number": "…"}`, and any arithmetic on it fails the page outright. That is
# how /docs shipped green from a host run and 500'd in CI (pipeline #18092).
# control-core does not build on macOS (bluer -> libdbus-sys), so the feature
# is asked for by name here instead: same code path, no container.
test-ap:
	cargo test --bin portal --features serde_json/arbitrary_precision

check: fmt-check lint test test-ap css-check

# macOS never compiles anything behind #[cfg(target_os = "linux")], so the
# Landlock tests in src/sandbox.rs are invisible in a local run: 548 tests pass
# here, 549 in the container, and the missing one is the confinement itself.
# This ran green on 1.87 and was reported as "clippy clean" once, while the
# Linux build did not compile at all (899af66).
#
# The apt line is the same one portal:lint/portal:test run (.gitlab-ci.yml's
# .native_deps): chirpstack_api generates its stubs in a build script, and the
# rust image ships neither protoc nor the well-known .proto files it imports.
#
# 2026-08-26: it now really is the same list. It previously installed only the two
# protobuf packages, so this target could not have run since control-core was imported —
# control-core depends on `bluer`, whose libdbus-sys build script panics with
# "The system library `dbus-1` required by crate `libdbus-sys` was not found", and this
# target is the one that runs `--workspace`. A check that cannot run is worse than no
# check, because the comment below still claims it covers control-core.
# Two settings this target needs that a host run does not, both established by
# bisecting the container's failures against the unmodified tree:
#
#   --test-threads=1        control-core's README calls it mandatory (several imported
#                           tests share global state) and CI passes it, but this target
#                           did not. Three libs::storage::db tests failed here and
#                           nowhere else purely because of that.
#   FIBER_TEST_ALLOW_ROOT_SKIPS=1
#                           the container runs as root, where a file mode cannot deny
#                           access. Two libs::factory_reset tests detect that and refuse
#                           to pass vacuously — the variable is the opt-out they name,
#                           and it declares the skip rather than hiding it.
check-linux:
	docker run --rm -v "$$PWD/..":/w -v "$$PWD/target-linux":/t -w /w/portal \
		-e CARGO_TARGET_DIR=/t $(RUST_IMAGE) bash -c '\
		apt-get update -qq && \
		apt-get install -y --no-install-recommends \
			pkg-config libssl-dev libdbus-1-dev \
			protobuf-compiler libprotobuf-dev >/dev/null && \
		rustup component add rustfmt clippy >/dev/null 2>&1; \
		cargo fmt --check && \
		cargo clippy --workspace --all-targets -- -D warnings && \
		FIBER_TEST_ALLOW_ROOT_SKIPS=1 cargo test --workspace -- --test-threads=1'
	@echo "check-linux: the whole workspace, control-core included"

# The aarch64 cross build for a Hub: tier 1 of the developer loop
# (../plan/system/design.md §11, issue #18). It runs ci/build-aarch64.sh, the
# body of CI's portal:build-aarch64, in an image built from the same
# ci/Dockerfile, so the toolchain and its glibc (bookworm's 2.36) are CI's, and
# ci/check-glibc.sh fails the build if a binary needs anything the Hub image
# lacks. The six binaries land in target/aarch64-unknown-linux-gnu/release/,
# which is where ../system/scripts/dev-sync.py looks (`--what bin`).
#
# Cargo works in target/cross/ inside the container, not in target/: the build
# scripts and proc macros it compiles there link against the container's
# glibc, and in a shared target/release/ the host would pick them up (or the
# container the host's, linked against a glibc it does not have). CARGO_HOME is
# in target/cross/ as well, so crates download once, and the container runs as
# you (CROSS_USER), so nothing under target/ ends up owned by root.
#
# The image is built locally, tagged after PORTAL_DOCKER_TAG in .gitlab-ci.yml;
# the first build takes minutes, every later one is cached. For CI's own copy
# of the image instead, log in to the registry first:
#   docker login registry.hardwario.com
#   make cross CROSS_IMAGE=registry.hardwario.com/proximos/proximos-v2/portal-ci:$(PORTAL_CI_TAG)
DOCKER            ?= docker
# The brand the binaries carry (plan/portal/branding.md): `make cross PROXIMOS_BRAND=fiber`
# builds the FIBER set fiber-agent packages. Same output directory: a cross build of one brand
# replaces the other's binaries there, and the BRAND file beside them says which they are.
PROXIMOS_BRAND    ?= proximos
PORTAL_CI_TAG     := $(shell sed -n 's/^ *PORTAL_DOCKER_TAG: *"\(.*\)".*/\1/p' ../.gitlab-ci.yml | head -1)
CROSS_LOCAL_IMAGE := proximos-portal-ci:$(PORTAL_CI_TAG)
CROSS_IMAGE       ?= $(CROSS_LOCAL_IMAGE)
CROSS_USER        ?= $(shell id -u):$(shell id -g)

cross:
ifeq ($(CROSS_IMAGE),$(CROSS_LOCAL_IMAGE))
	$(DOCKER) build -t $(CROSS_IMAGE) ci
endif
	@mkdir -p target/cross/cargo-home target/aarch64-unknown-linux-gnu/release
	$(DOCKER) run --rm --user "$(CROSS_USER)" -v "$$PWD/..":/w -w /w/portal \
		-e HOME=/tmp \
		-e CARGO_HOME=/w/portal/target/cross/cargo-home \
		-e CARGO_TARGET_DIR=/w/portal/target/cross \
		-e CROSS_DEST=/w/portal/target/aarch64-unknown-linux-gnu/release \
		-e PROXIMOS_BRAND=$(PROXIMOS_BRAND) \
		$(CROSS_IMAGE) ci/build-aarch64.sh
	@echo "cross: next, system/scripts/dev-sync.py <hub> --what bin"
