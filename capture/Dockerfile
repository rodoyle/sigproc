# sigproc (capture) — SDR RF front-end container for LibreSDR B210 (arm64)
#
# Builds ONLY the `capture` workspace member. The other members
# (channelizer, fixture, sink) have their own Dockerfiles because they have no
# UHD dependency at all and are an order of magnitude cheaper to build.
#
# Multi-stage build targeting linux/arm64 for Raspberry Pi 4 deployment.
# Kaniko builds this natively on an arm64 cluster node; no QEMU needed.
#
# UHD comes from Debian's native repos (no Ubuntu PPA — Debian trixie removed
# software-properties-common and PPAs don't target trixie). rust:latest is
# Debian-trixie-based as of 2026.
#
# Rust dependency caching uses the cargo-chef 3-stage pattern:
#   chef    — toolchain + cargo-chef binary
#   planner — generates recipe.json from the real source tree
#   builder — cooks deps from recipe.json (cached layer), then builds the crate
#
# Workspace caveat: `cargo chef prepare` resolves the WHOLE workspace, so every
# member's manifest AND source must exist in the planner stage. Copying only
# capture/ would make the recipe wrong (missing members) and silently degrade
# the cached dependency layer into a full rebuild.

# ── Stage 1: Chef — toolchain + cargo-chef ────────────────────────────────────
# Nightly Rust per locked decision #6 (NEON intrinsics for ARM64).
# Docker Hub has no `rust:nightly` tag — use latest stable + rustup nightly.
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

# ── Stage 3: Builder — UHD, cached deps, then the crate ──────────────────────
FROM chef AS builder

# UHD build-time deps.
#   pkg-config  — uhd-sys uses metadeps to locate the UHD library
#   libclang    — uhd-sys uses bindgen to generate bindings from uhd.h
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libuhd-dev \
        uhd-host \
        pkg-config \
        libclang-dev \
        clang \
        python3 \
        ca-certificates \
        wget \
        cmake \
        build-essential \
        libusb-1.0-0-dev \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# Debian's uhd.pc advertises -lboost_system, but Boost >= 1.69 ships
# boost_system as header-only and Debian trixie provides no
# libboost_system.so (libboost-system-dev is a docs-only transitional
# package). uhd-sys reads its link flags from this .pc via metadeps, so the
# final link fails with "cannot find -lboost_system". Drop the stale flag.
RUN PC=$(find /usr -name uhd.pc -print -quit) \
    && test -n "$PC" \
    && sed -i 's/[[:space:]]*-lboost_system//g' "$PC" \
    && echo "patched $PC:" \
    && cat "$PC"

# UHD FPGA images. Debian installs the downloader in /usr/libexec/uhd/utils
# with a wrapper in /usr/bin. Non-fatal — the LibreSDR B210 FPGA is fetched
# separately below regardless.
RUN uhd_images_downloader -i /usr/share/uhd/images \
    || echo "WARN: uhd_images_downloader failed — continuing (LibreSDR FPGA fetched separately)"

# LibreSDR B210-specific FPGA binary
RUN wget -q \
        https://github.com/lmesserStep/LibreSDRB210/raw/main/usrp_b210_fpga.bin \
        -O /usr/share/uhd/images/usrp_b210_fpga.bin

WORKDIR /build

# Cook dependencies only — this layer is cached while recipe.json is unchanged
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Copy real sources and build the capture binary (only this layer invalidates)
COPY Cargo.toml Cargo.lock ./
COPY common/ common/
COPY capture/ capture/
COPY channelizer/ channelizer/
COPY fixture/ fixture/
COPY sink/ sink/
RUN cargo build --release --package sigproc

# ── Stage 4: Runtime ─────────────────────────────────────────────────────────
FROM --platform=linux/arm64 debian:trixie-slim

# UHD runtime (libs + tools including uhd_usrp_probe for verification)
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        uhd-host \
        python3 \
        ca-certificates \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# Copy UHD FPGA images from builder
COPY --from=builder /usr/share/uhd/images /usr/share/uhd/images

# Copy compiled Rust binary
COPY --from=builder /build/target/release/sigproc /usr/local/bin/sigproc

# UHD environment
ENV UHD_IMAGES_DIR=/usr/share/uhd/images

# Default config path (overridden by ConfigMap mount in k8s)
ENV SIGPROC_CONFIG=/etc/sigproc/config.toml

RUN mkdir -p /etc/sigproc

ENTRYPOINT ["/usr/local/bin/sigproc"]
