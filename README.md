# lodge

**Inject your environment into any container.**

`lodge` (command: `ldg`) copies your personal tools (editor, shell utilities, AI
CLIs, …), together with the shared libraries they need, into a running
container and drops you into a shell inside it. You get your toolchain in
someone else's image without rebuilding it, and **without** requiring your
tools to be compatible with the container's (often much older) glibc.

It also doubles as a lightweight [devcontainer](https://containers.dev) runner:
`ldg up` starts a project's `.devcontainer`, honors its lifecycle hooks and
features, then injects your environment on top. No VS Code, no daemon, nothing
added to the image.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/klawr/lodge/master/install.sh | sh
```

Downloads the latest static Linux binary (x86_64 or aarch64) into
`~/.local/bin`. Set `LODGE_BIN_DIR` to install elsewhere, or `LODGE_VERSION`
(e.g. `v0.4.2`) to pin a version.

From source: `cargo install --git https://github.com/klawr/lodge`, or
`make install` in a clone (`PREFIX=~/.local/bin` for a user install).

## Quickstart

```sh
ldg init          # write ~/.config/lodge/config.toml, with the tools found here filled in
$EDITOR ~/.config/lodge/config.toml   # uncomment the tools you want
ldg doctor        # check the host and the config
cd my-project     # a project with a .devcontainer
ldg up            # start it, inject your tools, open a shell
```

Inside, your editor and tools run as if they were installed in the image. Leave
the shell and come back with `ldg up` again; the container, and everything
installed in it, is kept until you `ldg rm` it.

No devcontainer? `ldg exec <container>` injects into any running container.

Shell completions: `ldg completions bash|zsh|fish` (see `ldg completions --help`
for where to put them). Man page: `ldg man | man -l -`.

## Commands

| Command | What it does |
| --- | --- |
| `ldg up [PATH]` | Start the project's devcontainer, inject your environment, open a shell. `--build` rebuilds; `--yes` / `--no-recreate` answer the "config changed" question; `--no-attach` sets up without a shell. |
| `ldg exec CONTAINER` | Inject into an already-running container and open a shell (`-u USER`, `-w DIR`). |
| `ldg down [PATH]` | Stop the project's container (resume later with `ldg up`). |
| `ldg rm [PATH]` | Remove the project's container (the next `ldg up` starts fresh). |
| `ldg status [PATH]` | The container's state, whether its config changed, the injected bundle, placed secrets. |
| `ldg ls` | Every container lodge created, across projects. |
| `ldg ports [PATH]` | The ports forwarded to this machine for the project's container. |
| `ldg logs [PATH]` | The port-forwarding log (`-f` to follow). |
| `ldg clean [PATH]` | Remove what lodge injected from the container (configs are kept and listed). |
| `ldg init` | Write a starting config (`--print` to see it first). |
| `ldg doctor [PATH]` | Check the host, the config, and what the project's devcontainer needs. |
| `ldg pack` | Pack your tools and configs into a portable bundle (`lodge-<arch>.tar`). |
| `ldg bundle inspect\|verify FILE` | Show what a bundle holds; check it is intact. |
| `ldg cache [clear]` | Show or clear the cache of prepared bundles. |
| `ldg completions SHELL`, `ldg man` | Shell completions and the man page. |

Global flags, on every command: `-r/--remote HOST` runs against a docker engine
over SSH, `-c/--config FILE` uses another config file, and `-v/--verbose` prints
every docker/ssh command lodge runs (`-vv`, `-vvv` add ssh's own debug output).
`status`, `ls`, `ports` and `doctor` take `--json`.

## Requirements

On the host:

- `docker` with `docker compose` (for compose-based devcontainers). With
  `--remote`, docker runs on the *remote* host and locally you only need `ssh`.
- `ldd` and `patchelf`, **only to pack** your tools (that is, when your config
  lists `[[inject]]` entries). Injecting from a bundle needs neither.
- `oras`, only for devcontainer features from a registry.
- `tmux` is optional: without it the session opens directly.

`ldg doctor` checks all of this, and says what each missing piece would break.

## Documentation

- [Configuration](docs/configuration.md): `config.toml`, every field, and the
  JSON schema for editor completion.
- [Devcontainers](docs/devcontainers.md): which parts of the spec lodge runs,
  features, lifecycle hooks, change detection, port forwarding.
- [Remote containers](docs/remote.md): `--remote` over SSH.
- [Bundles and secrets](docs/bundles-and-secrets.md): carrying your setup to
  other machines, and delivering API keys and logins without putting them in
  a bundle.
- [Troubleshooting](docs/troubleshooting.md): common problems and fixes.
- [Architecture](docs/architecture.md): how the code is organised.

## How it works, briefly

When packing, lodge resolves each binary's shared libraries with `ldd` and
rewrites a copy with `patchelf` so it runs against a **bundled** glibc: the
bundled dynamic linker becomes its interpreter and `/opt/lodge/lib` its
`DT_RPATH`. Injecting copies that bundle into the container (`/opt/lodge`,
`/lodge`) and puts a thin wrapper at the tool's usual path. Because the search
path is baked into the binary rather than exported as `LD_LIBRARY_PATH`, the
programs your tool starts (git, shells, language servers) keep using the
container's own glibc, and `/proc/self/exe` stays correct for tools that
re-exec themselves. Statically linked binaries are copied as they are.

## Limitations

- Host and container must share a CPU architecture: injected binaries and
  compiled assets (treesitter parsers, Mason servers) are host binaries.
- Programs an injected tool starts but that weren't injected themselves (say,
  a Mason language server) run against the container's glibc. For language
  servers, prefer letting the container provide the toolchain.
- Libraries loaded at run time (`dlopen`, interpreters) are invisible to `ldd`;
  list them as `[[inject]]` entries.
- Dynamic port forwarding needs the container's bridge IP to be reachable from
  the engine's host: Linux with Docker Engine, or any engine with `--remote`.
  Docker Desktop on macOS/Windows doesn't expose container IPs.

## Roadmap

- Podman as a second container runtime, then plain hosts with no runtime.
- Devcontainer features without `oras` (a built-in OCI pull), and full
  round-based install ordering (lodge does a simple topological sort today).
- Change detection that ignores files generated by hooks.
- Other multiplexers.

## Development

```sh
make            # list the targets
make check      # fmt, clippy -D warnings, unit tests (what CI gates on)
make integration  # the real binary against docker (~30 s; needs docker, patchelf, tmux)
make dev        # open this repo's own devcontainer with the ldg built from this tree
```

The repo's devcontainer has docker (through the host's socket) and patchelf, so
`make integration` runs inside it too. CI (`.github/workflows/ci.yml`) runs the
gate, the integration tests, the MSRV build, `cargo deny` and the static musl
builds on every push and PR. Releases come from conventional
commits through [release-please](https://github.com/googleapis/release-please):
on `master` it keeps a release PR open with the next version, and merging it
tags the release, attaches the binaries (`ldg-<target>.tar.gz`) to a draft and
publishes it once they are all there.
