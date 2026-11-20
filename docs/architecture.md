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
ldg up [PATH]        devcontainer::start → ContainerUser::resolve → forward::spawn_daemon
                     → inject::run → Started::run_post_attach → secret::deliver → session::enter
ldg exec <ctr>       ContainerUser::resolve → inject::run → secret::deliver → session::enter
                     (no devcontainer)
ldg down [PATH]      devcontainer::stop
ldg status [PATH]    devcontainer::status (state, drift) + installed bundle + placed secrets
ldg rm [PATH]        devcontainer::remove (container, and the compose project)
ldg ls               Runtime::list_labeled (every container with lodge's project label)
ldg ports [PATH]     forward::active_ports (what the daemon reports)
ldg logs [PATH]      the daemon's log file
ldg clean [PATH]     inject::remove
ldg init             a commented config from the tools found on PATH
ldg doctor [PATH]    host tools, engine, compose, config, binaries, secrets
ldg pack             bundle::pack::pack_to_file (config → bundle tar)
ldg bundle inspect|verify FILE   bundle::inspect / bundle::verify_file
ldg cache [clear]    bundle::cache::entries / clear
ldg completions, ldg man         generated from the clap definition
ldg _forward …       forward::run_daemon (internal; spawned by `up`)
```

`src/main.rs` only parses the arguments and hands them to `cmd::run`. `cmd/` is
the command layer: it builds a `Ctx` (the `Target`, the `Runtime` and the config
path), loads the config **once** per command, resolves the container user once
(`ContainerUser`), and decides what to print (`--json` included) and how to ask
the person at the terminal (`cmd::prompt`). The modules below it take what they
need as arguments, don't read the config themselves, report progress with
`info!`/`warning!`, and return data rather than printing reports.
`-r/--remote`, `-c/--config` and `-v/--verbose` are global flags, available on
every subcommand.

## Module map

| Module | Responsibility |
| --- | --- |
| `cli.rs` | `clap` definitions: subcommands and the global flags. |
| `cmd/` | One file per group of commands (`attach`: up/exec, `project`: down/status/rm/ls/ports/logs/clean, `doctor`, `init`, `bundle`: pack/bundle/cache), plus `prompt`, the terminal side of `devcontainer::Prompt`. The only layer that loads the config, prints reports and reads the terminal. |
| `config.rs` | The user's `config.toml`: `[[inject]]`, `[session]`, `[git]`, `[aliases]`, `[[secret]]`, `bundle`. Unknown keys are errors (with a "did you mean"), and `validate` reports every problem at once. A relative `bundle =` resolves against the file's folder. The JSON schema in `docs/config.schema.json` is generated from these types. |
| `devcontainer/` | Everything about running a `devcontainer.json` (see below). |
| `bundle/` | A bundle is a portable tar of relinked binaries, their libraries and configs, with a manifest. `manifest` defines and validates it (paths derive from names, so a bundle can't point outside itself), `archive` writes, reads and verifies the tar, `pack` builds one from the config (the only step needing `patchelf`; `deps` parses `ldd`, `elf` reads a binary's architecture), `source` readies a bundle *directory* to inject from (extracted from a file, or packed live), keyed by the file's hash or a fingerprint of the live inputs, and `cache` keeps those directories under `~/.cache/lodge/bundles/`. `hash` is the content hash of a file or tree. |
| `inject/` | Copies a bundle's binaries, libraries and configs into a container and wires them up for the `ContainerUser`, reading only from the bundle directory: `binary`, `libs` (tracked by content hash), `config_files` (and ownership), `installed` (a hash and the destination per installed piece), `shell_env` (`PATH`, aliases, git, terminal), `state` (format version, installed bundle id), `remove` (`ldg clean`). |
| `layout.rs` | Every path lodge writes inside a container (the table under "What lives where"), and `INJECTED`, the part `ldg clean` removes. Nothing else spells these paths out. |
| `secret/` | Secrets that must not be in a bundle. `mod` is the `[[secret]]` declaration (validated; names reach shell scripts), the `Secret` value type (redacted `Debug`, no `Display`), `targets` globs, and `resolve` (env, file or a command run on *this* machine with a timeout, with errors that never contain the value). `place` writes values into a container under `/dev/shm/lodge-secrets-<uid>/` and builds the session lines that export them. `deliver` picks the secrets whose `targets` match, resolves and places them (or clears them when none are declared), and prints names only. |
| `session/` | Opens the interactive session: `docker exec -it`, optionally inside tmux. `terminfo` makes the user's terminal work in the container (passes `TERM`, copies a missing terminfo entry). |
| `forward/` | Port forwarding. `mod` spawns the daemon and owns its state files (log, lock, and the ports file `ldg ports` reads, which starts with the daemon's pid so a dead daemon's file is ignored), `daemon` is the `_forward` loop (declared ports plus whatever starts listening), `tunnel` one forward: the in-process TCP proxy or an `ssh -L` tunnel. |
| `host_tools.rs` | The helper programs lodge may run here (`ssh`, `ldd`, `patchelf`, `oras`, `tmux`): what each is for and how to get it. `doctor` reports from this table and each tool is started only through it. |
| `target.rs` | `Target` (`Local` or `Ssh(host)`): the one place that decides local vs remote, plus host-side file helpers (`fetch_dir` copies a folder from the project's host). |
| `runtime/` | The seam to whatever runs the workload: the `Env`, `Container` and `Runtime` traits (see below), `User` (who a command runs as) and `ContainerUser` (the session user's name, uid, gid and home, looked up once), the docker CLI implementation (`docker/`: the engine, the per-container `Env`, and the file-copy streaming), and a strict in-memory fake for tests. |
| `ssh.rs` | `--remote` destination parsing, connection multiplexing, small SSH helpers. |
| `shell.rs` | POSIX `shell_quote`, which everything that builds a command string goes through, and the rules for names written into scripts unquoted (`is_plain_name`, `is_alias_name`). |
| `fingerprint.rs` | The small content hash used to notice changes (devcontainer inputs, updated libraries). |
| `tmpfile.rs` | Self-deleting staging files for `docker cp`. |
| `output.rs` | Everything lodge says (`info!`, `warning!`, `note!`, `line!` for reports), the `[lodge]` prefix on every line, and the verbosity setting with command tracing. |
| `conventions.rs`, `doc_facts.rs` | Test-only: architecture rules, and facts the docs state checked against the code. |

### `devcontainer/`

`start()` in `mod.rs` is the orchestration; each step is its own module.

| File | Responsibility |
| --- | --- |
| `mod.rs` | `start` / `stop` / `remove` / `status` / `find`, the `Started` result, the recreate policy (`--yes`, `--no-recreate`, or ask), and `Prompt`, the questions only a person can answer (the command layer implements it). |
| `config.rs` | Parse `devcontainer.json` (JSONC) into a backend-agnostic `Spec`; find the config; substitute `${localEnv:…}` and friends; derive stable container and session names. |
| `backend.rs` | The `Backend` trait with two implementations: `ComposeBackend` (`docker compose`) and `ContainerBackend` (`docker run` of an image or a local build). Both label the container `dev.lodge.project=<path>`. `CreateSettings` holds how a container is created (privileged, capabilities, mounts, …), from devcontainer.json and from features alike, rendered as `docker run` flags or a compose override. |
| `features/` | Devcontainer *features*: `fetch` (OCI with `oras`, or a local folder from the project's host), and in `mod.rs` the order, the creation settings they ask for, and the install (as root, told `_REMOTE_USER`). |
| `lifecycle.rs` | The hook types (string / argv / named map, all run as the same steps), running them on the host or in the container (as `remoteUser`), and the env profiles written into the container. |
| `markers.rs` | One-shot markers: which create-time hooks and feature installs already succeeded. |
| `hash.rs` | Drift detection: fingerprint `.devcontainer/` (plus compose files kept outside it) and compare with the value stored in the container. The files are read through `Target` (`fetch_dir`, `read_file`), so a remote project is hashed exactly like a local one. A stored hash that can't be read is not drift. |
| `workdir.rs` | Offer fallback directories when `workspaceFolder` doesn't exist. |
| `uid.rs` | Lines the container user's uid/gid up with the host user's so a bind-mounted workspace is writable. Planned in Rust from `/etc/passwd`, applied by a small script. |
| `vars.rs` | `${localEnv:X:default}`, `${containerWorkspaceFolder}`, `${devcontainerId}` and friends. |
| `remote.rs` | The `--remote` version of "find the config": one probe script on the remote host, with the same precedence as the local lookup. (Hashing reads the files through `Target`, so local and remote use the same algorithm.) |

The `Backend` split is what keeps `start()` simple: a backend only knows how to
find an existing container and how to create or start one. Hashing, env,
features and hooks all work from the resulting container id.

## What `ldg up` does, step by step

0. **Load the config and ready the bundle** (`cmd::attach`), so a broken config or
   bundle fails before anything starts. The project path is made canonical.
1. **Load the spec.** Find and parse `devcontainer.json`; pick a backend.
2. **Detect drift.** Fingerprint the inputs and compare with the hash stored in
   the existing container. On a mismatch, recreate per `--yes` / `--no-recreate`,
   or ask (without a TTY, keep the container). If the stored hash can't be read,
   the container is kept. `--build` forces a recreate.
3. **`initializeCommand`** runs on the host (the remote host with `--remote`).
   Unlike the other hooks, a failure stops `ldg up`, as the spec asks.
4. **Resolve features** when (re)creating, so their container flags (mounts,
   `privileged`, `capAdd`, …) can be applied at creation.
5. **Bring the container up** via the backend, then record the new hash.
6. **Align the container user's uid** with the host user's (image/Dockerfile backend, non-root user, when the container was just created).
7. **Write the env profile** (`containerEnv` + `remoteEnv`) into the container.
8. **Install features**, then the create-time hooks `onCreateCommand`,
   `updateContentCommand`, `postCreateCommand`. Each hook runs once
   *successfully*: a success marker is written only if it exits 0, so a failed
   hook retries on the next `up`. On a reused container only the features that
   haven't succeeded yet are fetched and installed. `postStartCommand` runs every
   time. Hooks run as `remoteUser`. A failing hook or feature is reported but
   never aborts `ldg`.
9. **Choose a working directory.** `workspaceFolder` if it exists, otherwise ask.
10. **Look up the container user** (`remoteUser`, or the default user: name,
    uid, gid, home) once, then **start the port-forward daemon** and **inject
    your environment** for that user.
11. **`postAttachCommand`**, then **deliver secrets** and **open the session**.

## Injection

Injection reads only from a **bundle directory** (`bundle/source`), never from the
host's own files. The directory comes from a bundle file (`--bundle`, or `bundle =`
in the config), extracted once and cached by the file's hash, or from a live pack of
the `[[inject]]` entries, cached by a fingerprint of the config and of each source
file's size and mtime and the libraries `ldd` finds. Working out that fingerprint
runs `ldd` (so a live config needs it on every `up`); if nothing changed, the
cached directory is reused and nothing is copied or relinked (`patchelf` does
not run). With a bundle file neither tool is needed. The live path and the
bundle-file path end in the same place, so there is one injector. It is prepared
*before* anything starts, so a problem with it fails without leaving a half-started
container.

For each tool in the bundle (`inject/`):

- **Libraries first.** Copied to `/opt/lodge/lib/` when their hash differs from
  `/opt/lodge/lib/.manifest`, and the dynamic linker to `/lodge/<ld>` whenever it
  changed or is missing, so a linker and libc never come from different versions.
- **Binaries.** The relinked copy from the bundle goes to `/opt/lodge/bin/<name>`,
  so it runs directly, without an `ld.so` indirection, and `/proc/self/exe` stays
  correct for tools that re-exec themselves. A two-line wrapper at the target
  path (`/usr/local/bin/<name>`) just `exec`s it. A static binary is simply
  copied (relinking happens at pack time, where `patchelf` runs).
- **Skipping.** A hash is recorded per installed piece (`/opt/lodge/installed/`):
  unchanged is a no-op, changed is replaced, and `skip_if_exists = true` keeps a
  piece that is already there even when the bundle changed.
- **Configs** are copied to their target, and ownership under the user's home is
  fixed up to the container user (never on a host bind mount, where it would
  change the host's files).
- **Home directory.** The user's home is made writable first, since injected
  configs and tool caches live there.
- **Shell setup.** The container's `PATH` is preserved across login shells
  (`/etc/profile.d/lodge-path.sh`), aliases go to `/etc/profile.d/lodge.sh`, and
  git `safe.directory` is set so tools can run `git` on host-owned files.

`skip_if_exists` defaults to `false`: a piece is replaced when the bundle changed
and left alone when it did not. Set it to `true` for a tree the user updates
inside the container (installed plugins, for example).

## What lives where

**Inside the container** (each path is a constant in `layout.rs`)

| Path | Contents |
| --- | --- |
| `/opt/lodge/lib/`, `/lodge/<ld>` | Copied libraries and the dynamic linker. |
| `/opt/lodge/bin/` | Relinked binaries. |
| `/opt/lodge/installed/` | A content hash and the destination path per installed binary and config, so an unchanged piece is skipped, a changed one replaced (`skip_if_exists` protects a piece from replacement), and `ldg clean` knows what to remove. |
| `/opt/lodge/bundle` | The id of the bundle last injected; `ldg status` shows it. |
| `/opt/lodge/state/` | One-shot markers (`onCreate`, `postCreate`, feature installs). |
| `/opt/lodge/devcontainer.hash` | Fingerprint of the devcontainer inputs. |
| `/opt/lodge/version` | Format version of this layout, so a future lodge can migrate it. |
| `/opt/lodge/lib/.manifest` | Hash of every library copied, so a changed host library is refreshed (names don't change across glibc versions). |
| `/dev/shm/lodge-secrets-<uid>/` | Placed secret values (memory only, 0700 directory, 0600 files); see `secret/`. |
| `/opt/lodge/terminfo/` | The user's terminfo entry, when the container lacked it. |
| `/etc/profile.d/lodge{,-env,-path,-features}.sh` | Aliases, `containerEnv`/`remoteEnv`, `PATH`, feature env. |

Because all state is *in* the container, it survives reboots and disappears
when the container is recreated, with no host bookkeeping to go stale. Every
container lodge creates also carries the label `dev.lodge.project=<path>`.
`ldg clean` removes the injection (binaries, libraries, wrappers, shell setup,
secrets) and keeps the devcontainer's state (markers, hash, `containerEnv`).

**On the host**

| Path | Contents |
| --- | --- |
| `~/.config/lodge/config.toml` | Your configuration. |
| `~/.cache/lodge/bundles/` | Extracted or packed bundles, the four most recently used. |
| `~/.local/share/lodge/forward-<id>.log`, `.lock`, `.ports` | Log of the port-forward daemon, the lock that keeps it to one per container, and its active forwards (for `ldg ports`). |
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
  remote, so `cp_in` streams a tar over SSH (packed in-process, so no local
  `tar` is needed). Finding the devcontainer config is one probe script run
  remotely (`devcontainer/remote.rs`); its inputs are fetched for hashing with
  `Target::fetch_dir` and `read_file`.

The destination accepts a `~/.ssh/config` alias, `user@host`, `user@host:port`,
`[v6]:port` and an `ssh://` prefix (`ssh::split`).

## Runtime traits

`runtime/` separates *what lodge needs* from *how docker provides it*, in three
small traits from narrow to broad:

| Trait | A… | Used by |
|---|---|---|
| `Env` | place you can run commands in and copy files into | injection, features, lifecycle hooks, markers, the stored config hash |
| `Container: Env` | `Env` that can also be inspected (name, mounts, address, peers) | port forwarding, workdir selection, the session |
| `Runtime` | container engine: find, build, run, start, stop, compose | devcontainer backends, `start`/`stop` |

`Env` carries the primitives (`exec`, `output`, `cp_in`, plus `write_secret` and
`interactive_command`, which need engine-specific stdin and TTY handling) and
provides everything else on top of them: `write_file`, `read_file`, `mkdir_p`,
`file_exists`, and so on. So code that injects a tool takes one `&dyn Env`
instead of a `(docker, container)` pair, and anything new that implements the
primitives gets the rest for free. The traits know nothing of lodge's own
layout: markers, hashes and profiles live in the modules that use them.

`runtime::docker` implements all three over the `docker` CLI, locally or on a
remote host over SSH. `runtime::fake::FakeEnv` is an in-memory `Env` that
records commands and keeps a virtual filesystem, which is what the injection
tests run against. Another engine, or a plain host with no engine at all, would
implement the same traits.

Backends (`devcontainer/backend.rs`) are plain data: the `Runtime` is passed in
rather than stored.

## Local vs remote

`--remote` is parsed once, in `cmd::run`, into a `Target` (`Local` or
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
  for something that went wrong but lodge carries on, `error!` for why a command
  failed, `note!` for hints, and `line!` for reports and lists (no prefix).
  Every message line gets the `[lodge]` prefix, including continuation lines of
  a multi-line message, so call sites write plain text. `--verbose` also traces
  every command lodge spawns, in shell-pasteable form; `-vv` / `-vvv` pass `-v`
  to ssh. Reports and questions belong to `cmd/`. *Enforced:* clippy denies
  `println!` and friends outside `output.rs`.
- **One place per external program.** `ssh` is started only in `ssh.rs`,
  `docker` only in `runtime/docker/`, a local `sh` only in `target.rs` (scripts
  on the project's host), `secret/` (a secret's `command` source, which is on
  this machine whatever the target) and `session/` (launching the user's
  session), and the helper tools (`ldd`, `patchelf`, `oras`, `tmux`) only
  through `host_tools`. lodge never runs a local `tar`; tar streams are built and
  unpacked in-process. `ssh.rs` offers `command`, `sh_output`, `write_file`,
  `tunnel`, `pty_denied` and `interactive_line`, all built on one private
  `base`; nothing else builds an `ssh` command. *Enforced:* the tests in
  `conventions.rs`.
- **Paths in the container** come from `layout.rs`; **the container user** is a
  `ContainerUser` resolved once by the command; **names written into scripts**
  are checked with `shell::is_plain_name` / `is_alias_name`.
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
- **Failures.** Hooks and feature installs warn and continue, except
  `initializeCommand`, which runs before anything starts and aborts (as the spec
  asks); structural problems (no config, container won't start) abort with an
  error. "Could not find out" is never treated as "absent": `Env::read_file`
  tells a missing file from a failed read.
- **Docs follow the code.** `doc_facts.rs` checks what the docs can be checked
  on: every command appears in the README and in the list above, the documented
  `skip_if_exists` default, the README's host tools, its links, and that
  `docs/config.schema.json` is current (`make schema` regenerates it).

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
might refuse. The one exception is a `sh -c` script run for its effect, which is
recorded and assumed to work; tests assert on the recorded script, and scripts
whose behaviour matters (secrets, `ldg clean`'s wrapper search) are also run for
real against a temp directory. The fake records who each command ran as
(`ran_as`), so "as root" and "as `remoteUser`" are tested too. Tests that need
scratch files use `tmpfile::TempDir`, which removes itself even when the test
fails.

**Integration tests** (`tests/integration/`) drive the real `ldg` binary against a
real container engine: create and reuse, hooks, drift, injection and library
refresh, the architecture refusal, uid alignment, container settings and
variables, compose, forwarding (loopback-only, one daemon), `--remote`, bundles
(pack, inspect, verify, damage, the cache, injecting without `ldd`/`patchelf`)
secrets (never in the image, the trace or the disk; scoping), hooks and feature
scripts as `remoteUser`, local features over `--remote`, one container per
folder however it is spelled, `ls`/`status --json`/`ports`/`logs`/`clean`, config
typos, `init`/`completions`/`man`, a real tmux session, a glibc tool in an Alpine
(musl) container, and `--remote` against a real OpenSSH server. They are `#[ignore]`d, so `cargo test`
stays Docker-free; run them with `make integration` (about 30 s; quiet on success, and a failure shows the tail of
the command that failed). Narrow with `make integration T=uid`. They need docker
with compose, `patchelf`, `tmux` and an ssh client (the tests build their own
`sshd` image and pull `debian:stable-slim` and `alpine:3`).

Each test gets a sandbox: its own config, data and temp directories, and, for
`--remote`, a fake `ssh` on `PATH` that skips its options and runs the remote
command locally, so no second machine is needed. One test instead starts a real
`sshd` (a container with the docker CLI and this engine's socket, the sandbox
mounted at the same path) and uses the real ssh client through a wrapper that
adds a private `-F` config. A private `TMUX_TMPDIR` keeps tmux tests away from
the tmux you may be running them in. A project removes its container
when dropped, even if the test failed, and the sandbox waits for its forward
daemons before deleting itself. `ldg up --no-attach` and `ldg exec --no-attach`
do all the work and exit without opening a shell, which is what makes the exit
codes meaningful. Terminals are pseudo-terminals made by `script`; nothing
checks what a person would see on screen.

CI (`.github/workflows/ci.yml`) runs, on every push and PR: fmt, clippy
`-D warnings`, the unit tests and a release build; the integration tests; a build
with the minimum supported Rust (`rust-version` in `Cargo.toml`); `cargo deny`
(`.github/deny.toml`: licenses, advisories, sources); and the static musl builds for
x86_64 and aarch64. On `master`, once all of those pass, release-please
(`.github/release-please/`) keeps a release PR
open that bumps the version in `Cargo.toml` from the conventional commits (`feat`
the minor version, `fix` the patch, a breaking change the minor before 1.0; no
changelog file). Merging it tags the version and creates a *draft* release; the
binaries (`ldg-<target>.tar.gz`, what `install.sh` downloads) are attached, and
only then is the release published, so "latest" never lacks them. `make check` runs the same gate locally, `make deny`
the dependency check.

The repo's own devcontainer (`.devcontainer/`) has the docker CLI and compose
(talking to the host's engine through its socket), host networking and a shared
`TMPDIR`, so `make integration` runs inside it as well; `make dev` opens it with
the `ldg` built from the tree.
