###############################################################################
# CI build environment for the portal/control Rust workspace. Built and
# pushed by the docker:build-portal CI job as
# $CI_REGISTRY_IMAGE/portal-ci:{$PORTAL_DOCKER_TAG,<short-sha>} and consumed by
# portal:lint, portal:test and portal:build-aarch64.
#
# Everything here is either a native dependency those jobs' build scripts
# need (see the comment this replaces at .gitlab-ci.yml's old .native_deps
# anchor for exactly why each package is required), the arm64 cross-toolchain
# portal:build-aarch64 needs, or sccache. Baking all three in here means none
# of the three jobs runs apt-get, which was previously the least reliable
# step in the pipeline (a flaky Debian mirror, retried per job).
###############################################################################

# The exact patch release, not `rust:1.94-bookworm`: that tag floats (it is
# 1.94.1 today), and then the image's toolchain no longer matches the
# `channel = "1.94.0"` pin in portal/rust-toolchain.toml. rustup resolves the
# pin file, installs a bare 1.94.0 at job start, and the aarch64 target and the
# clippy/rustfmt components added below land on a toolchain nobody uses, so
# portal:build-aarch64 fails with "can't find crate for `core`". Bump this
# together with rust-toolchain.toml and PORTAL_DOCKER_TAG in .gitlab-ci.yml.
FROM rust:1.94.0-bookworm

ENV DEBIAN_FRONTEND=noninteractive

# Native deps (amd64): every one is a hard requirement of the workspace's
# default feature set, not optional tooling.
#   libdbus-1-dev      -> bluer (unconditional dep of control-core)
#   libssl-dev         -> rusqlite "bundled-sqlcipher" links system OpenSSL
#   protobuf-compiler  -> control-core's build.rs calls prost_build, and
#                         chirpstack_api's runs tonic-prost-build for all six
#                         proto groups; prost-build 0.14 bundles no protoc
#   libprotobuf-dev    -> the well-known types (chirpstack_api imports
#                         google/protobuf/timestamp.proto, resolved from
#                         /usr/include)
#
# arm64 cross deps (portal:build-aarch64): the foreign architecture has to be
# registered before the second `apt-get update`, or the :arm64 packages
# aren't in any index yet. protobuf-compiler/libprotobuf-dev stay host-arch
# (build-time tools run by prost_build, not link targets).
# curl/ca-certificates: rust:bookworm's base debian:bookworm-slim ships
# neither, and the sccache install step below needs curl.
# git-lfs: portal/static/doc-images/ and the two documentation PDFs are LFS
# objects; with the binary present the runner fetches them at checkout
# instead of leaving 130-byte pointer files behind for the tests to find.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev libdbus-1-dev protobuf-compiler libprotobuf-dev \
        curl ca-certificates git-lfs \
    && dpkg --add-architecture arm64 \
    && apt-get update && apt-get install -y --no-install-recommends \
        gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
        libssl-dev:arm64 libdbus-1-dev:arm64 \
    && rm -rf /var/lib/apt/lists/*

# The image's default toolchain is 1.94.0 (matches portal/rust-toolchain.toml
# exactly, see FROM above), so these land on the same toolchain
# `cd portal && cargo ...` resolves via the pin file — no separate install.
RUN rustup component add clippy rustfmt \
    && rustup target add aarch64-unknown-linux-gnu

# sccache. Pinned like kas is in system/docker/Dockerfile: bump this line
# deliberately (and rebuild via docker:build-portal), never float it.
# v0.18.0 = latest release on GitHub, verified 2026-09-14.
ARG SCCACHE_VERSION=0.18.0
RUN curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
      | tar xz -C /usr/local/bin --strip-components=1 \
        "sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache"

CMD ["/bin/bash"]
