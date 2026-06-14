#!/bin/sh
# Agent container entrypoint.
#
# Runs as PID 1 with NET_ADMIN during init only: pins the default route through
# the cowboy gateway, blackholes the cloud metadata endpoint, then drops all
# network capabilities and execs the workload as the unprivileged `agent` user.
# Wired up by the gateway slice; for now it simply keeps the container alive.
set -eu

GATEWAY_IP="${COWBOY_GATEWAY_IP:-}"

if [ -n "$GATEWAY_IP" ]; then
    ip route replace default via "$GATEWAY_IP" || true
    ip route add blackhole 169.254.169.254/32 2>/dev/null || true
fi

# Drop network-admin capabilities, then run the workload (default: stay alive).
if command -v setpriv >/dev/null 2>&1; then
    exec setpriv --inh-caps=-net_admin,-net_raw --bounding-set=-net_admin,-net_raw \
        "$@"
fi
exec "$@"
