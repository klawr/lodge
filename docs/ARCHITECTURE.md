# Architecture

This is a map of how `ldg` works internally. For usage and configuration see the
[README](../README.md).

## What it does

`ldg` solves two related problems with one binary:

1. **Bring your tools into a container you don't control.** It copies your
   editor, shell utilities and their configs into a running container, together
   with the shared libraries they need, and re-links them so they run against
   *your* glibc rather than the container's (often much older) one.
2. **Run a project's devcontainer without VS Code.** It reads
   `.devcontainer/devcontainer.json`, brings the container up (compose, image or
   build), runs the spec's lifecycle hooks and features, then does step 1 on top.

Everything works against a local Docker daemon or, with `--remote`, a daemon on
another machine reached over SSH. Nothing beyond `ldg` itself is installed on
the host, and nothing is installed in the image.

## Commands and control flow

```
ldg up [PATH]      devcontainer::start → forward::spawn_daemon → inject::lodge
                   → Started::run_post_attach → container::enter
ldg exec <ctr>     inject::lodge → container::enter            (no devcontainer)
ldg down [PATH]    devcontainer::stop
ldg status [PATH]  devcontainer::status      (container state, config drift)
ldg rm [PATH]      devcontainer::remove      (container, and the compose project)
ldg doctor         doctor::run               (what is installed, what is missing)
ldg _forward …     forward::run_daemon        (internal; spawned by `up`)
```

`src/main.rs` is just this dispatch. `-r/--remote` and `-v/--verbose` are global
flags, available on every subcommand.

## Module map

| Module | Responsibility |
| --- | --- |
| `cli.rs` | `clap` definitions: subcommands and the global `--remote` / `--verbose`. |
| `doctor.rs` | `ldg doctor`: checks the engine, helper tools, config and the binaries it lists. |
| `elf.rs` | Reads a binary's CPU architecture from its ELF header, to refuse an injection for the wrong one. |
| `fingerprint.rs` | The small content hash used to notice changes (devcontainer inputs, updated libraries). |
| `terminfo.rs` | Makes the user's terminal work in the container: passes `TERM`, copies a missing terminfo entry. |
| `config.rs` | The user's `~/.config/lodge/config.toml`: `[[inject]]`, `[session]`, `[git]`, `[aliases]`. |
| `devcontainer/` | Everything about running a `devcontainer.json` (see below). |
| `inject/` | Copies binaries, libraries and configs into a container and wires them up: `binary`, `libs` (tracked by content hash), `config_files` (and ownership), `shell_env` (`PATH`, aliases, git, terminal), `state` (format version). |
| `deps.rs` | Resolves a binary's shared libraries and dynamic linker by parsing `ldd`. |
| `features.rs` | Devcontainer *features*: fetch via `oras`, order, apply creation flags, install. |
| `container.rs` | Opens the interactive session: `docker exec -it`, optionally inside tmux. |
| `forward.rs` | Port forwarding: the `_forward` daemon, in-process TCP proxy, `ssh -L` tunnels. |
| `target.rs` | `Target` (`Local` or `Ssh(host)`): the one place that decides local vs remote, plus host-side file helpers. |
| `runtime/` | The seam to whatever runs the workload: the `Env`, `Container` and `Runtime` traits (see below), the docker CLI implementation (`docker/`: the engine, the per-container `Env`, and the file-copy streaming), and a strict in-memory fake for tests. |
| `ssh.rs` | `--remote` destination parsing, connection multiplexing, small SSH helpers. |
| `shell.rs` | POSIX `shell_quote`. Everything that builds a command string goes through it. |
| `tmpfile.rs` | Self-deleting staging files for `docker cp`. |
| `output.rs` | Everything lodge says (`info!`, `warning!`, `note!`), the `[lodge]` prefix on every line, and the verbosity setting with command tracing. |

### `devcontainer/`

`start()` in `mod.rs` is the orchestration; each step is its own module.

| File | Responsibility |
| --- | --- |
| `mod.rs` | `start` / `stop`, the `Started` result, the recreate prompt. |
| `config.rs` | Parse `devcontainer.json` (JSONC) into a backend-agnostic `Spec`; find the config; substitute `${localEnv:…}` and friends; derive stable container and session names. |
| `backend.rs` | The `Backend` trait with two implementations: `ComposeBackend` (`docker compose`) and `ContainerBackend` (`docker run` of an image or a local build). |
| `lifecycle.rs` | The hook types (string / argv / named map), running them on the host or in the container, and the env profile written into the container. |
| `hash.rs` | Drift detection: fingerprint `.devcontainer/` and compare with the value stored in the container. |
| `workdir.rs` | Pick a fallback directory when `workspaceFolder` doesn't exist. |
| `uid.rs` | Lines the container user's uid/gid up with the host user's so a bind-mounted workspace is writable. Planned in Rust from `/etc/passwd`, applied by a small script. |
| `vars.rs` | `${localEnv:X:default}`, `${containerWorkspaceFolder}`, `${devcontainerId}` and friends. |
| `remote.rs` | The `--remote` versions of "find the config" and "hash the inputs", which must run on the remote host. |

The `Backend` split is what keeps `start()` simple: a backend only knows how to
find an existing container and how to create or start one. Hashing, env,
features and hooks all work from the resulting container id.

## What `ldg up` does, step by step

1. **Load the spec.** Find and parse `devcontainer.json`; pick a backend.
2. **Detect drift.** Fingerprint the inputs and compare with the hash stored in
   the existing container. On a mismatch, ask whether to recreate it (without a
   TTY, keep the container). `--build` forces a recreate.
3. **`initializeCommand`** runs on the host (the remote host with `--remote`).
4. **Resolve features** when (re)creating, so their container flags (mounts,
   `privileged`, `capAdd`, …) can be applied at creation.
5. **Bring the container up** via the backend, then record the new hash.
6. **Align the container user's uid** with the host user's (image/Dockerfile backend, non-root user, when the container was just created).
7. **Write the env profile** (`containerEnv` + `remoteEnv`) into the container.
8. **Install features**, then the create-time hooks `onCreateCommand`,
   `updateContentCommand`, `postCreateCommand`. Each hook runs once
   *successfully*: a success marker is written only if it exits 0, so a failed
   hook retries on the next `up`. `postStartCommand` runs every time. A failing
   hook is reported but never aborts `ldg`.
9. **Choose a working directory.** `workspaceFolder` if it exists, otherwise ask.
10. **Start the port-forward daemon**, then **inject your environment**.
11. **`postAttachCommand`**, then **open the session**.

## Injection

For each `[[inject]]` entry in the config (`inject/`):

- **Dynamic binaries.** `deps.rs` runs `ldd` on the host binary. Missing
  libraries are copied to `/opt/lodge/lib/` and the dynamic linker to
  `/lodge/<ld>`. The binary is then *copied and patched with `patchelf`*:
  `--set-interpreter /lodge/<ld>` and `--force-rpath --set-rpath /opt/lodge/lib`.
  The patched copy lives at `/opt/lodge/bin/<name>`, so it runs directly,
  without an `ld.so` indirection, and `/proc/self/exe` stays correct for tools
  that re-exec themselves. A two-line wrapper at the target path
  (`/usr/local/bin/<name>`) just `exec`s it.
- **Static binaries** are copied as they are.
- **Configs** are copied to their target, and ownership under the user's home is
  fixed up to the container user (never on a host bind mount, where it would
  change the host's files).
- **Home directory.** The user's home is made writable first, since injected
  configs and tool caches live there.
- **Shell setup.** The container's `PATH` is preserved across login shells
  (`/etc/profile.d/lodge-path.sh`), aliases go to `/etc/profile.d/lodge.sh`, and
  git `safe.directory` is set so tools can run `git` on host-owned files.

`skip_if_exists` (default `true`) makes re-runs cheap; set it to `false` for
things that change, such as an editor config.

## What lives where

**Inside the container**

| Path | Contents |
| --- | --- |
| `/opt/lodge/lib/`, `/lodge/<ld>` | Copied libraries and the dynamic linker. |
| `/opt/lodge/bin/` | Patched binaries. |
| `/opt/lodge/state/` | One-shot markers (`onCreate`, `postCreate`, feature installs). |
| `/opt/lodge/devcontainer.hash` | Fingerprint of the devcontainer inputs. |
| `/opt/lodge/version` | Format version of this layout, so a future lodge can migrate it. |
| `/opt/lodge/lib/.manifest` | Hash of every library copied, so a changed host library is refreshed (names don't change across glibc versions). |
| `/opt/lodge/terminfo/` | The user's terminfo entry, when the container lacked it. |
| `/etc/profile.d/lodge{,-env,-path,-features}.sh` | Aliases, `containerEnv`/`remoteEnv`, `PATH`, feature env. |

Because all state is *in* the container, it survives reboots and disappears
when the container is recreated, with no host bookkeeping to go stale.

**On the host**

| Path | Contents |
| --- | --- |
| `~/.config/lodge/config.toml` | Your configuration. |
| `~/.local/share/lodge/forward-<id>.log`, `.lock` | Log of the port-forward daemon, and the lock that keeps it to one per container. |
| `$TMPDIR/lodge-ssh/*.sock` | SSH `ControlMaster` sockets (`--remote` only). |
| `$TMPDIR/lodge-<pid>-*` | Short-lived staging files, removed when done. |

## Remote mode

With `--remote <dest>`, the docker CLI runs *on the remote host* over SSH, so no
local Docker is needed. Three things make that work:

- `DockerCli::command` wraps every `docker …` in `ssh <dest> …`, shell-quoting each
  argument. Anything that reads **files** (compose projects, build contexts)
  goes through `DockerCli::command_in`, which also `cd`s into the project
  directory *on the remote*.
- **Connection reuse.** A single `ldg up` issues dozens of SSH calls.
  `ssh::base_command` multiplexes them over one `ControlMaster` socket per
  destination; without that, each call renegotiates and a flaky link silently
  breaks random steps.
- **Local bytes → remote container.** `docker cp` can't see local paths on the
  remote, so `cp_in` streams a tar over SSH (packed and unpacked in-process, so no local `tar` is needed). Reading the devcontainer config
  and hashing its inputs are likewise done by shell scripts run remotely
  (`devcontainer/remote.rs`).

The destination accepts a `~/.ssh/config` alias, `user@host`, `user@host:port`,
`[v6]:port` and an `ssh://` prefix (`ssh::split`).

## Runtime traits

`runtime/` separates *what lodge needs* from *how docker provides it*, in three
small traits from narrow to broad:

| Trait | A… | Used by |
|---|---|---|
| `Env` | place you can run commands in and copy files into | injection, features, lifecycle hooks, the stored config hash |
| `Container: Env` | `Env` that can also be inspected (name, mounts, address, peers) | port forwarding, workdir selection, the session |
| `Runtime` | container engine: find, build, run, start, stop, compose | devcontainer backends, `start`/`stop` |

`Env` carries the primitives (`exec`, `output`, `cp_in`, `cp_out`) and provides
everything else on top of them: `write_file`, `mkdir_p`, `file_exists`,
`task_done`, and so on. So code that injects a tool takes one `&dyn Env` instead
of a `(docker, container)` pair, and anything new that implements those four
primitives gets the rest for free.

`runtime::docker` implements all three over the `docker` CLI, locally or on a
remote host over SSH. `runtime::fake::FakeEnv` is an in-memory `Env` that
records commands and keeps a virtual filesystem, which is what the injection
tests run against. Another engine, or a plain host with no engine at all, would
implement the same traits.

Backends (`devcontainer/backend.rs`) are plain data: the `Runtime` is passed in
rather than stored.

## Local vs remote

`--remote` is parsed once, in `main.rs`, into a `Target` (`Local` or
`Ssh(host)`), which the `Runtime` is built from. From there on nothing handles a
bare `Option<String>`. Code that must behave differently does a single `match` on
the `Target` at the boundary (reading the config, hashing it, running
`initializeCommand`) and is otherwise written once. `Target` also owns host-side
file operations (`write_file`, `remove_file`, `scratch_path`), which the compose
backend uses for its temporary override file.

## Conventions

Most of these are enforced by the compiler or a test, so they can't quietly
erode.

- **Output.** Everything goes through `output`: `info!` for progress, `warning!`
  for something that went wrong but lodge carries on, `note!` for hints. Every
  line gets the `[lodge]` prefix, including continuation lines of a multi-line
  message, so call sites write plain text. `--verbose` also traces every command
  lodge spawns, in shell-pasteable form; `-vv` / `-vvv` pass `-v` to ssh.
  *Enforced:* clippy denies `println!` and friends outside `output.rs`.
- **One place per external program.** `ssh` is started only in `ssh.rs`,
  `docker` only in `runtime/docker/`, and a local `sh` only in `target.rs` (scripts
  on the project's host) and `container.rs` (launching the user's session).
  `ssh.rs` offers `command`, `sh_output`, `write_file`, `tunnel`, `pty_denied`
  and `interactive_line`; nothing else builds an `ssh` command. *Enforced:* the
  tests in `conventions.rs`.
- **Running commands.** In a container through an `Env`; engine operations
  through a `Runtime`; on the project's host through `Target` (`sh_output`,
  `run_in`, `write_file`, `remove_file`, `uid_gid`).
- **Quoting.** Anything interpolated into `sh -c` or an SSH command string goes
  through `shell::shell_quote`. Don't build such strings by hand.
- **Writing files into a container.** Use `Env::write_file` (creates parent
  directories, works over `--remote`).
- **No panics for errors.** Non-test code returns `Result`; it does not
  `unwrap()` or `expect()`. *Enforced:* clippy denies both.
- **Ignoring a `Result` is a decision.** Either handle it, or say why in an
  `#[allow(clippy::let_underscore_must_use, reason = "…")]`. Scratch-file cleanup
  goes through `tmpfile::discard_file` / `discard_dir`. A failure that matters
  later (starting a container, recording a marker, starting the forwarder) is
  reported, not swallowed. *Enforced:* clippy denies an unexplained `let _ =`.
- **Failures.** Hooks and feature installs warn and continue; structural
  problems (no config, container won't start) abort with an error.

## Testing

`cargo test` runs the unit tests and needs no Docker. They cover the pure parts
(config parsing and building a `Spec`, ldd parsing, shell quoting, SSH
destination parsing, port-forward parsing, hash normalisation, feature ordering,
the uid plan, library refresh) and, through `FakeEnv`, the logic that writes into
a container: aliases, `PATH` preservation, the wrapper script, the env profile,
the stored hash and the hook markers. `conventions.rs` checks the architecture
rules above.

`FakeEnv` is strict on purpose. It emulates a handful of commands, and a test
declares anything else it depends on (`with_output`, `with_failure`,
`with_commands`). A command it neither emulates nor was told about panics, so a
test can't pass because the fake said "success" to something a real container
might refuse. Tests that need scratch files use `tmpfile::TempDir`, which removes
itself even when the test fails.

**Integration tests** (`tests/integration/`) drive the real `ldg` binary against a
real container engine: create and reuse, hooks, drift, injection and library
refresh, the architecture refusal, uid alignment, container settings and
variables, compose, forwarding (loopback-only, one daemon) and `--remote`. They
are `#[ignore]`d, so `cargo test` stays Docker-free; run them with
`make integration` (about 15 s; quiet on success, and a failure shows the tail of
the command that failed). Narrow with `make integration T=uid`. They need docker
with compose, and `patchelf`.

Each test gets a sandbox: its own config, data and temp directories, and, for
`--remote`, a fake `ssh` on `PATH` that skips its options and runs the remote
command locally, so no second machine is needed. A project removes its container
when dropped, even if the test failed, and the sandbox waits for its forward
daemons before deleting itself. `ldg up --no-attach` and `ldg exec --no-attach`
do all the work and exit without opening a shell, which is what makes the exit
codes meaningful. What these cannot cover is a real remote host, a different
container OS (musl/Alpine), and a real terminal.

CI (`.github/workflows/ci.yml`) runs fmt, clippy `-D warnings`, the unit tests
and a release build in one job, and the integration tests in another. On `master`, a release job derives the next version from
conventional commits and `v*` tags with `git-cliff`, tags it, and uploads static
musl binaries as `ldg-<target>.tar.gz`, which `install.sh` downloads.
`make check` runs the same gate locally.
