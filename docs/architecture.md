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
                     → inject::installed (skipped for a new container or --reinject)
                     → inject::resolve_layout → inject::run (only when nothing is injected)
                     → Started::run_post_attach → secret::deliver → session::enter
ldg down [PATH]      devcontainer::stop
ldg status [PATH]    devcontainer::status (state, drift) + installed bundle + placed secrets
ldg rm [PATH]        devcontainer::remove (container, and the compose project)
ldg ls               Runtime::list_labeled (every container with lodge's project label)
ldg ports [PATH]     forward::active_ports (what the daemon reports)
ldg logs [PATH]      the daemon's log file
ldg clean [PATH]     ContainerUser::resolve → inject::installed → inject::remove
ldg init             a commented config from the tools found on PATH
ldg doctor [PATH]    host tools, engine, compose, config, binaries, secrets
ldg pack             bundle::pack::pack_to_file (config → bundle tar)
ldg bundle inspect|verify FILE   bundle::inspect / bundle::verify_file
ldg cache [clear]    bundle::cache::entries / clear
ldg completions, ldg man         generated from the clap definition
ldg _forward …       forward::run_daemon (internal; spawned by `up`)
```

`src/main.rs` only parses the arguments and hands them to `cmd::run`. `cmd/` is
the command layer: it builds a `Ctx` (the `Host`, the `Runtime` and the config
path; see "Hosts and injection"), loads the config **once** per command, resolves the container user once
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
| `config/` | The user's `config.toml`: `[[inject]]`, `[session]`, `[git]`, `[[secret]]` (its declaration types are in `config/secret.rs`: `RawSecret` as written, `SecretDecl` once checked), `bundle`, `engine` (also readable on its own with `engine_setting`, because the engine is chosen before a command loads the config). Unknown keys and wrong types are errors (with a "did you mean"); past those, every problem, secrets included, is reported at once. A relative `bundle =` resolves against the file's folder. The JSON schema in `docs/config.schema.json` is generated from these types. |
| `devcontainer/` | Everything about running a `devcontainer.json` (see below). |
| `bundle/` | A bundle is a portable tar of relinked binaries, their libraries and configs, with a manifest, and is tied to no install prefix (see "Injection"). `manifest` defines and validates it (paths derive from names, so a bundle can't point outside itself), `archive` writes, reads and verifies the tar, `pack` builds one from the config (the only step needing a relinker; `deps` asks a `DepReader` once per binary however often the answer is needed, `elf` reads a binary's architecture, as an `arch::Arch`, the one name for a CPU that bundles, binaries and machines are compared by, and `Arch::host()` is this machine's, and writes the interpreter of a dynamic binary in place), `source` readies a bundle *directory* to inject from (extracted from a file, or packed live), keyed by the file's hash or a fingerprint of the live inputs, and `cache` keeps those directories under `~/.cache/lodge/bundles/`. `hash` is the content hash of a file or tree. |
| `inject/` | Copies a bundle's binaries, libraries and configs into a container for one `ContainerUser`, reading only from the bundle directory and acting *as that user* (through `runtime::Acting`), so it never needs root: `prefix` (chooses and checks the prefix: the user's home, else a private `/tmp` directory; `installed`, in `state`, finds where it was installed), `binary` (rewrites a dynamic binary's interpreter for the prefix, and writes the wrapper a config asked for), `libs` (tracked by content hash), `config_files`, `git` (`safe.directory`), `installed` (a hash and the destination per installed piece; a record that can't be read means that piece is installed again, and the list of records `ldg clean` works from is an error when it can't be read, not "nothing installed"), `state` (the installed bundle id, and `installed`, which finds what was injected and where), `remove` (`ldg clean`; placed secrets are removed by `secret::place::clear`). |
| `layout.rs` | Every path lodge writes inside a container: `Layout`, the paths inside the user's prefix (the table under "What lives where"); the provisioning state, which is the devcontainer's own and root-owned; and the secrets directory. Nothing else spells these paths out. |
| `secret/` | Secrets that must not be in a bundle. Their `[[secret]]` declarations are config (`config/secret.rs`: validated; names reach shell scripts). `mod` is the `Secret` value type (redacted `Debug`, no `Display`), matching `targets` globs, and `resolve` (env, file or a command run on *this* machine with a timeout, with errors that never contain the value). `place` writes values into a container under `/dev/shm/lodge-secrets-<uid>/` and builds the session lines that export them; `clear` removes the user's (`ldg clean`). `deliver` picks the secrets whose `targets` match, resolves and places them (or clears them when none are declared), and prints names only. |
| `session/` | Opens the interactive session: `docker exec -it`, and nothing around it; after a lost `--remote` connection it offers to reconnect. Its start-up script (`startup_script`) starts the shell as a login shell, so the profile scripts run, then puts the user's prefix `bin/` first on `PATH` and restores what the profile dropped before running the shell interactively; that is how the injected tools reach the session without any file outside the prefix. `terminfo` makes the terminal the session is opened from work in the container (copies its entry from this machine into the prefix when the container lacks it, and sets `TERM`). |
| `forward/` | Port forwarding. `mod` spawns the daemon and owns its state files (log, lock, and the ports file `ldg ports` reads, which starts with the daemon's pid so a dead daemon's file is ignored), `daemon` is the `_forward` loop (declared ports plus whatever starts listening), `tunnel` one forward: the in-process TCP proxy, or the tunnel process the `Host` provides (`ssh -L` for `SshHost`). The daemon decodes its own `--port` flags, re-points a forward when its destination's address changes (looked up again every 30 s), and exits when the container stops, not when the engine merely can't be reached for a while. |
| `port.rs` | `PortForward`, a port to forward: what the devcontainer spec declares and the daemon carries out, so neither depends on the other. |
| `tools/` | The build-time helpers behind interfaces: `DepReader` (which libraries a binary needs), `ElfEditor` (relink a copy of a binary for the bundle's prefix) and `OciFetcher` (pull a feature's layer), bundled as a `Toolchain` that `cmd::run` builds. `external` has the implementations that run the programs (`Ldd`, `Patchelf`, `Oras`, parsing `ldd`'s output); a native one is one more type. Packing, the bundle cache and feature fetching take the interfaces and are tested with fakes. |
| `host_tools.rs` | The helper programs lodge may run here (`ssh`, `ldd`, `patchelf`, `oras`): what each is needed for (a `Need`, which decides when `doctor` requires it) and how to get it. `doctor` reports from this table and each tool is started only through it (the `tools` implementations and `host/ssh.rs`). |
| `host/` | How a machine is reached. `mod` is the `Host` trait (a local process that runs a command on the host, one with a terminal, a tunnel, the connection hooks) with everything built on those provided: running a script, `read_first` (finds files by pattern and reads the first in one round trip), `read_file`, `canonical_dir`, `fetch_dir` (copies a folder from the project's host), `write_scratch`, `uid_gid`; and `attempt`, which reruns a step after a lost connection if it is marked `Repeat::Safe`. `local` is `LocalHost`, this machine. `ssh` is `SshHost`: `--remote` destination parsing, the shared connection (`connect` opens it up front with the terminal available, `ControlMaster` multiplexes everything after), and the few ssh invocations lodge needs, all built from one private `base`. |
| `runtime/` | The seam to whatever runs the workload: the `Env`, `Container`, `Runtime` and `Compose` traits (see below), `User` (who a command runs as) and `ContainerUser` (the session user's name, uid, gid and home, looked up once), `Acting` (an `Env` seen as one user, below), `tarball` (packs files as a tar owned by a user), the docker CLI implementation (`docker/`: the engine, which runs on a `Host` it is given, the per-container `Env`, and the file-copy streaming), and strict in-memory fakes for tests (`FakeEnv`, and `FakeRuntime`, whose containers each run in one). |
| `shell.rs` | POSIX `shell_quote`, which everything that builds a command string goes through; `script_argv`, how every script with arguments is started (`sh -c <script> sh <args…>`, so arguments are `$1`, `$2`, …); the rules for names written into scripts unquoted (`is_plain_name`, `is_var_name`) and for destinations in a container (`container_path_problem`); `slug` for container names; and `CmdLine`, a command as `sh -c` script or argv. |
| `fingerprint.rs` | The small content hash used to notice changes (devcontainer inputs, updated libraries, bundle contents), as a `Fingerprint` builder. Its values are stored, so a test pins them. |
| `tmpfile.rs` | Self-deleting staging files for `docker cp`. |
| `tree.rs` | Walking a file tree on this machine in one stable order (name order, a directory before its contents), with an explicit choice about symlinks (a followed link loop is entered once) and unreadable directories. Every hash, fingerprint and copy of a tree goes through it. |
| `output.rs` | Everything lodge says (`info!`, `note!`, `warning!`, `error!`, `prompt!`, `line!` for reports, and `line_err!`, `scoped!`, `raw_stdout`, `raw_stderr` for the special cases), the `[lodge]` prefix on every line, `plural`, and the verbosity setting with command tracing (`trace`, `trace_line`). |
| `conventions.rs`, `doc_facts.rs` | Test-only: architecture rules (one place per external program, only `cmd/` chooses implementations, the direction of dependencies), and facts the docs state checked against the code. |

### `devcontainer/`

`start()` in `mod.rs` is the orchestration; each step is its own module.

| File | Responsibility |
| --- | --- |
| `mod.rs` | `start` / `stop` / `remove` / `status` / `find`, the `Started` result, whether the container was created, and `Prompt`, the questions only a person can answer (the command layer implements it). Drift and the installed bundle are read with `Container::read_stored_file`, which works on a stopped container too. |
| `spec.rs` | Find the config on the project's host (through `Host`, so every host shares one lookup order); parse `devcontainer.json` (JSONC) into a backend-agnostic `Spec`; derive the session name. |
| `backend.rs` | Picks and builds the backend from the parsed config (with the stable container name, and the files outside the hashed folder that count toward drift: a Dockerfile, compose files). The `Backend` trait with two implementations: `ComposeBackend` (`docker compose`, run on the project's host) and `ContainerBackend` (`docker run` of an image or a local build). Both label the container `dev.lodge.project=<path>`. |
| `create.rs` | `CreateSettings`, how a container is created (privileged, capabilities, mounts, published `appPort`s, …), from devcontainer.json and from features alike, rendered as `docker run` flags or a compose override. |
| `features/` | Devcontainer *features*: `fetch` (OCI with `oras`, or a local folder from the project's host; a feature's `id` must be a plain name, since it names a folder), and in `mod.rs` the order, the creation settings they ask for, and the install (as root, told `_REMOTE_USER`), with one env profile per feature. |
| `lifecycle.rs` | The `Hook` type (string / argv / named map, all run as the same steps), running hooks on the host or in the container (as `remoteUser`), and the create-time hooks that run once successfully (`run_create_hooks`). |
| `profile.rs` | The env profiles written into the container (`containerEnv`/`remoteEnv`, and what features add): literals quoted, variable references left for the login shell. |
| `markers.rs` | One-shot markers: which create-time hooks and feature installs already succeeded. |
| `drift.rs` | Drift detection: fingerprint the folder the config is in (`.devcontainer/`, or `.devcontainer/<name>/`), or a standalone `.devcontainer.json`, plus the files the backend names outside it (a Dockerfile, compose files), and compare with the value stored in the container. `config_state` is the one comparison, used by `ldg up` and `ldg status` alike: `UpToDate`, `Changed`, or `Unknown` when no hash is recorded or it can't be read, which is not drift. The files are read through `Host` (`fetch_dir`, `read_file`), so a remote project is hashed exactly like a local one. |
| `workdir.rs` | The directory a session lands in: `workspaceFolder`, or one the person picks when it doesn't exist. |
| `uid.rs` | Lines the container user's uid/gid up with the host user's so a bind-mounted workspace is writable. Worked out in Rust from `/etc/passwd`, applied by a small script. |
| `vars.rs` | `${localEnv:X:default}`, `${containerWorkspaceFolder}`, `${devcontainerId}` and friends, and `path_tag`, the short hash of the project path behind `${devcontainerId}` and the container and session names. |

The `Backend` split is what keeps `start()` simple: a backend only knows how to
find an existing container and how to create or start one. `up` is handed what
`existing` found, so the image backend doesn't look again; compose finds its own
container (and is asked for its id after `up` anyway). Hashing, env, features
and hooks all work from the resulting container id.

## What `ldg up` does, step by step

0. **Load the config and ready the bundle** (`cmd::attach`), so a broken config or
   bundle fails before anything starts. The project path is made canonical on
   the project's host (`Host::canonical_dir`; on another machine, `pwd -P`
   there), so one folder is one container however it is spelled.
1. **Load the spec.** Find and parse `devcontainer.json`; pick a backend.
2. **Detect drift.** Fingerprint the inputs and compare with the hash stored in
   the existing container. On a mismatch, say so and keep the container; only
   `--build` recreates. If no hash is recorded, or it can't be read, the
   container is kept (`ldg status` says `unknown`).
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
    uid, gid, home) once, then **start the port-forward daemon**. Then **inject
    your environment** for that user, as that user, into the **prefix**, but only
    when the container was just created, `--reinject` was given, or nothing is
    recorded as injected. Otherwise the prefix is found from the bundle record
    and left alone; if the recorded bundle id differs from the current bundle's
    (config or ldg changed), that is reported.
11. **`postAttachCommand`**, then **deliver secrets** and **open the session**,
    which first copies in the terminfo entry of the terminal it is opened from
    and puts the prefix's `bin/` on the `PATH` of the shell it starts.

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

Injection is done **as the container user, into a prefix that user owns**, and
never needs root (see `runtime::Acting`). `ldg up` first resolves the
user once (`ContainerUser`: the configured `remoteUser`, or the container's default
user) and then the prefix: `~/.local/share/lodge`, or, when the home can't hold
programs (none, not writable, mounted `noexec`: checked by writing a script there
and running it), a private `/tmp/lodge-<uid>`. lodge does not repair a home the
user can't use (it would need root): the prefix then goes to `/tmp`, and a config
whose target is under `~` fails with the path and the user named.

For each tool in the bundle (`inject/`):

- **One copy for the prefix.** Libraries (with the dynamic linker, so they never
  come from different versions) and binaries are staged in a local directory laid
  out as the prefix (`lib/`, `bin/`) and sent as a single tar. `<prefix>/bin` and
  `<prefix>/lib` are removed first, so a re-injection leaves nothing of the
  previous one (a running binary can be unlinked, not overwritten), and the copy
  costs the same few engine calls however many files it holds.
- **Binaries.** A bundle holds no install prefix: a dynamic binary was packed with
  a placeholder interpreter filling a reserved 512 bytes and a library search path
  relative to itself (`$ORIGIN/../lib`). Staging overwrites the placeholder in
  place with `<prefix>/lib/<ld>` (`bundle::elf::set_interpreter`: the file keeps its
  size and layout, so `patchelf` is not needed here). It runs directly, without an
  `ld.so` indirection, and `/proc/self/exe` stays correct for tools that re-exec
  themselves. A static binary is simply copied. Only if the config sets
  `target_binary` is a two-line wrapper script written there, which `exec`s the
  binary.
- **Configs** are copied to their target (default `~/.config/<name>`) with the user
  as owner of every directory down to it, so nothing needs a `chown`. A target the
  user can't write is an error naming the path and the user. A target that
  already exists and that an earlier injection did not place (it is not in
  `<prefix>/injected`) is replaced with a warning; one lodge placed is replaced
  silently, edits made in the container included.
- **Git.** `safe.directory` is set (`git config --global`, as the user) so tools
  can run `git` on host-owned files.

Nothing is written outside the prefix except those targets: no `PATH` or alias
setup in `/etc/profile.d`, no wrappers in `/usr/local/bin`. The tools reach the
user through the session (`session::startup_script`), which puts `<prefix>/bin`
first on `PATH`.

The terminal's terminfo entry is not part of the injection: it belongs to the
machine a session is opened from, not to the bundle, so `session` copies it
when it opens one.

Injection always replaces what it injects; there is no comparing with what is
already there. Whether to inject at all is `ldg up`'s decision: always into a
container it just created, only on request (`--reinject`) into an existing one.
The record `<prefix>/injected` is written last, so an injection that failed halfway
reads as none.

## What lives where

**Inside the container** (each path comes from `layout.rs`)

The user's prefix, `<prefix>` = `~/.local/share/lodge` (or `/tmp/lodge-<uid>`), is
what `ldg clean` removes:

| Path | Contents |
| --- | --- |
| `<prefix>/bin/` | The injected binaries; first on the session's `PATH`. |
| `<prefix>/lib/` | Copied libraries and the dynamic linker, one copy. |
| `<prefix>/injected` | The id of the bundle last injected, and the wrappers and configs placed outside the prefix. `ldg status` shows the id, `ldg clean` removes the wrappers and reports the configs, and its presence is how both find the prefix. |
| `<prefix>/terminfo/` | The user's terminfo entry, when the container lacked it. |

Provisioning state, which is the devcontainer's own and written as root when the
container is created (so `ldg clean` keeps it):

| Path | Contents |
| --- | --- |
| `/opt/lodge/state/` | One-shot markers (`onCreate`, `postCreate`, feature installs). |
| `/opt/lodge/devcontainer.hash` | Fingerprint of the devcontainer inputs. |
| `/etc/profile.d/lodge-env.sh` | `containerEnv`/`remoteEnv`, for login shells. |
| `/etc/profile.d/lodge-feature-<id>.sh` | One feature's `containerEnv`, so installing one feature again leaves the others' env alone. |
| `/tmp/lodge-features/` | Devcontainer feature files, copied in to run their `install.sh` (left in place; `ldg clean` doesn't touch them). |

Elsewhere:

| Path | Contents |
| --- | --- |
| `/dev/shm/lodge-secrets-<uid>/` | Placed secret values (memory only, 0700 directory, 0600 files); see `secret/`. |
| the `target_config` / `target_binary` paths | What the config asked to be placed there; `ldg clean` removes the wrappers and keeps the configs. |

Because all state is *in* the container, it survives reboots and disappears
when the container is recreated, with no host bookkeeping to go stale. Every
container lodge creates also carries the label `dev.lodge.project=<path>`.
`ldg clean` removes the injection (the prefix, the wrappers it wrote and the user's
secrets) and keeps the devcontainer's state (markers, hash, `containerEnv`).

**On the host**

| Path | Contents |
| --- | --- |
| `~/.config/lodge/config.toml` | Your configuration. |
| `~/.cache/lodge/bundles/` | Extracted or packed bundles, the four most recently used. |
| `~/.local/share/lodge/forward-<id>.log`, `.lock`, `.ports` | Log of the port-forward daemon (started afresh by each daemon that takes over), the lock that keeps it to one per container, and its active forwards (for `ldg ports`, replaced whole on every change). |
| `$TMPDIR/lodge-ssh/*.sock` | SSH `ControlMaster` sockets (`--remote` only). |
| `$TMPDIR/lodge-<pid>-*` | Short-lived staging files and folders, removed when done. Each is created new and private to the user (0600 / 0700), never opened if the name is already taken. On a remote host the compose override comes from `mktemp`. |

## Remote mode

With `--remote <dest>`, `cmd::run` builds an `SshHost` instead of a `LocalHost`
and the docker CLI runs *on the remote host* over SSH, so no local Docker is
needed. Nothing but `host/ssh.rs` knows that; this section is what `SshHost`
does behind the `Host` trait. Four things make it work:

- `DockerCli` asks its host for a process that runs `docker …`
  (`Host::command`), and `SshHost` answers with `ssh <dest> …`, shell-quoting each
  argument. Anything that reads **files** (compose projects, build contexts)
  goes through `DockerCli::command_in`, which hands the host the project
  directory too, so `SshHost` `cd`s into it *on the remote*.
- **Connection reuse.** A single `ldg up` issues dozens of SSH calls.
  `Host::connect` opens one connection first (a password or 2FA prompt is
  answered once, with the terminal available) and every later call is
  multiplexed over its `ControlMaster` socket, one per destination, with
  `ServerAliveInterval=15` so a dead link fails in about 45 s instead of
  hanging.
- **Lost connections and repeat-safety.** ssh exiting 255 says the connection
  failed, not whether the remote command ran, so a step is rerun only if it is
  safe to run twice. Every remote step says which it is: `Repeat::Safe` (reads
  and probes, `mkdir -p`, `rm -rf`, the tar copy, an atomic secret write,
  `start`/`stop`/`rm -f`/`build`/`compose up|down`) or `Repeat::Never`, the
  default (user hooks, a feature's `install.sh`, the uid change,
  `initializeCommand`, scratch files, the interactive session). Exec steps set it
  with `ExecOpts::repeatable()`, everything else goes through
  `Host::attempt`. The host decides what counts as a lost connection
  (`Host::link_lost`: for `SshHost` exit 255, unless ssh says the login or host
  key was refused), how often to retry and how to reset (`retry_policy`,
  `reset_link`); a safe step is retried up to three times, waiting 1 s then
  2 s, after the dead master is dropped. A host whose connection cannot be lost
  (`LocalHost`) never retries. `docker run` is not repeatable as such, so a retry first looks for the
  container by name and starts what it finds. The forward daemon restarts a dead
  tunnel with a wait that doubles from 2 s to a minute. It has no
  terminal, so its tunnels never prompt: they ride on the connection `connect`
  opened, or on key or agent auth.
- **Local bytes → container.** The remote docker can't see local paths, so
  `put` streams a tar to `docker cp -q -a - <id>:<dir>`, locally and over SSH alike,
  packed in-process (`runtime::tarball`), so no local `tar` is needed, none in the
  container, and the entries carry the owner's uid and gid, which `-a` makes the
  daemon honour. The devcontainer config is found with
  `Host::read_first` (one probe script remotely, which also returns the
  file, with the same lookup order as locally); its inputs are fetched for
  hashing with `Host::fetch_dir`.

The destination accepts a `~/.ssh/config` alias, `user@host`, `user@host:port`,
`[v6]:port` and an `ssh://` prefix (`ssh::split`, private to `host/ssh.rs`).

## Runtime traits

`runtime/` separates *what lodge needs* from *how docker provides it*, in three
small traits from narrow to broad, plus one for compose:

| Trait | A… | Used by |
|---|---|---|
| `Env` | place you can run commands in and copy files into | injection, features, lifecycle hooks, markers, the stored config hash |
| `Container: Env` | `Env` that can also be inspected (name, mounts, address, peers, running: `Result<bool>`, so "stopped" and "couldn't ask" differ) and read while stopped (`read_stored_file`, through `docker cp`) | port forwarding, workdir selection, the session, drift, `ldg status` |
| `Runtime` | container engine: find, build, run, start, stop | devcontainer backends, `start`/`stop` |
| `Compose` | an engine's compose support (`Runtime::compose()`, an error where there is none): up, down, ps | the compose backend, `doctor` |

`Env` carries the primitives (`exec`, `output`, `put`, plus `write_secret` and
`interactive_command`, which need engine-specific stdin and TTY handling) and
provides everything else on top of them: `write_file`, `read_file`, `mkdir_p`,
`exists`, and so on. So code that injects a tool takes one `&dyn Env`
instead of a `(docker, container)` pair, and anything new that implements the
primitives gets the rest for free. The traits know nothing of lodge's own
layout: markers, hashes and profiles live in the modules that use them. An
`Env` doesn't say whether it is local or remote either; code that needs to
know (matching a secret's `targets`, diagnosing a refused PTY) is handed the
`Host` by the command and asks it (`names`, `pty_denied`).

`put` behaves the same on every endpoint: a file becomes `dest`, and a
directory's *contents* are merged into `dest`; every missing directory down to
it is made, and everything created is owned by the `owner` it is given. Callers
that want an exact copy remove `dest` first.

**Acting as a user.** The provided helpers act as `Env::actor()`: root for the
plain environment, which is what provisioning a devcontainer uses. What lodge
does *for a user* (inject, write their state, open their session) goes through
`Acting::new(env, &user)`, an `Env` whose default user is that user, whose helpers
(`mkdir_p`, `write_file`, `read_file`, `exists`) run as them, and whose copies are
theirs. So the injector and the code under it have no way to need root: asking
for it takes an explicit `exec_root`, which none of them contain.

`runtime::docker` implements all four over the `docker` CLI, run on whichever
`Host` it is given. `runtime::fake::FakeEnv` is an in-memory `Env` that
records commands and keeps a virtual filesystem, which is what the injection
tests run against; `FakeRuntime` is an engine (with compose) that keeps its
containers by name, each in its own `FakeEnv`, and refuses to act on one that
doesn't exist, which is what `devcontainer::start`'s create / reuse / recreate decisions are
tested against. Another engine (Podman, kubectl), or a plain host with no engine at all, would
implement the same traits.

Backends (`devcontainer/backend.rs`) are plain data: the `Runtime` is passed in
rather than stored.

## Hosts and injection

Every dependency sits behind an interface and is injected, so a new
implementation is one new type handed in at one place:

| Dependency | Interface | Implementations |
| --- | --- | --- |
| How a machine is reached | `host::Host` | `LocalHost`, `SshHost`; anything else that can run a command implements the two primitives |
| The container engine | `runtime::Runtime`, `Compose`, `Container`, `Env` | `runtime::docker`, and the in-memory `FakeRuntime` for tests |
| The build-time helpers | `tools::DepReader`, `ElfEditor`, `OciFetcher` (a `Toolchain`) | `Ldd`, `Patchelf`, `Oras`; a test passes fakes |
| What is injected | `[[inject]]` data | multiplexers and other tools are injected like any other, never named in code |

The pieces compose instead of branching. The engine is built *on* a host
(`DockerCli::new(host)`), so "docker over ssh" is the same engine with a different
host, not a third case; a remote Podman would be a second engine on the same
host. `Host` has two primitives, `command` (a local process that runs a command
on the host) and `interactive` (the same with a terminal), plus the connection
hooks (`connect`, `link_lost`, `reset_link`, `retry_policy`, `pty_denied`,
`tunnel`); everything else (`sh_output`, `run_in`, `canonical_dir`, `read_first`,
`read_file`, `fetch_dir`, `write_scratch`, `remove_file`, `uid_gid`) is provided on
top of them, so an implementation gets it by writing the primitives and
overrides only what it can do better (`LocalHost` reads files directly). The
compose backend writes its temporary override with `write_scratch`, whose guard
removes it again.

`--remote` and `--engine` (or `engine` in the config, `choose_engine`) are read once, in
`cmd::run`, which is the composition root: the only place that picks `LocalHost` or `SshHost` and, with
`build_engine`, the engine built on it (`DockerCli`, for `--engine docker`), then
puts both in the `Ctx`, together with the `Toolchain` of helper tools it builds. A child lodge (the forward daemon) is pointed at the same
pair through `Host::respawn_args` and `Runtime::respawn_args`. From there on nothing handles a bare `Option<String>`
or matches on a kind of host. The command hands `&dyn Host` (or `HostRef`, where a
backend keeps one) to whatever needs it (`devcontainer`, `forward`, `secret`,
`session`) and the `Runtime` as `&dyn Runtime`. What differs between hosts is
asked as a capability, not by name: `shares_cwd` (may the project path be left
out), `names` (what a secret's `targets` match), `respawn_args` (how the forward
daemon reaches the same host), `tunnel` (the process that forwards a port, or
`None` when the engine's addresses are reachable from here and the in-process
proxy is used).

To add a way of reaching machines (an ssh successor, `gh codespace ssh`, an
agent), implement `Host` in a new file under `host/` and add one arm to the
choice in `cmd::run`. To add an engine, implement `Runtime` (and `Env`,
`Container`) under `runtime/`, add a variant to `runtime::Engine` and an arm to
`cmd::build_engine`. To replace a helper tool (a native OCI client, a built-in ELF
writer), implement `OciFetcher` or `ElfEditor` in `tools/` and swap it in the
`Toolchain` that `cmd::run` builds. Nothing else changes, and
`conventions.rs` fails if anything else starts to name the new type.

## Conventions

Most of these are enforced by the compiler or a test, so they can't quietly
erode.

- **Output.** Everything goes through `output`: `info!` for progress, `note!`
  for hints (tagged `note:`), `warning!` for something that went wrong but lodge
  carries on, `error!` for why a command failed, `prompt!` for a question, all on
  stderr, and `line!` for reports and lists (no prefix), the only thing on
  stdout, so a script reading lodge's output gets just the data. `line_err!`
  (an unprefixed line on stderr, such as a question's choices), `scoped!` (the
  forward daemon's log) and `raw_stdout`/`raw_stderr` (a generated file, a log,
  another program's output, passed through untouched) cover the rest. Counts use `output::plural`.
  Every message line gets the `[lodge]` prefix, including continuation lines of
  a multi-line message, so call sites write plain text. `--verbose` also traces
  every command lodge spawns, in shell-pasteable form; `-vv` / `-vvv` pass `-v`
  to ssh. Reports and questions belong to `cmd/`. *Enforced:* clippy denies
  `println!` and friends outside `output.rs`.
- **Every dependency is injected.** The way a machine is reached is a `Host`, the
  engine a `Runtime`, and only `cmd/` (the composition root, `cmd::run`) names a
  concrete one (`SshHost`, `LocalHost`, `DockerCli`); everything else takes the
  interface. No code matches on a kind of host or engine or compares one's name:
  it asks a capability (see "Hosts and injection"). Each interface has a fake or
  a test host, so a branch is tested without docker, ssh or a network.
  *Enforced:* `conventions.rs` fails if a concrete host, engine or tool is named
  outside its own module and `cmd/`.
- **One place per external program.** `ssh` is started only in `host/ssh.rs`;
  `docker` is never started directly: `runtime/docker/` asks its `Host` to run it,
  and no other module spells it out; a local `sh` only in `host/local.rs` (scripts
  on this machine as the project's host) and `secret/` (a secret's `command`
  source, which is on this machine whatever the host); and the helper tools
  (`ldd`, `patchelf`, `oras`) only in `tools/external.rs`, through `host_tools`,
  behind `DepReader`, `ElfEditor` and `OciFetcher`. lodge never runs a local
  `tar`; tar streams are built and unpacked in-process. `host/ssh.rs` builds
  every `ssh` command from one private `base`. *Enforced:* the tests in
  `conventions.rs`.
- **Dependencies point one way.** `cmd/` on top (the only layer that knows
  `cli`), lodge's features below it (`config`, `devcontainer`, `bundle`,
  `inject`, `secret`, `session`, `forward`), and a foundation (`runtime`,
  `host`, `tools`, `shell`, `layout`, `output`, `tmpfile`, `tree`,
  `fingerprint`, `host_tools`, `port`) that names none of them. *Enforced:* the
  tests in `conventions.rs`.
- **Paths in the container** come from `layout.rs`; **the container user** is a
  `ContainerUser` resolved once by the command, after `devcontainer::start`
  (inside `start` the user's uid may still change, so uid alignment and feature
  scripts look it up there); **names written into scripts** are checked with
  `shell::is_plain_name` / `is_var_name`, and **destinations
  in a container** that come from a config or bundle with
  `shell::container_path_problem`. Anything read from a bundle or a feature's
  metadata is checked the same way before it becomes a path.
- **Running commands.** In a container through an `Env`; engine operations
  through a `Runtime`; on the project's host through `Host` (`sh_output`,
  `run_in`, `canonical_dir`, `read_first`, `read_file`, `write_scratch`, `remove_file`,
  `uid_gid`).
- **Quoting.** Anything interpolated into `sh -c` or an SSH command string goes
  through `shell::shell_quote`. Don't build such strings by hand; a script that
  takes arguments gets them through `shell::script_argv`, as `$1`, `$2`, ….
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
  tells a missing file from a failed read, and `Env::exists` a missing path
  from a failed check (a create hook whose marker can't be checked is skipped,
  not run again), and `ldg clean` fails when something could not be removed. The `bool` check (`succeeds`) is only for
  answers where a wrong "no" is harmless.
- **Docs follow the code.** `doc_facts.rs` checks what the docs can be checked
  on: every command appears in the README and in the list above, the README's host
  tools, its links, and that
  `docs/config.schema.json` is current (`make schema` regenerates it).

## Testing

`cargo test` runs the unit tests and needs no Docker. They cover the pure parts
(config parsing and building a `Spec`, ldd parsing, shell quoting, SSH
destination parsing, port-forward parsing, hash normalisation, feature ordering,
the uid plan, library refresh) and, through `FakeEnv`, the logic that writes into
a container: binaries (and the interpreter rewrite for the prefix), configs
owned by the user with no root involved, the choice of prefix, the session's
`PATH`, the wrapper script, the env profiles, feature installs, the stored
hash, the hook markers and a whole `inject::run`. Through `FakeRuntime` they
cover what `ldg up` decides about an existing container (create, start, ask,
recreate, keep one with no recorded config) and what `ldg status` reports,
running or stopped. `conventions.rs` checks the architecture rules above.

`FakeEnv` is strict on purpose. It emulates a handful of commands, and a test
declares anything else it depends on (`with_output`, `with_failure`, which
`recover` lifts again, and `with_commands`). A command it neither emulates nor
was told about panics, so a test can't pass because the fake said "success" to
something a real container might refuse; and a `FakeRuntime` container that is
stopped refuses every command, as `docker exec` does. The one exception is a `sh -c` script run for its effect, which is
recorded and assumed to work; tests assert on the recorded script, and scripts
whose behaviour matters (secrets, `ldg clean`'s removal script, the prefix probe, the session's `PATH` script) are also run for
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
typos, `init`/`completions`/`man`, a glibc tool in an Alpine
(musl) container, and `--remote` against a real OpenSSH server. They are `#[ignore]`d, so `cargo test`
stays Docker-free; run them with `make integration` (about 30 s; quiet on success, and a failure shows the tail of
the command that failed). Narrow with `make integration T=uid`. They need docker
with compose, `patchelf` and an ssh client (the tests build their own
`sshd` image and pull `debian:stable-slim` and `alpine:3`).

Each test gets a sandbox: its own config, data and temp directories, and, for
`--remote`, a fake `ssh` on `PATH` that skips its options and runs the remote
command locally, so no second machine is needed. One test instead starts a real
`sshd` (a container with the docker CLI and this engine's socket, the sandbox
mounted at the same path) and uses the real ssh client through a wrapper that
adds a private `-F` config. A project removes its container
when dropped, even if the test failed, and the sandbox waits for its forward
daemons before deleting itself. `ldg up --no-attach`
does all the work and exit without opening a shell, which is what makes the exit
codes meaningful. Terminals are pseudo-terminals made by `script`; nothing
checks what a person would see on screen.

CI (`.github/workflows/ci.yml`) runs, on every push and PR: fmt, clippy
`-D warnings`, rustdoc with warnings as errors, the unit tests and a release
build; the integration tests; a build
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
