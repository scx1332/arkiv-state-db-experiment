# The node: arkiv-proposer (producer, full and light roles, genesis tools).
#
#   docker build -t arkiv-node .
#   docker run --rm arkiv-node --version
#
# Runs as uid 10001 with all state under /data (mount a volume there). Keys and
# ARKIV_PROPOSER_TOKEN come from mounted files and the environment (Kubernetes
# Secrets). The binary binds 127.0.0.1 by default: pass --listen 0.0.0.0:9500
# (and --private-listen/--metrics-listen) to reach it from outside.
FROM rust:1.90-bookworm AS build
WORKDIR /work
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY vendor ./vendor
# arkiv-node compiles this genesis preset in (genesis_authoring.rs).
COPY docs/examples/genesis-bundle/profiles ./docs/examples/genesis-bundle/profiles
RUN cargo build --locked --release -p arkiv-node --bin arkiv-proposer \
    && strip target/release/arkiv-proposer

FROM debian:bookworm-slim
# curl serves health checks against the public status endpoint. The upgrade
# takes Debian's security fixes that the base image tag does not have yet
# (the image scan fails on any fixable HIGH or CRITICAL finding).
RUN apt-get update && apt-get upgrade -y \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --user-group --no-create-home --home-dir /data arkiv \
    && mkdir /data && chown arkiv:arkiv /data
COPY --from=build /work/target/release/arkiv-proposer /usr/local/bin/arkiv-proposer
USER 10001:10001
WORKDIR /data
VOLUME /data
# Public API and JSON-RPC, private listener (replication), Prometheus metrics.
EXPOSE 9500 9501 9600
HEALTHCHECK --interval=30s --timeout=5s --start-period=2h \
    CMD curl -fsS http://127.0.0.1:9500/signed/v1/status >/dev/null || exit 1
ENTRYPOINT ["arkiv-proposer"]
