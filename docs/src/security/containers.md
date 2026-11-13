# Containers

The agent can run containers (Testcontainers, its own Compose stack) when you turn
it on. It never gets your container engine's socket: a process that can talk to that
socket *is* you (`docker run -v $HOME:/h …` reads your SSH keys). It gets
**cowboy's container proxy** instead, a Docker Engine API server on a socket inside
the sandbox, run host-side by the worker, which reads and judges every request
before anything reaches the engine. Linux only for now.

## Turning it on

Host-owned `.cowboy/security.yaml` only. The personal overlay and the agent-editable
`agent.yaml` cannot turn it on, so a repository cannot grant itself an engine.

```yaml
sandbox:
  containers:
    enabled: true
    # Registries images may be pulled from (suffix-matched). Empty: no pulls.
    allow_registries: [docker.io, ghcr.io, quay.io, gcr.io, mcr.microsoft.com]
    build: false          # `docker build` / ImageFromDockerfile
    max_containers: 40
    # engine_socket: ~/... # default: $DOCKER_HOST, then rootless podman, then Docker
```

Your engine must be running: for rootless podman, `systemctl --user enable --now
podman.socket`. A session that cannot reach it says so and carries on without
containers.

Inside, `DOCKER_HOST=unix:///run/cowboy/docker.sock` is set, Testcontainers' reaper
(Ryuk) is turned off (`TESTCONTAINERS_RYUK_DISABLED`), and the Compose v2 plugin is on
`PATH` as `docker-compose`. Use `docker-compose`, not `docker compose`: on a podman
host the `docker` front end is podman, which speaks a different API the proxy does not
serve.

## What the proxy enforces

Every rule is decided on the host from the parsed request. The agent writes the
request, so nothing in it is trusted. A refusal is an HTTP 403 whose message starts
`cowboy:` and names the field, so a client shows exactly what was refused.

- **Mounts** must come from the project or the session's temporary directories
  (`/tmp`, `/var/tmp`). The sandbox path is translated to the host path the engine
  mounts, after resolving symlinks, so `./link -> ~/.ssh` resolves outside and is
  refused. Nothing under `.cowboy/` is mountable. Named volumes are fine; they are
  created labelled as the session's. Volume driver options are refused, because one
  can mount a host path.
- **Privilege** is refused: `Privileged`, added capabilities, devices, unmasked
  `/proc` paths, a different runtime, security options other than
  `no-new-privileges`, sysctls outside `net.*`, remote log drivers. A `HostConfig`
  field the proxy does not recognise is refused rather than passed on.
- **Host namespaces** are refused: `network_mode: host`, `container:` and `ns:`
  networks, `pid: host`, `ipc: host`, the host user namespace.
- **Networks are internal.** Every container joins a network with no route out,
  the session's own or one the session created (Compose's `<project>_default`),
  all created `internal`. Containers reach each other by name; they cannot reach the
  internet or your machine, so they are not a second, unfiltered way out around the
  egress relay.
- **Published ports** bind the host's `127.0.0.1` only (never every address, even
  for `PublishAllPorts`, which Testcontainers asks for) and are relayed into the
  sandbox, so `localhost:<mapped port>` works inside. The published set follows the
  running containers: a port is withdrawn when its container stops, before anything
  else on the host can take it.
- **Images:** pulls only from `allow_registries`, with short names qualified so the
  engine cannot resolve them to another registry. **Push is always refused**: it is
  the clean way to send the project out. Removing images is refused: the store is
  shared with yours. Building is off unless `build: true`, and then `RUN` steps get
  no network.
- **Only this session's objects.** Everything the proxy creates is labelled with the
  session, and any request naming a container, network, volume or exec must name one
  of the session's; anything else (your own stack's containers included) is reported
  as not found. Listings and the event stream are filtered to the session. A Compose
  project whose volume or network names collide with your own stack's is refused, with
  a hint to set `COMPOSE_PROJECT_NAME`.
- **One request per connection.** The proxy forwards with `Connection: close` and
  stops reading after the request it judged, so a second request on a kept-alive
  connection cannot ride past the filter. Only `attach` and `exec start` on the
  session's own containers become a two-way stream, and only once the engine has
  actually switched to one.
- The libpod API, swarm, plugins, `auth` and anything else not listed are refused.

When the session ends, every container, network and volume it created is removed.

## What this weakens, plainly

- Containers run under your engine's own rootless isolation, not cowboy's Landlock
  and seccomp. A kernel-level container escape is in scope where it was not.
- The project is writable from containers (it already is by the agent).
- With `build: true`, a Dockerfile's `RUN` runs arbitrary code at build time, inside
  the build container with no network.
- The proxy is new code on the boundary. It is unit-tested rule by rule
  (`sandbox/containers/filter.rs`) and against a real engine
  (`tests/sandbox_containers.rs`), but it is the part to review first.

## Not yet

- Your own shared stack (started outside cowboy) is not reachable from the sandbox.
- macOS.
- Containers left by a session that crashed are not swept until removed by hand
  (they carry the `cowboy.session` label).
