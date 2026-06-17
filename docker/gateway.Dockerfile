# cowboy-gateway image: the sole-egress network gateway.
# Build from the repo root (or `docker/build.sh gateway`):
#   docker build -f docker/gateway.Dockerfile -t ghcr.io/koshea/cowboy/gateway:<version> .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p cowboy-gateway

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        nftables iproute2 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/cowboy-gateway /usr/local/bin/cowboy-gateway
ENTRYPOINT ["/usr/local/bin/cowboy-gateway"]
