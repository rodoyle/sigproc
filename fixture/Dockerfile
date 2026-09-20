# sigproc-fixture — deterministic test-vector generator image (arm64)
#
# Used as an **init container** in the channelizer and gate pods: it writes a
# VITA49 fixture file into a shared emptyDir, so the end-to-end gate has
# deterministic input inside the cluster without a PVC, a host copy or a
# synthetic producer service.
#
# Same cargo-chef pattern and workspace caveat as channelizer/Dockerfile: the
# planner stage needs every member's manifest and source, because
# `cargo chef prepare` resolves the whole workspace.

# ── Stage 1: Chef ────────────────────────────────────────────────────────────
FROM --platform=linux/arm64 rust:latest AS chef
RUN rustup default nightly
RUN cargo install cargo-chef

# ── Stage 2: Planner ─────────────────────────────────────────────────────────
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

# ── Stage 3: Builder ─────────────────────────────────────────────────────────
FROM chef AS builder
WORKDIR /build
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY common/ common/
COPY fixture/ fixture/
COPY capture/ capture/
COPY channelizer/ channelizer/
COPY sink/ sink/
RUN cargo build --release --package sigproc-fixture

# ── Stage 4: Runtime ─────────────────────────────────────────────────────────
FROM --platform=linux/arm64 debian:trixie-slim
COPY --from=builder /build/target/release/sigproc-fixture /usr/local/bin/sigproc-fixture

# Non-root by construction: this runs as an init container whose pods set
# runAsNonRoot, and fsGroup makes the shared fixture emptyDir writable here.
RUN groupadd --gid 10001 sigproc \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin sigproc
USER 10001:10001

ENTRYPOINT ["/usr/local/bin/sigproc-fixture"]
