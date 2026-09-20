# vita49-sink — verification consumer image (arm64)
#
# The sink is what makes the chain's output assertable from outside the process:
# it parses real UDP VITA49 packets with the shared parser and prints a JSON
# verdict. Deployed both as the gate's Job and (until the demod exists) as a
# standing consumer, so the chain always terminates in something that can say
# whether the stream is healthy.
#
# Same cargo-chef pattern and workspace caveat as channelizer/Dockerfile.

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
COPY sink/ sink/
COPY capture/ capture/
COPY channelizer/ channelizer/
COPY fixture/ fixture/
RUN cargo build --release --package vita49-sink

# ── Stage 4: Runtime ─────────────────────────────────────────────────────────
FROM --platform=linux/arm64 debian:trixie-slim
COPY --from=builder /build/target/release/vita49-sink /usr/local/bin/vita49-sink
ENTRYPOINT ["/usr/local/bin/vita49-sink"]
