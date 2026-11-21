# Devcontainers

`ldg up [PATH]` looks for the devcontainer config under the project path
(default: current directory), in spec order:

1. `PATH/.devcontainer/devcontainer.json`
2. `PATH/.devcontainer.json`
3. `PATH/.devcontainer/<folder>/devcontainer.json` (one level deep; with
   several, the alphabetically first wins and a warning lists the choice)

The path is made absolute first, so `ldg up`, `ldg up .` and `ldg up $PWD` all
mean the same container. The config is JSONC, and lodge supports the parts of
the spec real devcontainers use (it does not aim for full conformance).

## Backends

- **`dockerComposeFile`** + `service`: lodge drives `docker compose` under a
  project name derived from the project path (`-p lodge-<name>-<hash>`), so two
  projects whose compose files both live in `.devcontainer/` don't collide on
  compose's directory-derived name. `runServices` limits what starts.
- **`image`**: lodge runs the image as a `sleep infinity` container (unless
  `overrideCommand` is false) with the workspace bind-mounted, honoring
  `workspaceFolder`/`workspaceMount`, `mounts` and `runArgs`.
- **`build`** / **`dockerFile`**: lodge builds the image (honoring `context`,
  `dockerfile`, `target`, `args`) and runs it the same way.

For the image/build backends, `privileged`, `capAdd`, `securityOpt`, `init`,
`containerUser` and `appPort` (published on loopback only) are applied when the
container is created; with compose, those belong in the compose file.

Every container lodge creates carries the label `dev.lodge.project=<path>`,
which is how `ldg ls` finds them, and the two labels the devcontainer CLI and
VS Code write (`devcontainer.local_folder` and `devcontainer.config_file`), so
"Attach to Running Container" and the CLI find lodge's container too.

The other way round works as well: when lodge has no container for a project but
one with those labels exists for the same folder and config file (VS Code made it),
`ldg up` uses it. It is started if it is stopped and lodge injects into it, and
that is all: lodge installs no features and runs no hooks in it (the editor did),
and `down` and `rm` refuse to stop or remove it, because it is the editor's.
`ldg up --build` makes a container of lodge's own beside it.

## Lifecycle hooks

`initializeCommand` runs on the host (the remote host with `--remote`) before
the container starts. In the container, as `remoteUser`:

- `onCreateCommand`, `updateContentCommand`, `postCreateCommand` run once
  *successfully*: a marker is written only when the hook exits 0, so a failed
  hook is retried on the next `ldg up`.
- `postStartCommand` runs on every `ldg up`.
- `postAttachCommand` runs right before the shell opens.

All three forms work: a string (`sh -c`), an array (argv), and an object of
named commands. A failing hook is reported but never locks you out.

## Features

A feature is either a registry reference (`ghcr.io/devcontainers/features/node:1`)
or a folder in the project (`./local-feature`; with `--remote`, on the remote
host). Their `install.sh` runs in the container, as root, when it is created:
options become environment variables, `_REMOTE_USER` / `_CONTAINER_USER` and
their homes are set, and `containerEnv` is written to `/etc/profile.d` so login
shells pick it up.

Registry features are pulled by lodge itself, so there is nothing to install. The
pull is anonymous unless you have run `docker login` for that registry: lodge
reads the login from docker's config file (`~/.docker/config.json`, or
`$DOCKER_CONFIG`), or asks the credential helper it names (`credsStore`,
`credHelpers`; `docker-credential-<name> get`). Everything pulled is
checked against its digest. If a pull fails, see
[Troubleshooting](troubleshooting.md#a-bundle-or-a-feature-cannot-be-pulled-or-published).

Order: `dependsOn` features are installed too (with the options it gives, unless
you list the feature yourself), and before the feature that needs them; one that
can't be fetched takes what depends on it down with it. `installsAfter` only
orders features that are both present. `overrideFeatureInstallOrder` comes
first, then the feature's reference decides ties.

lodge installs features *in the container* rather than baking them into the
image, so a feature's `mounts`, `privileged`, `capAdd`, `securityOpt` and
`init` are applied when the container is created, and its `entrypoint` is
ignored (lodge runs `sleep infinity`). A feature that failed to install is
retried on the next `ldg up`.

## Variables and environment

`${localEnv:VAR}`, `${localEnv:VAR:default}`, `${containerWorkspaceFolder}`,
`${localWorkspaceFolder}` and `${devcontainerId}` (lodge's own stable
per-project id) are substituted. `containerEnv` and `remoteEnv` are exported to
hooks and written to `/etc/profile.d/lodge-env.sh` for login shells.

`updateRemoteUserUID` (default true, image/build backend) aligns the container
user's uid, and a private group's gid, with yours at creation, so the
bind-mounted workspace is writable.

## Change detection

When the container already exists, lodge compares the current `.devcontainer/`
contents (JSON canonicalized, so comments and formatting don't count; plus any
compose files) with a hash recorded in the container at creation. If they
differ, it says so and keeps the container. `ldg up --build` rebuilds and
recreates it.

Known limit: every file under `.devcontainer/` counts, so a hook that writes
there makes the next run report a change.

## Port forwarding

`forwardPorts` entries are forwarded from the moment the container is up
(connections fail until the service listens):

- `3000` / `"3000"`: local port 3000 → port 3000 of the container.
- `"db:5432"`: local 5432 → port 5432 of `db` on the container network (another
  compose service). Resolved inside the container, falling back to container
  names; if the service isn't up yet, lodge keeps retrying.
- `"3000:8080"`: local 3000 → container port 8080.

Beyond that, lodge watches `/proc/net/tcp` in the container and forwards any port
that starts listening to the same port here (loopback-only listeners are
skipped). Forwards always bind `127.0.0.1`. A small daemon per container does
this, with a built-in TCP proxy locally or `ssh -L` tunnels with `--remote`.
`ldg ports` shows what it has forwarded, `ldg logs` its log.

Dynamic forwarding needs the container's bridge IP to be reachable from the
engine's host: Linux with Docker Engine, or any engine with `--remote`. Rootless
Podman has no such address: there, the ports the container publishes
(`appPort`) are forwarded, and the others are not. Docker
Desktop on macOS/Windows doesn't expose container IPs. A pod (`--target kubernetes`)
needs neither: each port is relayed by `kubectl port-forward`.

## Read and ignored

`userEnvProbe`, `portsAttributes`, `shutdownAction`, `waitFor`, `secrets` and
`customizations` are accepted and ignored: they are IDE behaviour, or not needed
here.
