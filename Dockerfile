# sigproc — SDR RF front-end container for LibreSDR B210 (arm64)
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
COPY src/ src/
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

# Copy real sources and build the binary (only this layer invalidates on edits)
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release

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
