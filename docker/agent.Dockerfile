# Default cowboy agent image: batteries-included dev environment with the
# in-container `cowboy` helper baked in.
#
# Build from the repo root (or just `docker/build.sh agent`, which tags it with
# the version-pinned name the `cowboy` binary expects):
#   docker build -f docker/agent.Dockerfile -t ghcr.io/koshea/cowboy/agent:<version> .

# --- stage 1: build the in-container `cowboy` helper ---
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p cowboy-cli

# --- stage 2: the agent runtime image ---
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    MISE_ENV=devcontainer \
    MISE_DATA_DIR=/usr/local/share/mise \
    MISE_TRUSTED_CONFIG_PATHS=/workspace \
    MISE_YES=1 \
    PATH=/usr/local/share/mise/shims:/usr/local/cargo/bin:/usr/local/go/bin:$PATH

# Core CLI tooling + language toolchains commonly needed by coding tasks.
# The lib*-dev set is the standard ruby-build / native-extension dependency set so
# `mise install` can compile language runtimes from source — notably Ruby, whose
# psych (libyaml), fiddle (libffi), and zlib extensions otherwise fail to build.
RUN apt-get update && apt-get install -y --no-install-recommends \
        bash git curl wget ca-certificates \
        ripgrep fd-find jq \
        python3 python3-pip python3-venv \
        nodejs npm \
        build-essential make gcc pkg-config autoconf bison \
        libssl-dev openssl \
        libyaml-dev libffi-dev zlib1g-dev \
        libreadline-dev libgmp-dev libncurses-dev libgdbm-dev libdb-dev uuid-dev \
        sqlite3 postgresql-client redis-tools \
        iproute2 util-linux \
    && ln -sf /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

# GitHub CLI (`gh`) — commonly used for PR review/creation, issues, releases.
# Not in Debian's repos, so add GitHub's signed apt source.
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends gh \
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

# mise (https://mise.jdx.dev) — the preferred way to manage per-project dev
# dependencies (language runtimes, tools, env). Cowboy auto-runs `mise install`
# at launch when the workspace has a mise config, and the image defaults to
# MISE_ENV=devcontainer. Installed system-wide with a shared, world-writable
# data dir so its shims work for the non-root agent (HOME=/tmp at runtime).
RUN curl -fsSL https://mise.run | MISE_INSTALL_PATH=/usr/local/bin/mise sh \
    && mkdir -p /usr/local/share/mise \
    && chmod -R a+rwX /usr/local/share/mise

# Make the toolchains available in login shells too (interactive `cowboy shell`),
# and activate mise so its managed tools + env are on PATH there.
RUN printf 'export PATH=/usr/local/cargo/bin:/usr/local/go/bin:$PATH\n' \
        > /etc/profile.d/cowboy.sh \
    && printf 'eval "$(/usr/local/bin/mise activate bash)"\n' >> /etc/profile.d/cowboy.sh

# The mounted /workspace is owned by the host user; let in-container git treat
# it as safe so `cowboy patch` works without "dubious ownership" errors.
RUN git config --system --add safe.directory '*'

# HOME=/tmp at runtime and the agent runs as the (dynamic) host uid, with no
# passwd entry. Credential grants bind-mount into /tmp/.config/<tool>; Docker
# would otherwise synthesize the missing /tmp/.config parent as root:root 0755,
# locking the agent out of writing sibling configs there (e.g. gcloud failing to
# create /tmp/.config/gcloud). Pre-create the XDG base dirs world-writable so any
# runtime uid can populate them and credential mounts land in an existing parent.
RUN mkdir -p /tmp/.config /tmp/.cache /tmp/.local/share /tmp/.local/state \
    && chmod -R 1777 /tmp/.config /tmp/.cache /tmp/.local

# Unprivileged user the agent runs as (network caps are dropped at the runtime layer).
RUN useradd -m -s /bin/bash agent

# The in-container `cowboy` helper (e.g. `cowboy patch show`) + entrypoint.
COPY --from=build /src/target/release/cowboy /usr/local/bin/cowboy
COPY docker/agent-entrypoint.sh /usr/local/bin/agent-entrypoint.sh
RUN chmod +x /usr/local/bin/agent-entrypoint.sh

WORKDIR /workspace
