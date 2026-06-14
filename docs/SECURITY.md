# cowboy — security model

The central principle: **the agent is not trusted for security**. Controls are
enforced by Docker, host-owned configuration, and a Cowboy-controlled network
gateway — never by prompting the model.

## Boundaries

- **Host-owned config is never mounted.** `security.yaml` controls the image,
  mounts, networks, policy, and secrets. The project is mounted at `/workspace`,
  but `security.yaml` and `models.yaml` are **masked** with an empty read-only
  file inside the container, so the agent cannot read them even though they live
  under `.cowboy/`. (Enforced in `AgentRuntime::build_spec`; proven by the
  `security_yaml_is_masked_inside_container` E2E test.)
- **Config validation refuses to expose security config.** `SecurityConfig::validate`
  rejects any mount whose source is `security.yaml` or the `.cowboy` directory.
- **Network egress is route-enforced, not prompt-enforced.** The agent runs on
  an internal-only Docker network with its default route forced through the
  gateway and `NET_ADMIN`/`NET_RAW` dropped, so it cannot change its route. See
  [NETWORK.md](NETWORK.md). Default external policy is `ask`; with no approver
  connected, asks **fail closed** (deny).
- **Secrets are explicit and host-configured.** `secrets.env` injects host env
  vars into the container by name. Values are never printed at startup or
  written to logs; only names appear.

## Dangerous options (surfaced, not silently honored)

`cowboy doctor` warns when `container.privileged` or `container.docker_socket`
is enabled — both widen the boundary and should be used deliberately.

## Follow-ups (post-MVP)

- Host control-socket server + TUI approval for live `ask` decisions.
- Log redaction, per-command secret exposure, secret provenance, and
  integration with 1Password/Vault/SOPS.
- Full DNS policy and DNS-tunnel detection.
