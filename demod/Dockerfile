# sigproc-demod — standalone NBFM/AM audio service (linux/arm64, no UHD)
FROM --platform=linux/arm64 rust:latest AS chef
RUN rustup default nightly
RUN cargo install cargo-chef

FROM chef AS planner
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY common/Cargo.toml common/
COPY capture/Cargo.toml capture/
COPY channelizer/Cargo.toml channelizer/
COPY fixture/Cargo.toml fixture/
COPY sink/Cargo.toml sink/
COPY demod/Cargo.toml demod/
COPY common/src/ common/src/
COPY capture/src/ capture/src/
COPY channelizer/src/ channelizer/src/
COPY fixture/src/ fixture/src/
COPY sink/src/ sink/src/
COPY demod/src/ demod/src/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
WORKDIR /build
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY common/ common/
COPY capture/ capture/
COPY channelizer/ channelizer/
COPY fixture/ fixture/
COPY sink/ sink/
COPY demod/ demod/
RUN cargo build --release --package sigproc-demod

FROM --platform=linux/arm64 debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 sigproc \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin sigproc \
    && mkdir -p /recordings \
    && chown 10001:10001 /recordings
COPY --from=builder /build/target/release/sigproc-demod /usr/local/bin/sigproc-demod
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/sigproc-demod"]
