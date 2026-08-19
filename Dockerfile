# ---- build ----
FROM rust:1.90-slim-bookworm AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates && rm -rf /var/lib/apt/lists/*

# Cargo.lock is copied deliberately: without it cargo re-resolves inside the image and can
# pull newer dependencies than the ones this project was tested against. A release build
# must be reproducible.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --bin copytrader && strip target/release/copytrader

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 -s /usr/sbin/nologin copytrader
COPY --from=builder /build/target/release/copytrader /usr/local/bin/copytrader
# Never run the trading engine as root.
USER copytrader
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/copytrader"]
