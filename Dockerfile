# sigproc — SDR RF front-end container for LibreSDR B210 (arm64)
#
# Multi-stage build targeting linux/arm64 for Raspberry Pi 4 deployment.
# Kaniko builds this natively on an arm64 cluster node; no QEMU needed.

# ── Stage 1: Builder ──────────────────────────────────────────────────────────
# Nightly Rust per locked decision #6 (NEON intrinsics for ARM64).
FROM --platform=linux/arm64 rust:nightly AS builder

# Install UHD build-time dependencies.
# Try Ettus PPA first; fall back to source build if arm64 packages unavailable.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        software-properties-common \
        python3 \
        ca-certificates \
        wget \
        cmake \
        build-essential \
        libusb-1.0-0-dev \
    && ( \
        add-apt-repository -y ppa:ettusresearch/uhd \
        && apt-get update \
        && apt-get install -y --no-install-recommends libuhd-dev uhd-host \
    ) || ( \
        echo "Ettus PPA unavailable (arm64 fallback) — building UHD from source" \
        && mkdir -p /tmp/uhd-build \
        && cd /tmp/uhd-build \
        && wget -q https://github.com/EttusResearch/uhd/releases/download/v4.7.0.0/uhd-4.7.0.0.tar.gz \
        && tar xzf uhd-4.7.0.0.tar.gz \
        && cd uhd-4.7.0.0/host \
        && mkdir build && cd build \
        && cmake -DCMAKE_BUILD_TYPE=Release .. \
        && make -j$(nproc) \
        && make install \
        && ldconfig \
        && rm -rf /tmp/uhd-build \
    ) \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# Download UHD FPGA images (B200/B210 firmware)
RUN /usr/lib/uhd/utils/uhd_images_downloader.py -i /usr/share/uhd/images

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
FROM --platform=linux/arm64 debian:bookworm-slim

# Install UHD runtime (libs + tools including uhd_usrp_probe for verification)
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        software-properties-common \
        python3 \
        ca-certificates \
    && add-apt-repository -y ppa:ettusresearch/uhd \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        uhd-host \
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
