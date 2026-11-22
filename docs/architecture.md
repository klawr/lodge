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
ldg                  resume::pick (History → Choose → the command that opened it, and the route it was reached by) → then that command, as if typed
ldg up [PATH]        Endpoints::provision (devcontainer::Devcontainer, or plain_host::PlainHostEndpoints) → Attached, then attach:
                     ContainerUser::resolve → ForwardDaemon::start
                     → inject::installed (skipped for a new container or --reinject)
                     → inject::resolve_layout → inject::run (only when nothing is injected)
                     → Ctx::remember (not for --no-attach) → Attached::run_post_attach → secret::deliver → session::enter
ldg attach NAME      Runtime::existing (a handle that is not disposable) → is_running (never started) → the same attach as `up`, with `created: false`
ldg down [PATH]      Lifecycle::stop (devcontainer::stop; an error for a target with no lifecycle, a plain host)
ldg status [PATH]    Endpoints::status (state, drift) + installed bundle + placed secrets
ldg rm [PATH]        Lifecycle::remove (container, and the compose project)
ldg ls               Lifecycle::list (every container with lodge's project label)
ldg ports [PATH]     forward::active_ports (what the daemon reports)
ldg logs [PATH]      the daemon's log file
ldg clean [PATH|--container NAME]   ForwardDaemon::stop → ContainerUser::resolve → inject::installed → inject::remove
ldg init             a commented config from the tools found on PATH
ldg doctor [PATH]    host tools, engine (a pod: kubectl and the pod), compose, config, binaries, secrets
ldg pack             bundle::pack::pack_to_file (config → bundle tar)
ldg bundle inspect|verify FILE   bundle::inspect / bundle::verify_file
ldg cache [clear]    bundle::cache::entries / clear
ldg completions, ldg man         generated from the clap definition
ldg _forward …       forward::run_daemon (internal; spawned by `up`)
```

`src/main.rs` only parses the arguments and hands them to `cmd::run`. `cmd/` is
the command layer: it builds a `Ctx` (the `Host`, the `Endpoints`, the helper-tool `Toolchain`, the
`ForwardDaemon` and the config path; see "Choices and how they are wired"), loads the config **once** per command, resolves the container user once
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
| `cmd/` | One file per group of commands (`attach`: up/exec, `project`: down/status/rm/ls/ports/logs/clean, `doctor`, `init`, `bundle`: pack/bundle/cache), plus `resume` (a bare `ldg`: offers the history and turns the choice back into a `Command` and the `Route` to run it by; it connects to nothing, so `run` builds the host and target from it as if they had been typed), and `prompt`: the terminal side of `devcontainer::Prompt` and `Choose`, the generic "pick one of these" question, which `select` answers with a list moved over by arrow keys or vim keys (`map_key` turns a keypress into a `Key`, `Cursor` decides what it does to the selection and `frame` draws the list as text, all plain functions; `pick` is the raw-mode loop around them, and the only place that names `crossterm`; a terminal that can't give single keys is asked for a number instead). The options are a `table::Table`: titles and rows of `Cell`s, each with a `Tone` (what a row is called, plain, detail, or what sets a row apart), and the column that takes the room left over. The command says what matters; how that looks is `select`'s alone (`paint`: names bold, detail dim, a place on another machine in the terminal's own cyan, none of it as a colour under `NO_COLOR`, the selected row a bar across the width). The picker asks for the `Grid` at the width it has at each draw, so a resize only means the next draw is laid out again: extra room goes to that column (up to 120 characters, so a row can still be read across), too little shortens it in the middle, then the others widest first, and never the last. How airy the screen is `plan`'s decision, from the height at each draw: an empty line after the question and before the hints when every entry still fits with a line to spare, none when it doesn't, and past that the list scrolls under its question, titles and hints. The list is drawn on the alternate screen from its top left corner each time, so a resize cannot leave lines behind and the screen it came from is back untouched. The terminal is a dependency like the rest: a command takes the interface for its question, and only `run` names `Terminal`; a new question is a new method and its implementation there. The only layer that loads the config, prints reports and reads the terminal. |
| `config/` | The user's `config.toml`: `[[inject]]`, `[session]`, `[git]`, `[[secret]]` (its declaration types are in `config/secret.rs`: `RawSecret` as written, `SecretDecl` once checked), `bundle`, `target` and `targets`, a table of settings per target (`[targets.docker]`, `[targets.podman]`; the chosen one is also readable on its own with `target_setting` and `target_settings`, because the target is built before a command loads the config; what is in a table is checked by its target). Unknown keys and wrong types are errors (with a "did you mean"); past those, every problem, secrets included, is reported at once. A relative `bundle =` resolves against the file's folder. The JSON schema in `docs/config.schema.json` is generated from these types. |
| `devcontainer/` | Everything about running a `devcontainer.json` (see below). |
| `bundle/` | A bundle is a portable tar of relinked binaries, their libraries and configs, with a manifest, and is tied to no install prefix (see "Injection"). `manifest` defines and validates it (paths derive from names, so a bundle can't point outside itself), `archive` writes, reads and verifies the tar, `pack` builds one from the config (the only step needing a relinker; `deps` asks a `DepReader` once per binary however often the answer is needed, `elf` reads a binary's architecture, as an `Arch` (see `arch.rs`), and writes the interpreter of a dynamic binary in place), `source` readies a bundle *directory* to inject from (fetched and extracted through a store, or packed live), keyed by what the store identified or a fingerprint of the live inputs, `store/` is where a bundle comes from (see [`bundle/store/`](#bundlestore)), and `cache` keeps those directories under `~/.cache/lodge/bundles/`. `hash` is the content hash of a file or tree. |
| `inject/` | Copies a bundle's binaries, libraries and configs into a container for one `ContainerUser`, reading only from the bundle directory and acting *as that user* (through `runtime::Acting`), so it never needs root: `prefix` (chooses and checks the prefix: the user's home, else a private `/tmp` directory; `installed`, in `state`, finds where it was installed), `binary` (rewrites a dynamic binary's interpreter for the prefix, and writes the wrapper a config asked for), `libs` (tracked by content hash), `config_files`, `git` (`safe.directory`), `installed` (a hash and the destination per installed piece; a record that can't be read means that piece is installed again, and the list of records `ldg clean` works from is an error when it can't be read, not "nothing installed"), `state` (the installed bundle id, and `installed`, which finds what was injected and where), `remove` (`ldg clean`; placed secrets are removed by `secret::place::clear`). |
| `layout.rs` | Every path lodge writes inside a container: `Layout`, the paths inside the user's prefix (the table under "What lives where"); the provisioning state, which is the devcontainer's own and root-owned; and the secrets directory. Nothing else spells these paths out. |
| `secret/` | Secrets that must not be in a bundle. Their `[[secret]]` declarations are config (`config/secret.rs`: validated; names reach shell scripts). `mod` is the `Secret` value type (redacted `Debug`, no `Display`), matching `targets` globs, and `resolve` (env, file or a command run on *this* machine with a timeout, with errors that never contain the value). `place` writes values into a container under `/dev/shm/lodge-secrets-<uid>/` and builds the session lines that export them; `clear` removes the user's (`ldg clean`). `deliver` picks the secrets whose `targets` match, resolves and places them (or clears them when none are declared), and prints names only. |
| `session/` | Opens the interactive session: `docker exec -it`, and nothing around it; after a lost `--remote` connection it reconnects by itself (tries for about two and a half minutes) and opens a new shell, without asking. Its start-up script (`startup_script`) starts the shell as a login shell, so the profile scripts run, then puts the user's prefix `bin/` first on `PATH` and restores what the profile dropped before running the shell interactively; that is how the injected tools reach the session without any file outside the prefix. `terminfo` makes the terminal the session is opened from work in the container (copies its entry from this machine into the prefix when the container lacks it, and sets `TERM`). |
| `forward/` | Port forwarding. `mod` has the `ForwardDaemon` trait (`start`, `stop`; `DetachedDaemon` spawns the daemon, and `stop` leaves it a note to end, which it looks for between polls; a test records instead) and owns its state files (log, lock, and the ports file `ldg ports` reads, which starts with the daemon's pid so a dead daemon's file is ignored), `daemon` is the `_forward` loop (declared ports plus whatever starts listening), `tunnel` one forward behind the `Forward` and `Forwarders` traits: `HostForwarders` starts the tunnel process the `Host` provides (`ssh -L` for `SshHost`) or else the in-process TCP proxy; the daemon's start, restart, re-point and cooldown logic is tested with a fake `Forwarders`. The daemon decodes its own `--port` flags, re-points a forward when its destination's address changes (looked up again every 30 s), and exits when the container stops, not when the engine merely can't be reached for a while, or when `ldg clean` asks it to (a machine is always up, so nothing else ends the daemon for a plain host). `up` starts none for an endpoint that says it has nothing to forward (`Container::forwards_ports`: this machine as a plain host). |
| `history.rs` | The places a session was opened in, most recent first, for a bare `ldg`: an `Entry` is a `Place` (a project folder, or a container `ldg attach` worked in, with its `--workdir`), the `Route` it was reached by (the target and the `--remote` host) and a name (the devcontainer's `name`, else the folder or container name). It records no flags of a single run (`--build`, `--reinject`, `--bundle`), so resuming is the same as typing the command again, and `up` still wakes a stopped container. `History` is the seam (`entries`, `remember`; `Memory` in tests); `FileHistory` keeps the list in `~/.local/share/lodge/history.json`, written whole through a temporary file and a rename, readable by the user only (it names folders and hosts), at most 50 entries, a repeat moving to the front instead of adding one. Recording happens once a session is about to open, and failing to record is a warning, never an error. |
| `provision.rs` | `TargetKind` (one kind of endpoint the user can name: its name, its one-line description, and how to build its `Endpoints` from the `Wiring` and the `[targets.<name>]` settings it was given; the whole of what is particular to a target outside its endpoint) and the seam between making an endpoint and working in one: `Endpoints` (provision a project's endpoint, find it, its status, hand out a handle by id for the forward daemon, which engine is behind it, whether `--remote` needs a project path, and an optional `Lifecycle`) returns `Attached` (the endpoint, the name it calls itself, its user, workdir, forward ports, whether it was just created, and the post-attach hook). `cmd::attach::up` is `provision` then `attach`, and `attach` takes only an `Attached`, so it is tested with a fake container and no provisioner. |
| `plain_host.rs` | A machine you can reach as an endpoint (`--target host`): `PlainHost` is an `Env` and `Container` built on a `Host` only (commands and `tar` through `Host::exec`, secrets on stdin, the session through `Host::interactive`), acting as the user the host connects as and not [disposable](#capabilities); `PlainHostEndpoints` is its `Endpoints`: nothing to create, so it has no `Lifecycle` (`down`, `rm` and `ls` say so). |
| `port.rs` | `PortForward`, a port to forward: what the devcontainer spec declares and the daemon carries out, so neither depends on the other. |
| `tools/` | The build-time helpers behind interfaces: `DepReader` (which libraries a binary needs), `ElfEditor` (relink a copy of a binary for the bundle's prefix) and `OciFetcher` (pull an artifact's layers from a registry), bundled as a `Toolchain` that `cmd::run` builds. `external` runs the programs (`Ldd`, `Patchelf`, parsing `ldd`'s output); `oci/` is the in-process registry client (see [`tools/oci/`](#toolsoci)). Packing, the bundle cache and feature fetching take the interfaces and are tested with fakes. |
| `host_tools.rs` | The helper programs lodge may run here (`ssh`, `ldd`, `patchelf`): what each is needed for (a `Need`, which decides when `doctor` requires it) and how to get it. `doctor` reports from this table and each tool is started only through it (the `tools` implementations and `host/ssh.rs`). |
| `local.rs` | `Local`: the machine lodge runs on (environment variables, files a secret names, a local shell for a secret's `command`, a program fed some input, as a credential helper is), with `SystemLocal` and, in tests, `FakeLocal`. Built once in `cmd/` and handed down, so nothing else asks the process. |
| `host/` | How a machine is reached. `mod` is the `Host` trait (`run` one `Job` with the streams the caller asks for, `interactive`, `tunnel`, the connection hooks; no process type crosses it) with everything built on those provided (`exec` runs a job and retries it after a lost connection if it is `Repeat::Safe`): running a script, `read_first` (finds files by pattern and reads the first in one round trip), `read_file`, `canonical_dir`, `fetch_dir` (copies a folder from the project's host), `write_scratch`, `uid_gid`; and `attempt`, which reruns a step after a lost connection if it is marked `Repeat::Safe`. `fake` (tests) is `FakeHost`: it records every job and answers from a script, with files in memory, so everything that only uses a host is tested without a process. `local` is `LocalHost`, this machine. `ssh` is `SshHost`: `--remote` destination parsing, the shared connection (`connect` opens it up front with the terminal available, `ControlMaster` multiplexes everything after), and the few ssh invocations lodge needs, all built from one private `base`. |
| `runtime/` | The seam to whatever runs the workload: the `Env`, `Container`, `Runtime` and `Compose` traits (see below), `User` (who a command runs as) and `ContainerUser` (the session user's name, uid, gid and home, looked up once), `Acting` (an `Env` seen as one user, below), `inside` (`Inside`: what is particular to reaching inside one endpoint, five small methods, and the `Env` every endpoint needs built on it once), `tarball` (packs files as a tar owned by a user), the docker-compatible CLI implementation (`cli/`, for `docker` and `podman`: the engine, which runs on a `Host` it is given, the per-container `Inside` and the copy out of a container; `Docker` and `Podman` are two engines, each a `CliEngine` of its own `Flavor` (`docker.rs`, `podman.rs`), which holds what is its own: the program, the `[targets.docker]`/`[targets.podman]` settings (a context or connection, as arguments on every command), `cp`'s flags and wording, extra `run` flags, how compose is asked, and whether a container's address is reachable (not for rootless Podman). The plumbing is shared and asks the flavor; it never checks which engine it is), and strict in-memory fakes for tests (`FakeEnv`, and `FakeRuntime`, whose containers each run in one). |
| `shell.rs` | POSIX `shell_quote`, which everything that builds a command string goes through; `script_argv`, how every script with arguments is started (`sh -c <script> sh <args…>`, so arguments are `$1`, `$2`, …); the rules for names written into scripts unquoted (`is_plain_name`, `is_var_name`) and for destinations in a container (`container_path_problem`); `slug` for container names; and `CmdLine`, a command as `sh -c` script or argv. |
| `arch.rs` | `Arch`, the one name for a CPU that bundles, binaries, machines and registry platforms are compared by. A foundation module, so `tools/` (the registry client picks an index entry by it) and `bundle/` can both use it. |
| `fingerprint.rs` | The small content hash used to notice changes (devcontainer inputs, updated libraries, bundle contents), as a `Fingerprint` builder. Its values are stored, so a test pins them. |
| `tmpfile.rs` | Self-deleting staging files for `docker cp`. |
| `tree.rs` | Walking a file tree on this machine in one stable order (name order, a directory before its contents), with an explicit choice about symlinks (a followed link loop is entered once) and unreadable directories. Every hash, fingerprint and copy of a tree goes through it. |
| `output.rs` | Everything lodge says (`info!`, `note!`, `warning!`, `error!`, `prompt!`, `line!` for reports, and `line_err!`, `scoped!`, `raw_stdout`, `raw_stderr` for the special cases), the `[lodge]` prefix on every line, `plural`, and the verbosity setting with command tracing (`trace`, `trace_line`). |
| `conventions.rs`, `doc_facts.rs` | Test-only: architecture rules (one place per external program, only `cmd/` chooses implementations, the direction of dependencies), and facts the docs state checked against the code. |

### `devcontainer/`

`start()` in `mod.rs` is the orchestration, behind the `Devcontainer` provisioner; each step is its own module.

| File | Responsibility |
| --- | --- |
| `mod.rs` | `Devcontainer` (the `Endpoints` over an engine: `start` is its orchestration, and returns an `Attached`), `stop` / `remove` / `status` / `find`, and `Prompt`, the questions only a person can answer (the command layer implements it). Drift and the installed bundle are read with `Container::read_stored_file`, which works on a stopped container too. |
| `spec.rs` | Find the config on the project's host (through `Host`, so every host shares one lookup order); parse `devcontainer.json` (JSONC) into a backend-agnostic `Spec`; derive the session name. |
| `backend.rs` | Picks and builds the backend from the parsed config (with the stable container name, and the files besides the config that count toward drift: a Dockerfile, compose files). The `Backend` trait with two implementations: `ComposeBackend` (`docker compose`, run on the project's host) and `ContainerBackend` (`docker run` of an image or a local build). Both label the container `dev.lodge.project=<path>`, and `devcontainer.local_folder` / `devcontainer.config_file` as the devcontainer CLI does (`editor_labels`). `devcontainer/mod.rs` finds a container by lodge's own name (or compose project) first, and only then by those two labels (`Runtime::find_by_labels`): that one is `foreign`, used as it is by `up` (started, injected into, no features or hooks) and never stopped or removed. |
| `create.rs` | `CreateSettings`, how a container is created (privileged, capabilities, mounts, published `appPort`s, …), from devcontainer.json and from features alike, applied to a `runtime::RunSpec` (the engine-neutral description of a container; `runtime/cli` renders it as `docker run` flags, and a devcontainer's `runArgs` ride along in `engine_args`, last, because the spec defines them as the engine's own flags) or rendered as a compose override (compose-file YAML, which is not docker's). |
| `features/` | Devcontainer *features*: `fetch` (OCI through the `OciFetcher`, or a local folder from the project's host; a feature's `id` must be a plain name, since it names a folder), and in `mod.rs` `resolve` (adds what `dependsOn` names, once each, and drops a feature whose dependency is unavailable), the order (`dependsOn` and `installsAfter`, then `overrideFeatureInstallOrder`, then the reference; a circle is warned about), the creation settings they ask for, and the install (as root, told `_REMOTE_USER`), with one env profile per feature. |
| `lifecycle.rs` | The `Hook` type (string / argv / named map, all run as the same steps), running hooks on the host or in the container (as `remoteUser`), and the create-time hooks that run once successfully (`run_create_hooks`). |
| `profile.rs` | The env profiles written into the container (`containerEnv`/`remoteEnv`, and what features add): literals quoted, variable references left for the login shell. |
| `markers.rs` | One-shot markers: which create-time hooks and feature installs already succeeded. |
| `drift.rs` | Drift detection: fingerprint what the container is built from, and nothing else: the config file, the files the backend names (a Dockerfile, compose files) and each local feature folder (`Spec::hash`). Other files under `.devcontainer/` are not inputs, so a hook writing there changes nothing, and neither does the Dockerfile's build context. Compare with the value stored in the container. `config_state` is the one comparison, used by `ldg up` and `ldg status` alike: `UpToDate`, `Changed`, or `Unknown` when no hash is recorded or it can't be read, which is not drift. The files are read through `Host` (`fetch_dir`, `read_file`), so a remote project is hashed exactly like a local one. |
| `workdir.rs` | The directory a session lands in: `workspaceFolder`, or one the person picks when it doesn't exist. |
| `uid.rs` | Lines the container user's uid/gid up with the host user's so a bind-mounted workspace is writable. Worked out in Rust from `/etc/passwd`, applied by a small script. |
| `vars.rs` | `${localEnv:X:default}`, `${containerWorkspaceFolder}`, `${devcontainerId}` and friends, and `path_tag`, the short hash of the project path behind `${devcontainerId}` and the container and session names. |

The `Backend` split is what keeps `start()` simple: a backend only knows how to
find an existing container and how to create or start one. `up` is handed what
`existing` found, so the image backend doesn't look again; compose finds its own
container (and is asked for its id after `up` anyway). Hashing, env, features
and hooks all work from the resulting container id.

### `tools/oci/`

`NativeOci` talks to a registry itself, with no helper program, and implements
two separate interfaces: `OciFetcher` (read) and `OciPusher` (write). It knows
registries and nothing about what an artifact holds, so devcontainer features and
bundles use it unchanged. `pull(reference, arch, into)` resolves the reference,
picks the image for `arch` out of an index, and writes each layer to `into` as a
file named after its digest. `pin(reference, arch)` names what a tag points at
right now by digest, which is how the bundle cache avoids a download. `push`
publishes an `Artifact` (one layer from a file, with the types that say what it
is) under a tag.

| File | Responsibility |
| --- | --- |
| `mod.rs` | `NativeOci`: the order of a pull (manifest, then the image an index names, then each layer) and of a push (blobs, then the manifest). Built from the two interfaces below, by `cmd::run`, and handed to what reads and to what writes as the same value. |
| `http.rs` | `Http`, the transport: one request (a method, headers, and a body that is nothing, bytes or a file, so it can be sent again) and one answer, any status returned, redirects followed without ever sending the request's headers to another host. `UreqHttp` is the real one and the only file that names the HTTP crate; `FakeHttp` (tests) answers each URL from a script and records what it was sent. |
| `credentials.rs` | `Credentials`, where a login comes from. `DockerConfig` reads what `docker login` stored in the config file (`$DOCKER_CONFIG` or `~/.docker`), through `Local`, and asks the credential helper the file names (`credHelpers` for the registry, else `credsStore`) as docker does: `docker-credential-<name> get`, the server on stdin, through `Local::run_program`. A helper with nothing for the registry leaves the file's own entry to answer; one that fails is an error that names it; an identity token is refused. `Login` never prints its password. |
| `registry.rs` | One repository on one registry, for a pull or a push (`Access`, which decides the scope a token is asked for): the calls, and the handshake that lets them through. |
| `reference.rs` | Splits `ghcr.io/owner/name:1` (or `@sha256:…`) into registry, repository and tag or digest, and refuses anything that would break a URL. Pure. |
| `manifest.rs` | Reads a manifest as an image (layers) or an index (images per platform), picks the entry for an architecture, writes an image manifest, and merges one more image into a tag's index (same input, same bytes). Pure. |
| `auth.rs` | Reads a `WWW-Authenticate` challenge and builds what answers it. Pure. |
| `digest.rs` | `Digest`, the digest of a file, and copying a stream while checking its size and hash. The only file that names the hash crate. |

How a pull stays safe:

- **Everything is checked.** A manifest asked for by digest must hash to it, and a
  layer must be exactly the size the manifest declares and hash to its digest.
  Nothing past the declared size is read, and a download that fails a check leaves
  no file behind.
- **The handshake.** A 401 is answered once: with a bearer token (the scope the
  registry names, else a pull of this repository) fetched from the token service,
  or with basic auth, using a login if there is one and anonymously if not. A
  token that goes stale mid-pull is renewed the same way; a second 401 is the
  registry's answer, and the error says how a login is found.
- **Logins stay put.** A login is only sent to a token service over TLS, and never
  to another host when a request is redirected (a registry sends a blob to a CDN).
- **Local registries are spoken to without TLS**, as docker does: `localhost`,
  `127.0.0.1` and `[::1]`.
- **A stall is an error, not a hang.** There is a timeout for connecting, for the
  response and for each read, and none for the whole download: a blob can be
  large. A write is given longer for the registry's answer, growing with what was
  sent (two minutes, a second more per 8 MiB, half an hour at most), because it
  verifies and commits a large blob before it replies.
- **"Not now" is tried again.** A 429, 502, 503 or 504 (and a plain 500 for a read,
  which cannot have done anything) is sent again after 1, 3 and 9 seconds, or as
  long as its `Retry-After` says when that is at most 30 s; after that the status
  is the answer. A network failure is not retried here (an upload's hang-up has
  its own retry, below).
- **Proxies** are the HTTP crate's: `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and
  `NO_PROXY` are honoured, as `curl` does.
- **The older docker manifest types** (`manifest.v2`, `manifest.list.v2`) are
  asked for and read as the OCI ones; a schema 1 manifest is an error.

How a push stays safe, and what it does:

- **The blobs first, the manifest last**, so a manifest never names what is not
  there. A blob the registry already has (`HEAD`) is not sent again. The empty
  config and the layer are uploaded in one piece (`POST` for a place, `PUT` with
  the digest); the layer is streamed from its file. A registry (or a proxy in
  front of it) that answers 413 to that gets the blob again in pieces of 32 MiB
  (`PATCH` with a `Content-Range`, then an empty `PUT` that closes the upload).
- **Writing is asked for as docker asks**: the token is requested for `pull,push`
  whatever scope the registry's challenge names. A refusal says to `docker login`
  with an account that may write.
- **The upload goes where the registry says, and only there.** The `Location` it
  answers with (absolute or relative) must name the same host and port, because the
  token is sent along; the URL is rebuilt with the scheme lodge chose, so a proxy
  that writes `http://` in its answers is still spoken to over TLS.
- **A hang-up during an upload is tried once more**, from the start and with a new
  handshake: a registry that refuses a write (a stale token) answers at once and
  closes, which the sender sees as a broken connection and not as a 401.
- **What was stored is checked.** A registry that reports a digest for what it
  stored must report the one that was sent.
- **The tag holds an index, one image per architecture.** A push writes the blobs,
  then the image by its digest, then reads what the tag holds and writes the index
  back with this architecture's entry replaced in place (or added) and every other
  entry kept as it was read. A tag that held one image, or nothing, becomes an
  index of one; one that cannot be read is an error, never overwritten. The tag is
  read as late as possible, but last write still wins when two machines publish at
  once. What a push returns is the digest of the image for this machine.

A push can refuse to replace: with `--no-clobber` the tag is read first, and an
entry for this architecture (or a single image) already there is an error before
anything is uploaded.

### `bundle/store/`

A bundle is named by a *reference* the user wrote (`--bundle`, `bundle =`,
`pack -o`), and a kind of place it can be kept has two halves, each its own
interface:

- a `BundleStore` reads from it, and
- a `BundleSink` writes to it.

They are separate so a machine that only uses bundles never needs a way to write
one, and a fake of one does not have to implement the other. `bundle::source`
hands a reference to `Stores::find`, `ldg pack` hands its output to `Sinks::find`;
each returns the one that `handles` it and never names a concrete type.
`cmd::run` builds both lists (`bundle_stores`, `bundle_sinks`), so a new place (an
`https://` URL, a bucket) is a type per direction and one line in each. A reference
without a scheme is a file; with one, it belongs to whoever reads that scheme, and
a scheme nobody reads is an error that lists the forms that work. Two claiming a
reference is a wiring error, reported and not resolved by order.

Reading takes two calls, so the cache can skip the costly one: `identify` is cheap
and names the content as it is now (a `key` the cache uses as its directory name,
and the `pinned` reference to fetch), `fetch` returns the tar. Writing is one:
`put` writes the tar and returns the exact reference the bundle can be used by.

| Place | Reference | `identify` | `fetch` | `put` |
| --- | --- | --- | --- | --- |
| `FileStore`, `FileSink` | no scheme: a path | `f-<hash of the file>` | the file itself, nothing copied | copies beside the destination, then renames: the file appears only when whole |
| `OciStore`, `OciSink` | `oci://registry/name[:tag\|@digest]` (a sink wants a tag) | `OciFetcher::pin`: one manifest request, `o-<digest of the image for this machine>` | `OciFetcher::pull` of the pinned reference; one layer, which is the tar | `OciPusher::push` (given the bundle's architecture) as one layer typed as a lodge bundle, into the tag's index; returns `oci://registry/name@sha256:…` |
| `FakeStore`, `FakeSink` (tests) | `fake://name` | a scripted key | a scripted tar, recording what was fetched | keeps what it is given |

`OciStore` and `OciSink` are built on `OciFetcher` and `OciPusher` and know nothing
of HTTP or registries; the file ones know nothing of the cache. What comes out of
a store is a tar that `archive::extract` unpacks and checks, so a layer that is not
a bundle fails there, whoever published it. `pack` builds the tar in a scratch
directory and hands it to the sink, so a destination nobody writes to is refused
before anything is packed, and a sink says where the tar is best built
(`staging`): a file's is beside the destination, so placing it is a rename and a
large bundle is not held in the scratch space as well. *Enforced:* `conventions.rs` allows the concrete stores
and sinks to be named only in `bundle/store/` and `cmd/`, and the `oci://` scheme to
be spelled only in `oci.rs`.

## What `ldg up` does, step by step

`up` is two halves. **Provision** (steps 1–9) makes the endpoint ready and hands
back an `Attached`: the container plus the user, working directory, forward ports,
whether it was just created, and the post-attach hook. **Attach** (steps 10–11)
works in whatever it is given and knows nothing of how it was made. Today the
only provisioner is the devcontainer one; a pod or a plain host would be another
provisioner feeding the same attach half.

0. **Load the config and ready the bundle** (before both halves) (`cmd::attach`), so a broken config or
   bundle fails before anything starts. The project path is made canonical on
   the project's host (`Host::canonical_dir`; on another machine, `pwd -P`
   there), so one folder is one container however it is spelled.
1. **Load the spec.** Find and parse `devcontainer.json`; pick a backend.
2. **Detect drift.** Fingerprint the inputs (see `drift.rs`) and compare with the hash stored in
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
10. **Attach starts here.** **Look up the container user** (`remoteUser`, or the default user: name,
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
host's own files. The directory comes from a bundle reference (`--bundle`, or
`bundle =` in the config: a file, a registry; see `bundle/store/`), identified,
fetched and extracted once and cached by what it identified as (a file's hash, an
image's digest), or from a live pack of
the `[[inject]]` entries, cached by a fingerprint of the config and of each source
file's size and mtime and the libraries `ldd` finds. Working out that fingerprint
runs `ldd` (so a live config needs it on every `up`); if nothing changed, the
cached directory is reused and nothing is copied or relinked (`patchelf` does
not run). With a stored bundle neither tool is needed. The live path and the
stored-bundle path end in the same place, so there is one injector. It is prepared
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
| `<prefix>/injected` | The id of the bundle last injected, and the wrappers, configs and git `safe.directory` entries placed outside the prefix. `ldg status` shows the id, `ldg clean` removes the wrappers and the `safe.directory` entries lodge added (never one that was already there) and reports the configs, and its presence is how both find the prefix. |
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
| `~/.gitconfig` (`safe.directory`) | The entries the bundle lists; `ldg clean` unsets the ones lodge added. |

Because all state is *in* the container, it survives reboots and disappears
when the container is recreated, with no host bookkeeping to go stale. Every
container lodge creates also carries the label `dev.lodge.project=<path>`.
`ldg clean` removes the injection (the prefix, the wrappers it wrote, the
`safe.directory` entries it added and the user's secrets) and keeps the devcontainer's state (markers, hash, `containerEnv`).

**On the host**

| Path | Contents |
| --- | --- |
| `~/.config/lodge/config.toml` | Your configuration. |
| `~/.cache/lodge/bundles/` | Extracted or packed bundles, the four most recently used. |
| `~/.local/share/lodge/forward-<id>.log`, `.lock`, `.ports`, `.stop` | Log of the port-forward daemon (started afresh by each daemon that takes over), the lock that keeps it to one per container, its active forwards (for `ldg ports`, replaced whole on every change), and the note `ldg clean` leaves for it to end (taken away when answered). |
| `$TMPDIR/lodge-ssh/*.sock` | SSH `ControlMaster` sockets (`--remote` only). |
| `$TMPDIR/lodge-<pid>-*` | Short-lived staging files and folders, removed when done. Each is created new and private to the user (0600 / 0700), never opened if the name is already taken. On a remote host the compose override comes from `mktemp`. |

## Remote mode

With `--remote <dest>`, `cmd::run` builds an `SshHost` instead of a `LocalHost`
and the docker CLI runs *on the remote host* over SSH, so no local Docker is
needed. Nothing but `host/ssh.rs` knows that; this section is what `SshHost`
does behind the `Host` trait. Four things make it work:

- `CliEngine` hands its host a `Job` that runs `docker …` (`Host::exec`), and
  `SshHost` answers with `ssh <dest> …`, shell-quoting each argument. Anything
  that reads **files** (compose projects, build contexts) goes through
  `CliEngine::job_in`, which gives the job the project directory too, so
  `SshHost` `cd`s into it *on the remote*.
- **Connection reuse.** A single `ldg up` issues dozens of SSH calls.
  `Host::connect` opens one connection first (a password or 2FA prompt is
  answered once, with the terminal available) and every later call is
  multiplexed over its `ControlMaster` socket, one per destination, with
  `ServerAliveInterval=15` so a dead link fails in about 45 s instead of
  hanging. A tunnel's forward lives in that connection, not in the tunnel's
  process, so `SshHost` hands out a tunnel that also cancels its forward
  (`ssh -O cancel`) when it is stopped; otherwise the local port stays bound
  until the connection ends.
- **Lost connections and repeat-safety.** ssh exiting 255 says the connection
  failed, not whether the remote command ran, so a step is rerun only if it is
  safe to run twice. Every remote step says which it is: `Repeat::Safe` (reads
  and probes, `mkdir -p`, `rm -rf`, the tar copy, an atomic secret write,
  `start`/`stop`/`rm -f`/`build`/`compose up|down`) or `Repeat::Never`, the
  default (user hooks, a feature's `install.sh`, the uid change,
  `initializeCommand`, scratch files, the interactive session). Exec steps set it
  with `ExecOpts::repeatable()`, everything else goes through
  `Host::exec` (`attempt` for a step built by hand). The host decides what counts as a lost connection
  (`Host::link_lost`: for `SshHost` exit 255, unless ssh says the login or host
  key was refused), how often to retry and how to reset (`retry_policy`,
  `reset_link`); a safe step is retried up to three times, waiting 1 s then
  2 s, after the dead master is dropped. A host whose connection cannot be lost
  (`LocalHost`) never retries. `docker run` is not repeatable as such, so a retry first looks for the
  container by name and starts what it finds. The forward daemon restarts a dead
  tunnel with a wait that doubles from 2 s to a minute. It has no
  terminal, so its tunnels never prompt: they ride on the connection `connect`
  opened, or on key or agent auth (`SshHost::unattended`, which `cmd::run`
  gives the daemon, adds `BatchMode` to every command, not only the tunnels).
  When that connection is gone and the host
  wants a login only a person can give (`Host::awaiting_login`: for `SshHost` a
  `BatchMode` attempt refused with "Permission denied"), the daemon does not
  give up after its five minutes of failed polls (`--unreachable-polls`, a
  setting of the hidden `_forward` command); it logs why once and polls every 20 s
  until any command with a terminal runs `connect` again. A daemon that was
  waiting rides out two checks that find nothing before it concludes the host is
  gone, since one check can fail for its own reasons.
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
| `Container: Env` | `Env` that can also be inspected (name, mounts, `reach`, running: `Result<bool>`, so "stopped" and "couldn't ask" differ) and read while stopped (`read_stored_file`, through `docker cp`) | port forwarding, workdir selection, the session, drift, `ldg status` |
| `Runtime` | container engine: find, build (`BuildSpec`), run (`RunSpec`), start, stop | devcontainer backends, `start`/`stop` |
| `Compose` | an engine's compose support (`Runtime::compose()`, an error where there is none): up, down, ps | the compose backend, `doctor` |

`Env` carries the primitives (`exec`, `output`, `put`, plus `write_secret` and
`interactive`, which need stdin and TTY handling). An endpoint reached by running
commands on a `Host` does not write them: it implements `Inside` (the host, the job
that runs a command as a user, the job that unpacks a tar from stdin, a session, and
whether it is disposable) and `runtime::inside` provides the `Env`: the user check,
the retry marking, secrets on stdin and never in argv, the tar packed in-process.
A container and a plain host are each about forty lines of `Inside`. `Inside` is the convenience for endpoints reached by running commands on a `Host`, not the seam: that is `Env`, and an endpoint reached some other way (an API client) implements `Env` itself, as the in-memory `FakeEnv` does. `Env` then
provides everything else on top of the primitives: `write_file`, `read_file`, `mkdir_p`,
`exists`, and so on. So code that injects a tool takes one `&dyn Env`
instead of a `(docker, container)` pair, and anything new that implements the
primitives gets the rest for free. The traits know nothing of lodge's own
layout: markers, hashes and profiles live in the modules that use them. An
`Env` doesn't say whether it is local or remote either; code that needs to
know (matching a secret's `targets`, diagnosing a refused PTY) is handed the
`Host` by the command and asks it (`names`, `pty_denied`).

**Capabilities.** Generic code never asks an engine's name; it asks the
endpoint, through methods with a default that an implementation may override:

| Question | Where | Docker's answer (a plain host's differs where noted) |
|---|---|---|
| Can this user write and run a program in a directory? (`inject/prefix` picks the home, else a private `/tmp` directory) | `Env::can_run_in`, by default a probe script run as that user | the default |
| How do I reach the container's ports, or a peer's, from the engine's host? (the forwarder) | `Container::reach` -> `Reach`: `Address` (an IP on the container network, every port), `Published` (a rootless engine: only the published ports, each at the address the host has for it; `destination(local, port)` is `None` for the rest, which the forwarder says once and skips) or `Relayed` (a way in that is a process: the forwarder first asks `Container::relay` to start it on the engine's host, listening on loopback at the local port, and forwards to that; the relay lives and dies with its forward, and one aimed at another container port is a new one). Either way the forwarder uses the host's tunnel or the built-in proxy, and does not proxy a port over itself when it is already on this machine | its first network's IP; a peer by container DNS, else by network (rootless Podman: its published ports); a pod: `Relayed` by `kubectl port-forward`, no peers |
| How do files get in? | `Env::put`, below | an in-process tar to `docker cp`; a plain host feeds the same tar to its own `tar -x` |
| Is this place thrown away, so replacing what is there is fine? (injection replaces a config lodge did not place, sets git's `safe.directory = *`, and does not check the OS) | `Env::is_disposable`, default `true` | `true`; **a plain host and a pod answer `false`**: a config it did not place is left alone, the `*` wildcard is not set, and `uname -s` must say Linux |

Uid mapping and mount labels are decided when a container is *provisioned*
(`Runtime::run_detached` arguments, `PodmanFlavor`, `devcontainer/uid.rs`); no
generic code reads them, and a query with no caller would be a guess.

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

`runtime::cli` implements all four over the `docker` CLI, run on whichever
`Host` it is given. `runtime::fake::FakeEnv` is an in-memory `Env` that
records commands and keeps a virtual filesystem, which is what the injection
tests run against; `FakeRuntime` is an engine (with compose) that keeps its
containers by name, each in its own `FakeEnv`, and refuses to act on one that
doesn't exist, which is what `start`'s create / reuse / recreate decisions are
tested against. Another engine, or a plain host or a pod with no engine at all
(`plain_host.rs`, `kube.rs`: each an `Inside` that spells how to run, unpack and open
a terminal, and the same `LoginOnly` rule that only the user it connects as can be
acted as), implement the same traits.

Backends (`devcontainer/backend.rs`) are plain data: the `Runtime` is passed in
rather than stored.

## Choices and how they are wired

lodge can vary in a few places: how a machine is reached, which engine runs the
containers, how an endpoint is made ready, which helper tools build a bundle. Each
is a choice behind a small interface, so a new option (Podman, a pod, a
different ssh) is one new type plus one line where the choice is made, and
options add up side by side instead of tangling into the code that uses them.
Tests use the same seam to swap in fakes. That is the point; the wiring below is
just how it is done.

| Dependency | Interface | Implementations |
| --- | --- | --- |
| How a machine is reached | `host::Host` | `LocalHost`, `SshHost`, and `FakeHost` for tests; anything else that can run a command implements `run`, `interactive`, `tunnel` and `spawn` |
| The machine lodge runs on | `local::Local` | `SystemLocal` (environment, files a secret names, a local shell); `FakeLocal` for tests |
| The container engine | `runtime::Runtime`, `Compose`, `Container`, `Env` | `runtime::cli`, and the in-memory `FakeRuntime` for tests |
| The build-time helpers | `tools::DepReader`, `ElfEditor`, `OciFetcher` (a `Toolchain`) | `Ldd`, `Patchelf`, `NativeOci` (on `Http` and `Credentials`: `UreqHttp`, `DockerConfig`); a test passes fakes |
| Making an endpoint ready, and finding it again | `provision::Endpoints` | `devcontainer::Devcontainer` (a devcontainer on an engine), `plain_host::PlainHostEndpoints` (the machine itself), `kube::KubeEndpoints` (a running pod, found by name or by a label selector, through `kubectl` on a `Host`). An endpoint also says what `doctor` should check about it (`Endpoints::checks`: for a pod, `kubectl` and that the pod is there), so `doctor` never asks which kind it is |
| Port forwarding for a container | `forward::ForwardDaemon` (and `Forwarders` for one tunnel) | `DetachedDaemon`, `HostForwarders`; a test passes fakes |
| Where a bundle comes from, and goes to | `bundle::store::BundleStore` and `BundleSink` (the `Stores` and `Sinks` in `Ctx`) | `FileStore`/`FileSink`, `OciStore`/`OciSink` (on `OciFetcher` and `OciPusher`); fakes for tests; a new place is a type per direction and a line in `cmd::bundle_stores` and `bundle_sinks` |
| What is injected | `[[inject]]` data | multiplexers and other tools are injected like any other, never named in code |

The pieces compose instead of branching. The engine is built *on* a host
(`Docker::new(host)`, `Podman::new(host)`), so "docker over ssh" is the same engine with a different
host, not a third case; a remote Podman would be a second engine on the same
host. `Host` has four primitives, `run` (one `Job`: the command, its directory, what
it reads on stdin and what happens to its output and errors), `interactive` (a
command with a terminal), `tunnel` (`ssh -L`) and `spawn` (a process that keeps
running until killed, which is what a relay is; over ssh it gets a terminal so it
hangs up with the connection), plus the connection hooks (`connect`,
`link_lost`, `reset_link`, `retry_policy`, `pty_denied`); everything else (`sh_output`, `run_in`, `canonical_dir`, `read_first`,
`read_file`, `fetch_dir`, `write_scratch`, `remove_file`, `uid_gid`) is provided on
top of them, so an implementation gets it by writing the primitives and
overrides only what it can do better (`LocalHost` reads files directly). The
compose backend writes its temporary override with `write_scratch`, whose guard
removes it again.

`--remote` and `--target` (or `target` in the config, `choose_target`) are read once, in
`cmd::run`, which is the composition root: the only place that picks `LocalHost` or `SshHost` and, with
`build_endpoints`, the `TargetKind` it finds by name in `cmd/targets.rs`, which builds the endpoints on it, then puts both in the `Ctx`, together with
the `Toolchain` of helper tools and the `Local` machine it builds. A child lodge
(the forward daemon) is pointed at the same pair by `cmd::respawn_args`: the
command line is the command layer's to know, not the host's or the engine's. From there on nothing handles a bare `Option<String>`
or matches on a kind of host. The command hands `&dyn Host` (or `HostRef`, where a
backend keeps one) to whatever needs it (`devcontainer`, `forward`, `secret`,
`session`) and the `Runtime` as `&dyn Runtime`. What differs between hosts is
asked as a capability, not by name: `shares_cwd` (may the project path be left
out), `names` (what a secret's `targets` match), `tunnel` (the process that forwards a port, or
`None` when the engine's addresses are reachable from here and the in-process
proxy is used).

Provisioning and attaching meet at `provision::Attached`, plain data holding the
endpoint, its user, workdir, forward ports, whether it was just created and the
post-attach hook. `cmd::attach::up` calls `ctx.endpoints.provision`;
`attach` takes only the `Attached`, the `Ctx` and the bundle. Because of that,
`attach` is tested with a fake container and a recording `ForwardDaemon`, with no
devcontainer involved. Which `Endpoints` a run uses is chosen once, in
`cmd::build_endpoints`, from `--target`: `docker` and `podman` build a
`Devcontainer` on that engine, `host` builds `PlainHostEndpoints`, `kubernetes`
builds `KubeEndpoints`; every command
that is about the project's container (`up`, `down`, `status`, `rm`, `ls`,
`ports`, `logs`, `clean`, the forward daemon) asks the `Endpoints`, so none of them
knows which it is.

**What is not a choice yet.** Some things are still used directly, so there is
nothing to swap or fake there:
- the interactive session's local side (`session/` asks this process whether it
  has a terminal, to decide whether to reconnect);
- this machine's own files and standard directories (the bundle cache, temp
  files, loading the config, finding `ldg`'s own executable to start the forward
  daemon). `conventions.rs` lists which files still reach for the process itself,
  and the list may only shrink. The environment, the files a secret names, a
  secret's `command` and `${localEnv:…}` go through `local::Local`.

Making one of these a choice follows the same recipe as the others.

To add a way of reaching machines (an ssh successor, `gh codespace ssh`, an
agent), implement `Host` in a new file under `host/` and add one arm to the
choice in `cmd::run`. To add a kind of endpoint (a Podman-like engine, a pod, a Codespace), implement
`Endpoints`, `Inside` and `Container` for what it hands back (and `Lifecycle` only if
lodge creates and can throw away what it hands back), then add **one struct and one
line** in `cmd/targets.rs`: a `TargetKind` with its name, its settings (a serde
struct that refuses unknown keys, read from `[targets.<name>]`, with its schema for
`docs/config.schema.json`), its `build`, and what it is made of (`made_of`: its
concrete types and the modules that may name them, which `conventions.rs` turns into
a guard, so a new target cannot leave itself unguarded). That list is the only place that says
which targets exist: the `--target` values and their help, the `target` in the
config, the settings tables and the schema are all read from it, and `attach` and
every other command are unchanged. To replace a helper tool (a built-in ELF
writer, another registry client), implement `OciFetcher` or `ElfEditor` in
`tools/` and swap it in the `Toolchain` that `cmd::run` builds. Nothing else changes, and
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
  to ssh, whose debug output goes to a file beside the connection's socket (`-E`), because
  a background master would keep a captured terminal stream open. Reports and questions belong to `cmd/`. *Enforced:* clippy denies
  `println!` and friends outside `output.rs`.
- **Every dependency is injected.** The way a machine is reached is a `Host`, the
  engine a `Runtime`, and only `cmd/` (the composition root, `cmd::run`) names a
  concrete one (`SshHost`, `LocalHost`, `CliEngine`); everything else takes the
  interface. No code matches on a kind of host or engine or compares one's name:
  it asks a capability (see "Choices and how they are wired"). Each interface has a fake or
  a test host, so a branch is tested without docker, ssh or a network.
  *Enforced:* `conventions.rs` fails if a concrete host, engine or tool is named
  outside its own module and `cmd/`.
- **One place per external program.** `ssh` is started only in `host/ssh.rs`;
  `docker` is never started directly: `runtime/cli/` asks its `Host` to run it,
  and no other module spells it out; a local `sh` only in `host/local.rs` (scripts
  on this machine as the project's host) and `local.rs` (a secret's `command`
  source, which is on this machine whatever the host); and the helper tools
  (`ldd`, `patchelf`) only in `tools/external.rs`, through `host_tools`,
  behind `DepReader` and `ElfEditor`. The registry is spoken to in-process
  (`tools/oci/`, behind `OciFetcher`): the HTTP crate is named only in
  `http.rs` and the hash crate only in `digest.rs`. lodge never runs a local
  `tar`; tar streams are built and unpacked in-process. `host/ssh.rs` builds
  every `ssh` command from one private `base`. *Enforced:* the tests in
  `conventions.rs`.
- **Dependencies point one way.** `cmd/` on top (the only layer that knows
  `cli`), lodge's features below it (`config`, `devcontainer`, `bundle`,
  `inject`, `secret`, `session`, `forward`), and a foundation (`runtime`,
  `host`, `tools`, `shell`, `layout`, `output`, `tmpfile`, `tree`,
  `fingerprint`, `host_tools`, `port`, `provision`) that names none of them. *Enforced:* the
  tests in `conventions.rs`.
- **Paths in the container** come from `layout.rs`; **the container user** is a
  `ContainerUser` resolved once by the command, after provisioning
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
running or stopped. Through `FakeHost`, which records every job and answers from a script (with files in
memory, and a remote one losing its connection on exit 255 like ssh), the engine
plumbing is tested without a process: the commands it builds, a dropped connection
during `docker run` (the retry starts what it finds instead of running again),
build and compose retries, secrets staying out of the arguments, and everything
that reads a project through a host (config discovery, the drift hash, local
features, `start` and `status`). `FakeLocal` does the same for the machine lodge
runs on: environment variables, a secret's file and command, `${localEnv:…}`, the
terminal. `conventions.rs` checks the architecture rules above.

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
binaries (`ldg-<target>.tar.gz`, what `install.sh` downloads, with the shell completions and the man page beside `ldg`) are attached, and
only then is the release published, so "latest" never lacks them. `make check` runs the same gate locally, `make deny`
the dependency check.

The repo's own devcontainer (`.devcontainer/`) has the docker CLI and compose
(talking to the host's engine through its socket), host networking and a shared
`TMPDIR`, so `make integration` runs inside it as well; `make dev` opens it with
the `ldg` built from the tree.
