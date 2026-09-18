# sigproc — SDR RF front-end container for LibreSDR B210 (arm64)
#
# Multi-stage build targeting linux/arm64 for Raspberry Pi 4 deployment.
# Kaniko builds this natively on an arm64 cluster node; no QEMU needed.
#
# UHD is installed from Debian's native repos (no Ubuntu PPA — Debian
# trixie removed software-properties-common and Ubuntu PPAs don't target
# trixie). rust:latest is Debian-trixie-based as of 2026.

# ── Stage 1: Builder ──────────────────────────────────────────────────────────
# Nightly Rust per locked decision #6 (NEON intrinsics for ARM64).
# Docker Hub has no `rust:nightly` tag — use latest stable + rustup nightly.
FROM --platform=linux/arm64 rust:latest AS builder
RUN rustup default nightly && rustup target add aarch64-unknown-linux-gnu

# Install UHD build-time dependencies from Debian repos.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libuhd-dev \
        uhd-host \
        python3 \
        ca-certificates \
        wget \
        cmake \
        build-essential \
        libusb-1.0-0-dev \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# Download UHD FPGA images (B200/B210 firmware).
# Debian installs the tool at /usr/libexec/uhd/utils/ with a wrapper in /usr/bin.
# Non-fatal: the LibreSDR B210 FPGA is fetched separately below regardless.
RUN uhd_images_downloader -i /usr/share/uhd/images \
    || echo "WARN: uhd_images_downloader failed — continuing (LibreSDR FPGA fetched separately)"

# LibreSDR B210-specific FPGA binary
RUN wget -q \
        https://github.com/lmesserStep/LibreSDRB210/raw/main/usrp_b210_fpga.bin \
        -O /usr/share/uhd/images/usrp_b210_fpga.bin

# Install cargo-chef for Rust dependency layer caching
RUN cargo install cargo-chef

WORKDIR /build
COPY Cargo.toml Cargo.lock ./

# Prepare and cache dependencies (layer cache hit when Cargo.toml unchanged)
RUN cargo chef prepare --recipe-path recipe.json \
    && cargo chef cook --release --recipe-path recipe.json

# Copy source and build the binary
COPY src/ src/
RUN cargo build --release

# ── Stage 2: Runtime ───────────────────────────────────────────────────────────
FROM --platform=linux/arm64 debian:trixie-slim

# Install UHD runtime (libs + tools including uhd_usrp_probe for verification)
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
