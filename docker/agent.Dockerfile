# Default cowboy agent image: batteries-included dev environment.
# Built locally as `cowboy/agent:local` on first run (no registry required).
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

# Rust toolchain.
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal \
    && chmod -R a+rwX "$CARGO_HOME" "$RUSTUP_HOME"

# Go toolchain.
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in amd64) goarch=amd64;; arm64) goarch=arm64;; *) goarch=amd64;; esac; \
    curl -sSL "https://go.dev/dl/go1.23.4.linux-${goarch}.tar.gz" -o /tmp/go.tgz; \
    tar -C /usr/local -xzf /tmp/go.tgz; rm /tmp/go.tgz

# Unprivileged user the agent runs as (caps are dropped at the runtime layer).
RUN useradd -m -s /bin/bash agent

# cowboy helper binary (in-container `cowboy patch`, `cowboy proc`).
COPY cowboy-agent /usr/local/bin/cowboy
COPY agent-entrypoint.sh /usr/local/bin/agent-entrypoint.sh
RUN chmod +x /usr/local/bin/agent-entrypoint.sh || true

WORKDIR /workspace
