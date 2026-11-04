# The public demo portal image (packaging/demo/README.md). Built by the
# portal:demo-image CI job on a `demo-*` tag with context `portal/`, never on
# a Hub: real hardware gets the bitbake recipe in packaging/fiber-portal.bb.
#
# Same native deps as ci/Dockerfile, minus libdbus (control-core is not a
# dependency of the portal binary) and minus the arm64 cross toolchain.
FROM rust:1.94-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev protobuf-compiler libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --bin portal

# libssl3: rusqlite's bundled-sqlcipher links the system OpenSSL. Paths match
# the Yocto layout so packaging/templates/portal and this config differ only
# in hostnames.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends libssl3 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 1000 --home /data/portal portal \
    && mkdir -p /data/portal && chown portal:portal /data/portal
COPY --from=build /src/target/release/portal /usr/bin/portal
COPY templates /usr/share/fiber-portal/templates
COPY static /usr/share/fiber-portal/static
USER portal
ENV PORTAL_CONFIG=/etc/fiber-portal/config.yaml
EXPOSE 8088
CMD ["/usr/bin/portal"]
