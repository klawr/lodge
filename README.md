# lodge

**Inject your environment into any container.**

`lodge` (command: `ldg`) copies your personal tools (editor, shell utilities, …) — together with
the shared libraries they need — into a running Docker container and drops you
into a shell inside it. You get your toolchain in someone else's image without
rebuilding it, and **without** requiring your tools to be compatible with the
container's (often much older) glibc.

It also doubles as a lightweight [devcontainer](https://containers.dev) runner:
`ldg up` starts a project's `.devcontainer`, honors its lifecycle hooks and
features, then injects your environment on top.

```sh
ldg up                        # find the devcontainer config, start it, inject your environment, open a shell
ldg up ./some/project         # same, for a specific project path
ldg up --build                # force-rebuild images and recreate the container
ldg down                      # stop the project's container (resume later with `ldg up`)
ldg status                    # show the project's container and whether its config changed
ldg rm                        # remove the project's container (next `ldg up` creates a fresh one)
ldg doctor                    # check the host: engine, helper tools, config, listed binaries
ldg exec my-container         # inject into an already-running container by name/id
ldg up -r user@host           # operate against a remote Docker daemon over SSH
ldg up -r myhost ./project    # remote via a Host alias from ~/.ssh/config
ldg up -r user@host:2222      # remote on a non-standard SSH port
ldg up -v                     # verbose: print every docker/ssh command lodge runs
ldg --version                 # print the installed version
```

`-r/--remote` and `-v/--verbose` work on every subcommand. `--verbose` prints
each `docker`/`ssh` command before it runs; repeating it (`-vv`, `-vvv`) passes
matching `-v` flags to lodge's own ssh connections (config probe, port tunnels)
for connection-level debugging.

## Install

### From a release (prebuilt binary)

```sh
curl -fsSL https://raw.githubusercontent.com/klawr/lodge/master/install.sh | sh
```

Downloads the latest static Linux binary into `~/.local/bin`. Overrides:

| variable | default | meaning |
| --- | --- | --- |
| `LODGE_BIN_DIR` | `~/.local/bin` | install location |
| `LODGE_VERSION` | latest | pin to e.g. `v0.1.0` |

### From source

```sh
cargo install --git https://github.com/klawr/lodge   # needs a Rust toolchain
```

Or from a clone:

```sh
make install                    # installs to /usr/local/bin/ldg
make install BINDIR=~/.local/bin  # install to a user-local bin directory
```

## Requirements

On the host:

- `docker` (and `docker compose` for `ldg up`) — for local use. With
  `--remote`, docker runs on the *remote* host instead; locally you only need
  `ssh` (see [Remote containers](#remote-containers)).
- `ldd` and `patchelf` (to resolve and re-link injected binaries)
- `oras` (only if your devcontainer uses `features`)
- `ssh` (only for `--remote`)
- `tmux` is optional: without it the session opens directly (or set
  `multiplexer = "none"` to say so explicitly)

`ldg doctor` checks all of this and says what each missing piece would break.

## Configuration

`lodge` reads `~/.config/lodge/config.toml`. If it doesn't exist, nothing is
injected (you still get a shell).

```toml
# Inject Neovim plus its runtime and plugins.
[[inject]]
name           = "nvim"
binary         = "/usr/local/bin/nvim"     # host binary to copy in
config         = "~/.config/nvim"          # host config dir/file to copy in
target_binary  = "/usr/local/bin/nvim"     # where the wrapper goes (optional)
target_config  = "/root/.config/nvim"      # where the config goes (optional)
skip_if_exists = false                     # re-copy on every injection (default: true)
copy_deps      = true                      # copy shared libs via ldd (default: true)

# Neovim needs its runtime at the compiled-in $VIMRUNTIME path.
[[inject]]
name          = "nvim-runtime"
config        = "/usr/local/share/nvim/runtime"
target_config = "/usr/local/share/nvim/runtime"

# Ship installed plugins so the container uses your exact host versions instead
# of re-cloning them at HEAD (which drifts into incompatible versions).
[[inject]]
name          = "nvim-plugins"
config        = "/home/me/.local/share/nvim/lazy"
target_config = "/root/.local/share/nvim/lazy"

[session]
shell       = "/bin/bash"   # shell launched inside the container (default)
multiplexer = "tmux"        # "tmux" or "none" (default: "tmux")

[git]
# Registered as git `safe.directory` in the container, so git-backed tools
# (lazy.nvim, gitsigns, …) don't trip over "detected dubious ownership" on the
# host-owned workspace/plugins. Default: ["*"]; set [] to disable.
safe_directories = ["*"]

[aliases]
# Written to /etc/profile.d/lodge.sh (login shells).
ll = "ls -la"
```

### `[[inject]]` fields

| field            | required | default                 | meaning                                                                       |
| ---------------- | -------- | ----------------------- | ----------------------------------------------------------------------------- |
| `name`           | yes      | —                       | identifier; used for default paths                                            |
| `binary`         | no       | —                       | host path to a binary to inject                                               |
| `config`         | no       | —                       | host path to a config file/dir to inject                                      |
| `target_binary`  | no       | `/usr/local/bin/<name>` | wrapper location in the container                                             |
| `target_config`  | no       | `/root/.config/<name>`  | config location in the container                                              |
| `skip_if_exists` | no       | `true`                  | skip if already present; `false` re-copies every run                          |
| `copy_deps`      | no       | `true`                  | copy shared-library dependencies via `ldd`                                    |

## Devcontainer support

`ldg up [PATH]` looks for the devcontainer config under the project path
(default: current directory), in spec order:

1. `PATH/.devcontainer/devcontainer.json`
2. `PATH/.devcontainer.json`
3. `PATH/.devcontainer/<folder>/devcontainer.json` (one level deep; with
   several, the alphabetically first wins and a warning lists the choice)

The config is JSONC and lodge supports a useful subset of the spec. All three
project forms work:

- **`dockerComposeFile`** + `service` — lodge drives `docker compose`, under a
  project name derived from the project path (`-p lodge-<name>-<hash>`), so
  two projects whose compose files both live in `.devcontainer/` don't collide
  on compose's directory-derived project name.
- **`image`** — lodge runs the image as a `sleep infinity` container with the
  workspace bind-mounted (honoring `workspaceFolder`/`workspaceMount`, `mounts`,
  and `runArgs`).
- **`build`** / **`dockerFile`** — lodge builds the image (honoring `context`,
  `dockerfile`, `target`, `args`) and runs it the same way.

On top of that:

- **Lifecycle hooks** — `initializeCommand` (host, before up), `onCreateCommand` /
  `updateContentCommand` / `postCreateCommand` (in-container, once at creation),
  `postStartCommand` (every up), and `postAttachCommand` (before the shell). All
  three forms work: string (`sh -c`), array (argv), and object (named commands).
  A failing hook is reported but never locks you out of the container.
- **Features** — `features` are pulled with `oras` and their `install.sh` runs in
  the container at create-time: options become env vars, `installsAfter` /
  `overrideFeatureInstallOrder` ordering is honored, and `containerEnv` is written
  to `/etc/profile.d` so login shells pick it up. lodge installs features *in the
  container* rather than baking them into the image, so create-flag fields
  (`mounts`, `privileged`, `capAdd`, `entrypoint`) can't be applied — they're
  warned and skipped.
- **Change detection** — if the container already exists, lodge compares the
  current `.devcontainer/` contents (canonicalized JSON, so comments and
  formatting don't count) against a hash recorded inside the container at
  creation. If they differ it offers to recreate; otherwise it reuses the
  container. `ldg up --build` forces a rebuild without prompting.
- **Port forwarding** — `forwardPorts` entries are forwarded eagerly, from the
  moment the container is up (connections just fail until the service starts
  listening). This works even for minimal images where lodge can't inspect
  listeners (no shell in the container). All entry forms are supported:
  - `3000` / `"3000"` — local port 3000 → port 3000 of the primary container.
  - `"db:5432"` — local port 5432 → port 5432 of host `db` on the container
    network (e.g. another compose service). Resolved via DNS inside the
    primary container, falling back to matching container names / compose
    naming on the container's networks; if the service isn't up yet, lodge
    keeps retrying and forwards once it appears.
  - `"3000:8080"` — local port 3000 → port 8080 of the primary container
    (docker `-p` style; a purely numeric prefix is read as a local port).

  Beyond that, lodge watches `/proc/net/tcp` inside the running container and
  dynamically forwards any port that starts listening to the same port on the
  host (loopback-only listeners are skipped — they're unreachable from outside
  the container). Forwarding uses a built-in TCP proxy locally or an `ssh -L`
  tunnel when operating with `--remote`.

## Remote containers

`ldg up -r <dest>` (or `--remote`) runs the docker CLI *on the remote host*
over SSH (`ssh <dest> docker …`), so **no local docker CLI is required** — only
an `ssh` client (and `tar`, for copying your local environment in). The
devcontainer config is read from the remote host, features are installed in the
remote container, and port forwarding tunnels ports back to your local machine.

The destination is anything your SSH client understands:

- `user@host` — explicit user and host.
- `myhost` — a `Host` alias from `~/.ssh/config`; its `User`, `Port`,
  `IdentityFile`, etc. all apply.
- `user@host:2222` — a non-standard SSH port (becomes `-p 2222`).

Because docker runs remotely, `docker`/`docker compose`/`docker build` must be
installed on the remote host (they don't need to exist locally). Your injected
environment — binaries, their libraries, and config — lives locally and is
streamed into the remote container over the SSH connection: directories are
`tar`'d and unpacked by the remote daemon, individual files are piped straight
in. That is why `tar` is needed locally for `--remote`.

Connections must be non-interactive (key/agent auth); if something fails, run
with `-v` to see each command and `-vv`/`-vvv` for ssh's own debug output.

## How it works

For each tool in your config, `lodge`:

1. Resolves the binary's shared-library dependencies on the **host** with `ldd`,
   including the dynamic linker (`ld-linux-*.so`).
2. Copies missing libraries into `/opt/lodge/lib/` inside the container, and the
   dynamic linker into `/lodge/`.
3. Uses `patchelf` to rewrite a copy of the binary so it runs **directly** against
   your injected glibc, then copies it to `/opt/lodge/bin/<name>`:
   - `--set-interpreter /lodge/<ld>` — your linker becomes the program interpreter.
   - `--force-rpath --set-rpath /opt/lodge/lib` — the search path is baked in as
     `DT_RPATH` (not `RUNPATH`), so it also resolves *transitive* deps.
4. Installs a thin wrapper at the target path that just `exec`s the patched binary.

Baking the search path into the binary (rather than exporting `LD_LIBRARY_PATH`)
gives two properties:

- **Subprocesses are unaffected.** Tools the binary spawns — `git`, shells,
  language servers — keep using the container's own glibc. An exported
  `LD_LIBRARY_PATH` would poison them (`undefined symbol … GLIBC_PRIVATE`).
- **`/proc/self/exe` stays correct.** Running directly (not via `ld.so <binary>`)
  keeps `/proc/self/exe` pointing at the binary. Tools that re-exec themselves
  need this — Neovim spawns its backend via `/proc/self/exe --embed`; under an
  `ld.so` wrapper that path would be the linker and the TUI wouldn't start.

Statically linked binaries are copied and run directly, with no patching. After
injecting, `lodge` opens a per-container `tmux` session; run from *inside* tmux it
`switch-client`s instead of nesting.

The setup steps that write to root-owned locations (`/opt/lodge`, `/lodge`,
`/etc/profile.d`, system wrappers) and feature install scripts run as root via
`docker exec -u 0`, so injecting works even on images whose default `USER` is
unprivileged. The interactive shell and devcontainer lifecycle hooks run as the
container's normal user, so your environment and `$HOME` are the user's own.

## Limitations

- An injected binary's libraries must all resolve on the host; anything `ldd`
  reports "not found" is skipped (with a warning) and may break the tool.
- Host and container must share an architecture — injected binaries, glibc, and
  compiled assets (treesitter parsers, Mason servers) are host binaries run
  inside the container.
- Host-built subprocesses the tool launches (e.g. a Mason language server) run
  against the *container's* glibc. Injection fixes the tool, not everything it
  shells out to — for LSPs, prefer letting the container provide the toolchain
  (via a feature, or in the image itself).
- A feature's `entrypoint` is ignored (lodge runs the container as `sleep
  infinity`); its `mounts`/`privileged`/`capAdd`/`securityOpt`/`init` *are*
  applied at creation.
- Dynamic port forwarding requires the container's bridge IP to be reachable from
  the host. This works on Linux with Docker Engine. Docker Desktop on macOS/Windows
  doesn't expose container IPs directly, so forwarding doesn't reach the
  container there; with `--remote` the `ssh -L` tunnel targets the IP from the
  remote host, which works.

## Roadmap

- Full round-based feature install ordering (lodge does a simple topo sort today).
- Support for other multiplexers.
- Support for other container runtimes.

## Development

```sh
cargo test                                  # unit tests (no Docker needed)
make integration                            # the real binary against docker (needs docker + patchelf; ~15 s)
cargo clippy --all-targets -- -D warnings   # lints
cargo fmt --check                           # formatting
cargo build --release
```

`make check` runs the same fmt, clippy and test gate as CI.

CI (`.github/workflows/ci.yml`) runs that gate plus a release build on every
push/PR. On `master`, a release job derives the next version from conventional
commits and `v*` tags with `git-cliff`, tags it, and attaches static musl
binaries (`ldg-<target>.tar.gz`) to a GitHub release for `install.sh` to fetch.

For a tour of the code (modules, the `ldg up` flow, what gets written where in a
container, how `--remote` works) see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).
