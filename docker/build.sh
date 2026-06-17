#!/usr/bin/env bash
# Build the cowboy container images from the repo root.
#
#   docker/build.sh            # build both (agent + gateway)
#   docker/build.sh agent      # build just the agent image
#   docker/build.sh gateway    # build just the gateway image
set -euo pipefail

# Resolve the repo root (this script lives in <root>/docker).
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Tag with the same version-pinned GHCR names the `cowboy` binary expects, so a
# local build is picked up directly (the binary uses an image that already
# exists locally instead of pulling). Override with COWBOY_*_IMAGE for ad-hoc tags.
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
AGENT_IMAGE="${COWBOY_AGENT_IMAGE:-ghcr.io/koshea/cowboy/agent:${VERSION}}"
GATEWAY_IMAGE="${COWBOY_GATEWAY_IMAGE:-ghcr.io/koshea/cowboy/gateway:${VERSION}}"

build_agent() {
    echo "==> building $AGENT_IMAGE (batteries-included; this takes a few minutes)"
    docker build -f docker/agent.Dockerfile -t "$AGENT_IMAGE" .
}
build_gateway() {
    echo "==> building $GATEWAY_IMAGE"
    docker build -f docker/gateway.Dockerfile -t "$GATEWAY_IMAGE" .
}

case "${1:-all}" in
    agent) build_agent ;;
    gateway) build_gateway ;;
    all) build_gateway; build_agent ;;
    *) echo "usage: $0 [all|agent|gateway]" >&2; exit 2 ;;
esac
echo "==> done"
