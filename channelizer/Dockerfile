# sigproc-channelizer — channelizer service image (arm64)
#
# Builds ONLY the `sigproc-channelizer` workspace member, and carries **no UHD**.
# That is the point of the microservice split: the capture image has to ship
# libuhd, uhd-host and the FPGA images to a node with a USB radio; this service
# needs a small binary and a slim base, because it only reads a UDP stream.
#
# Architecture: the cluster's kaniko builder runs on arm64 nodes, so this image is
# arm64 and `deploy/channelizer-deployment.yaml` pins kubernetes.io/arch=arm64.
# The platform flag makes that explicit rather than incidental.
#
# Rust dependency caching uses the cargo-chef 3-stage pattern:
#   chef    — toolchain + cargo-chef binary
#   planner — generates recipe.json from the real source tree
#   builder — cooks deps from recipe.json (cached layer), then builds one package
#
# Workspace caveat: `cargo chef prepare` resolves the WHOLE workspace, so every
# member's manifest AND source must exist in the planner stage. Copying only
# channelizer/ would make the recipe wrong and silently degrade the cached
# dependency layer into a full rebuild.

# ── Stage 1: Chef — toolchain + cargo-chef ────────────────────────────────────
FROM --platform=linux/arm64 rust:latest AS chef
RUN rustup default nightly
RUN cargo install cargo-chef

# ── Stage 2: Planner — derive the dependency recipe from real sources ─────────
FROM chef AS planner
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY common/Cargo.toml common/
COPY capture/Cargo.toml capture/
COPY channelizer/Cargo.toml channelizer/
COPY fixture/Cargo.toml fixture/
COPY sink/Cargo.toml sink/
COPY common/src/ common/src/
COPY capture/src/ capture/src/
COPY channelizer/src/ channelizer/src/
COPY fixture/src/ fixture/src/
COPY sink/src/ sink/src/
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: Builder — cached deps, then the crate ───────────────────────────
FROM chef AS builder
WORKDIR /build

# Cook dependencies only — this layer is cached while recipe.json is unchanged
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Copy real sources and build just this service
COPY Cargo.toml Cargo.lock ./
COPY common/ common/
COPY channelizer/ channelizer/
COPY capture/ capture/
COPY fixture/ fixture/
COPY sink/ sink/
RUN cargo build --release --package sigproc-channelizer

# ── Stage 4: Runtime ─────────────────────────────────────────────────────────
FROM --platform=linux/arm64 debian:trixie-slim

# The service speaks UDP and serves metrics over HTTP; it needs no runtime libs
# beyond glibc and CA roots.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/sigproc-channelizer /usr/local/bin/sigproc-channelizer

# Default config path (overridden by the ConfigMap mount in k8s)
ENV SIGPROC_CONFIG=/etc/sigproc/config.toml
RUN mkdir -p /etc/sigproc

ENTRYPOINT ["/usr/local/bin/sigproc-channelizer"]
