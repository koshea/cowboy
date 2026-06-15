# The boundary

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
- **Provider credentials live only in the home dir.** Endpoint URLs and API keys
  are stored in `~/.config/cowboy/providers.yaml` (`0600`), consumed host-side
  when building the model client, and never written into a project or mounted —
  so the agent cannot reach them by construction. Project `models.yaml` files
  reference a provider by name and are forbidden (by `deny_unknown_fields`) from
  carrying credentials.
- **Config validation refuses to expose security config.** `SecurityConfig::validate`
  rejects any mount whose source is `security.yaml` or the `.cowboy` directory.
- **Network egress is route-enforced, not prompt-enforced.** The agent runs on
  an internal-only Docker network with its default route forced through the
  gateway and `NET_ADMIN`/`NET_RAW` dropped, so it cannot change its route. See
  [Network gateway](network.md). Default external policy is `ask`; with no
  approver connected, asks **fail closed** (deny).
- **Secrets are explicit and host-configured.** `secrets.env` injects host env
  vars into the container by name. Values are never printed at startup or written
  to logs; only names appear.

## Where the agent loop runs

The agent loop runs **host-side** (in the worker process), not inside the
container. The Docker container is the sandbox for the agent's *shell commands*.
Host-handled tools (memory, artifacts, handoffs, plan, decisions, scope
proposals) are executed by the loop on the host and so can read/write
host-visible state under the workspace — but never the masked host-owned config
or the home-only credentials.

## Dangerous options (surfaced, not silently honored)

`cowboy doctor` warns when `container.privileged` or `container.docker_socket`
is enabled — both widen the boundary and should be used deliberately.

## Follow-ups

- Log redaction, per-command secret exposure, secret provenance, and integration
  with 1Password/Vault/SOPS.
- Full DNS policy and DNS-tunnel detection.
