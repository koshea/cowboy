# Default cowboy agent image: batteries-included dev environment with the
# in-container `cowboy` helper baked in.
#
# Build from the repo root:
#   docker build -f docker/agent.Dockerfile -t cowboy/agent:local .

# --- stage 1: build the in-container `cowboy` helper ---
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p cowboy-cli

# --- stage 2: the agent runtime image ---
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive \
    PATH=/usr/local/cargo/bin:/usr/local/go/bin:$PATH \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup

# Core CLI tooling + language toolchains commonly needed by coding tasks.
RUN apt-get update && apt-get install -y --no-install-recommends \
        bash git curl wget ca-certificates \
        ripgrep fd-find jq \
        python3 python3-pip python3-venv \
        nodejs npm \
        build-essential make gcc pkg-config \
        libssl-dev openssl \
        sqlite3 postgresql-client redis-tools \
        iproute2 util-linux \
    && ln -sf /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

# pnpm via corepack (ships with modern node).
RUN corepack enable || npm install -g pnpm || true

# Rust toolchain (for the agent's own coding tasks).
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal \
    && chmod -R a+rwX "$CARGO_HOME" "$RUSTUP_HOME"

# Go toolchain.
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in amd64) goarch=amd64;; arm64) goarch=arm64;; *) goarch=amd64;; esac; \
    curl -sSL "https://go.dev/dl/go1.23.4.linux-${goarch}.tar.gz" -o /tmp/go.tgz; \
    tar -C /usr/local -xzf /tmp/go.tgz; rm /tmp/go.tgz

# Make the toolchains available in login shells too (interactive `cowboy shell`).
RUN printf 'export PATH=/usr/local/cargo/bin:/usr/local/go/bin:$PATH\n' \
        > /etc/profile.d/cowboy.sh

# The mounted /workspace is owned by the host user; let in-container git treat
# it as safe so `cowboy patch` works without "dubious ownership" errors.
RUN git config --system --add safe.directory '*'

# Unprivileged user the agent runs as (network caps are dropped at the runtime layer).
RUN useradd -m -s /bin/bash agent

# The in-container `cowboy` helper (e.g. `cowboy patch show`) + entrypoint.
COPY --from=build /src/target/release/cowboy /usr/local/bin/cowboy
COPY docker/agent-entrypoint.sh /usr/local/bin/agent-entrypoint.sh
RUN chmod +x /usr/local/bin/agent-entrypoint.sh

WORKDIR /workspace
