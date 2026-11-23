# Bundles and secrets

## Bundles

Injection always works from a **bundle**: a plain tar holding your tools already
relinked to a bundled glibc, their libraries, your data and git settings, and a
manifest. A bundle is tied to a CPU architecture, but not to a user, a home
directory or an install location: it is injected into whichever user's prefix it
is used for. Without any options, `ldg up` packs your config into a
bundle itself and keeps it in `~/.cache/lodge/bundles/`. It is reused as long as
nothing it depends on has changed (the config, each source file's size and
mtime, the libraries `ldd` finds). Checking that runs `ldd`, but a repeated `up`
copies and relinks nothing (`patchelf` does not run). `ldg cache` lists the cached bundles (the four most recently used are
kept) and `ldg cache clear` empties the cache.

To use your setup on a machine that isn't your Linux workstation, pack once and
carry the file:

```sh
ldg pack -o lodge-x86_64.tar             # on a Linux (or WSL2) machine: resolves, relinks, snapshots
ldg bundle verify lodge-x86_64.tar       # after copying it somewhere
ldg bundle inspect lodge-x86_64.tar      # what it holds (both also take oci://…)
ldg up --bundle lodge-x86_64.tar         # anywhere with a container runtime (or just ssh with -r)
```

or list it under `bundles` in `client.toml` and set `default_bundle` to it. A configured
bundle replaces the `[[binary]]` and `[[data]]` entries of `config.toml` on that
machine; `[session]` stays local.

A bundle is made from `config.toml` alone. The settings that depend on the machine
(`default_target`, `default_bundle`, `bundles`, `[[secret]]`, `[keys]`) are in `client.toml`, so none
of them can end up in one.

A machine that only *uses* a bundle needs just the bundle (a file, or a registry
reference as below) and a small `client.toml` of its own: the `bundle =` line and any
`[[secret]]` (see below), and an empty `config.toml` beside it (or one with `[session]`).
Keep them in one folder and point lodge at it with `-c`:

```toml
# deploy/client.toml
bundles        = ["bundle.tar"]   # relative to this file's folder
default_bundle = "bundle.tar"

[[secret]]
name = "ANTHROPIC_API_KEY"
from = { env = "ANTHROPIC_API_KEY" }
as   = "env"
```

```sh
ldg -c deploy/config.toml --remote me@host:2222 up /devel/project
```

`-c` replaces `~/.config/lodge/config.toml` for the run (the file must exist), and with
it the `client.toml` beside it; `--bundle` on the command line still overrides the
client file's `default_bundle`. A bundle is
for one CPU architecture and must match the container's (from a registry, the
image for the machine you run `ldg` on is used, so for a container of another
architecture use a file or a tag of its own). It can hold credentials if you list
the wrong directory, so `pack` warns about `~/.ssh`, `~/.config/gh`, `~/.claude`
and similar.

### Keeping a bundle in a registry

Instead of carrying a file, a bundle can live in any OCI registry (ghcr.io,
Docker Hub, your own). The same reference is written by `ldg pack -o` and read by
`bundle =` and `--bundle`:

```sh
ldg pack -o oci://ghcr.io/me/lodge:1        # build it and publish it
ldg up --bundle oci://ghcr.io/me/lodge:1    # use it, on any machine (`-b` for short)
ldg up --bundle oci://ghcr.io/me/lodge@sha256:4f1c…   # pinned to exact content
```

```toml
# client.toml
bundles        = ["oci://ghcr.io/me/lodge:1"]
default_bundle = "oci://ghcr.io/me/lodge:1"
```

A reference with a scheme is a registry (or whatever else lodge learns to read or
write); anything else is a file.

**Publishing.** `ldg pack -o oci://…` packs as usual, uploads the bundle, and
prints the exact reference it was written under (`oci://ghcr.io/me/lodge@sha256:…`),
which you can pin. A tag holds an **index with one image per architecture**:
publishing from an x86_64 machine and then from an aarch64 one under `:1` gives a
tag that both use, each getting its own. Publishing replaces only the entry for the
machine's own architecture (its old image is no longer reachable by the tag), and
keeps the others. If two machines publish at once the last one wins, and the
other's entry may be lost: publish them one after the other. A tag that held a
single image (not made by this lodge) is replaced. Replacing is silent; for CI that
must not overwrite, `ldg pack -o oci://… --no-clobber` refuses, before uploading
anything, when the tag already holds an image for this machine (the same flag
refuses to overwrite an output file that exists).
Publishing needs a login with an account that may write there. Keep one with
`ldg registry login <registry>` (the password or access token is typed without being
shown, or piped in; with two-factor authentication on, registries want an access
token, not the account password). It goes in this machine's credential store, not in
either file or a bundle, so it is per machine and per user. lodge reads it first, then
the login `docker login` keeps in docker's config file or its credential helper. When
a registry refuses and you are at a terminal, `ldg` asks for a login on the spot,
tries it, and offers to keep it; with no terminal it fails as before (see
[Troubleshooting](troubleshooting.md#a-bundle-or-a-feature-cannot-be-pulled-or-published)).

**Using.** lodge asks the registry what the tag points at on every `up`, which is
one small request, and downloads only when it changed: the bundle is cached under
the digest, so an unchanged tag costs no download. lodge also remembers which
bundle the tag named the last time (in `tags.json` in the cache), so when the
registry **cannot be reached at all** (no network, no answer), `up` warns and uses
that bundle if it is still in the cache. A registry that answers and refuses, or
has no such tag, is never overruled by the cache, and a tag that was never fetched
on this machine has nothing to fall back to: that fails the `up` as before.

**What is in the registry.** An image with an empty config and **one layer: the
bundle's tar**, typed `application/vnd.lodge.bundle.v1` (the artifact) and
`application/vnd.lodge.bundle.v1.tar` (the layer). That is all lodge reads, so a
tool of your own that makes the same artifact works too. The tag itself holds
an index with one image per machine (each entry with its `platform`, such as
`linux/amd64` or `linux/arm64`; the one for the machine lodge runs on is used).
A single image under a tag is read too. Anything with more than one layer is refused, and so
is a tar that is not a bundle; the media types are not checked, the bundle's own
manifest is.

Two things to keep in mind:

- **A bundle is code that runs in your containers.** Pin by digest when the
  registry is not yours, and only use a registry you trust for what you publish.
- **A bundle holds your data and git settings**, so publish it to a private
  repository (see the warning below about what a data directory can hold).
  Secrets are never in a bundle.

`ldg bundle inspect` and `ldg bundle verify` take a file or a reference
(`ldg bundle verify oci://ghcr.io/me/lodge:1`): a reference is always fetched, since
a check must look at the bytes and not at what the cache remembers, and kept no
longer than the command runs. `ldg status` shows which bundle a container holds and
where it came from (`source`: the digest, for a registry), which answers "what is
actually in my container?". `ldg doctor` only looks a bundle up, as `up` would: it
says what the tag resolves to and whether it is cached, and fetches nothing.

### Several bundles

The client file lists the bundles **on offer** (`bundles = [...]`) and points at one of
them as the default (`default_bundle =`; without it the default is the packed config).
The default is always one of the listed ones.

```sh
ldg bundle ls                       # what is on offer; the default is marked
ldg bundle use oci://ghcr.io/me/lodge:2   # make it the default (fetched and checked first)
ldg bundle use --live               # the default is the config's own entries, packed (Linux)
ldg bundle use                      # choose from what is on offer
ldg bundle add ~/work.tar           # put one on offer without making it the default
ldg bundle rm ~/work.tar
```

- `use` fetches and checks the bundle **before** it touches `client.toml`, so a wrong
  reference, or an image for another architecture, changes nothing. The bundle is
  listed first if it is not (the default is always one of the listed ones), and the
  one that was the default stays listed.
- `ldg pack -o …` puts what it wrote on offer, a file by its absolute path and a
  registry as written (the tag, not the digest). It does that when `client.toml`
  exists, and only says so if it cannot.
- A container keeps the bundle it was injected from. After switching, `ldg up
  --reinject` (or `--reinject -b REF` for one container only) injects the new one,
  replacing what the old one placed. When what is injected differs from what a run
  would use, `ldg up` says which bundle each side has. `ldg status` shows the one
  in the container.
- **Asking.** When `ldg up` or `ldg attach` is about to inject, with no `--bundle`,
  into a place it has no history for (a new project) or with `--reinject`, and more
  than one bundle is on offer, it asks which. Declining, or having no terminal
  (`--no-attach`, CI, a pipe), takes the default. What you answered is used for that
  run and kept in the history; `client.toml` is not changed.
- The history (a bare `ldg`) shows the bundle each place was last opened with. It is
  shown, not replayed: opening a place again uses the default.
- The config screen (`c`) chooses the default on its `default_bundle` line.

## Secrets

Things you want in the container but must **not** be in a bundle (an API key, a
GitHub token, a credential file) are declared as `[[secret]]` in your
`client.toml`. A bundle is a shareable artifact; secrets never go into one, and
they are not in the file a bundle is made from (`config.toml`). They are declared on
the machine you attach from, so they work the same with a live `ldg up` and with a
bundle.

```toml
# A token from `pass`, as an environment variable in the session.
[[secret]]
name = "ANTHROPIC_API_KEY"
from = { command = "pass show anthropic" }
as   = "env"

# The GitHub CLI's token, only for some containers.
[[secret]]
name    = "GH_TOKEN"
from    = { command = "gh auth token" }
as      = "env"
targets = ["build-*"]

# A file the tool only reads (a deploy key), placed where it looks for it.
[[secret]]
name = "deploy_key"
from = { file = "~/.ssh/deploy_key" }
as   = { file = "~/.ssh/deploy_key", mode = "0600" }
```

| field     | required | meaning |
| --------- | -------- | ------- |
| `name`    | yes      | letters, digits and `_`; the variable name for `as = "env"`, otherwise just a label. Unique. |
| `from`    | yes      | where the value comes from on **this** machine: exactly one of `{ env = "VAR" }`, `{ file = "path" }`, `{ command = "…" }` (stdout, trimmed; 30 s limit, stdin closed) or `{ store = "entry" }` (this machine's credential store, see below). |
| `as`      | yes      | `"env"`, or `{ file = "/path/in/container", mode = "0600" }` (`~/` is the container user's home; `mode` defaults to `0600`). |
| `targets` | no       | globs (`*`, `?`) matched against the container name and the host (`local`, or the `--remote` value and its host part). Omitted: any container. |

### Keeping a value in the credential store

A `store` source reads the value from your operating system's credential store
(Windows Credential Manager, the macOS Keychain, or the Secret Service on Linux:
GNOME Keyring, KeePassXC, …), so neither `client.toml` nor a file on disk holds it:

```toml
[[secret]]
name = "CLAUDE_CODE_OAUTH_TOKEN"
from = { store = "claude_token" }
as   = "env"
```

```sh
ldg secret set claude_token     # typed without being shown (or: ldg secret set claude_token < file)
ldg secret ls                   # the entries your client file reads, and whether each is set
ldg secret rm claude_token
```

The value is never an argument, and it is never printed. The store belongs to your
user on this machine: a value set on one client is not on another, so set it on each
client that should deliver it. `ldg doctor` checks that each store entry is set. On
Linux without a Secret Service (a headless server), a `store` source fails with a
message that says so; `env`, `file` and `command` sources need nothing.

### How they are delivered

When you attach (`ldg up`, not with `--no-attach`):

- The value is read on your machine and sent over the container command's
  **stdin**, never as an argument, so it is in neither `ps` nor the `-v` trace.
- It is stored under `/dev/shm/lodge-secrets-<uid>/`: memory only, owner-only,
  never in an image layer, and removed when the session ends (see the limits below).
- An `env` secret is exported by the session start-up script, which reads it from
  that file. A `file` secret is symlinked from the path you gave.
- lodge prints secret *names*, never values. `ldg status` shows which declared
  secrets are placed; `ldg doctor` checks that each source resolves.
- A source that cannot be resolved is a warning; the session still opens.
- Removing a `[[secret]]` removes its value on the next attach (but see below).

lodge only delivers. Logging in, which credential a tool prefers, and refreshing
tokens are the tool's business. In particular, if you declare an API key, a tool
may use it in preference to a subscription login, so declare only what you want
used.

### Which credentials to inject, and which to leave in the container

There are two kinds, and they should not be mixed.

- **A secret you hold on the client**: an API key, a token. lodge reads it on your
  machine at every attach and places it for the session; it is gone when the session
  ends. Any client that has the value can deliver it, and a new container needs
  nothing done to it.
- **A login the tool owns**: you log in inside the container (the tool's own
  `login`), and the tool keeps its credential in the container's home. lodge never
  touches it. It lasts as long as the container does, so a new container means
  logging in again. Nothing is copied back to your machine.

The mistake to avoid is declaring a `file` secret for a file the tool also
**rewrites**. Every attach replaces the container's copy with a link to your
machine's, so whatever the tool refreshed inside is lost, and a tool that rotates its
refresh token on each refresh leaves your machine's copy stale, which can log you out.
Use `as = { file = … }` only for files the tool just reads.

For a tool that has a login, prefer a **long-lived token** you hold yourself over its
credential file. For Claude Code, `claude setup-token` prints one; keep it in the
credential store and deliver it as an environment variable, as in the example above
(`CLAUDE_CODE_OAUTH_TOKEN`). For the GitHub CLI, `GH_TOKEN` from `gh auth token`
does the same. A token like that does not rotate, so it can be on several clients
and in several containers at once.

A tool that keeps its login in the operating system's keychain instead of a file
cannot have it copied into a container: log in inside the container.

### Windows, VS Code and a corporate GitHub login

Being logged in on your Windows machine, or in VS Code, does not log you in inside a
container: lodge delivers only what you declare, and it does not read another tool's
login. What follows is what the tools' own documentation says at the time of writing;
check it if a tool changes.

- **Claude Code.** Its login is kept in `%USERPROFILE%\.claude\.credentials.json` on
  Windows, `~/.claude/.credentials.json` on Linux and the Keychain on macOS. Do not
  copy the file. Run `claude setup-token` (a one-year token, for the paid plans), keep
  it with `ldg secret set` (on Windows that is Credential Manager) and deliver it as
  `CLAUDE_CODE_OAUTH_TOKEN`, as above. An `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`
  or `apiKeyHelper` takes precedence over it.
- **GitHub Copilot.** VS Code keeps its sign-in itself, and what its Dev Containers
  support forwards into a container is VS Code's own doing. With lodge there is no
  VS Code in the container, so the tool you use there signs in on its own. Two ways,
  with the Copilot CLI as the example:
  - *Log in inside the container* with `copilot login`. In a dev container or over
    ssh it uses the device-code flow: it prints a code, and you approve it in a
    browser on your own machine (single sign-on included). With no system keychain in
    the container, the CLI offers to keep the token in a plaintext file in the
    container's home (`~/.copilot/config.json`). That is a login the tool owns: it
    lasts as long as the container, and a new container means logging in again.
  - *Deliver a token at every attach.* The CLI reads `COPILOT_GITHUB_TOKEN`, then
    `GH_TOKEN`, then `GITHUB_TOKEN`. It accepts an OAuth token (`gho_…`), a
    fine-grained personal access token with the "Copilot Requests" permission (owned
    by a personal account) or a GitHub App user token (`ghu_…`), but not a classic
    token (`ghp_…`).

    ```toml
    [[secret]]
    name = "COPILOT_GITHUB_TOKEN"
    from = { command = "gh auth token --hostname github.example.com" }
    as   = "env"
    ```

    Whether the CLI accepts the token that `gh auth token` prints is not documented;
    try it. An environment variable overrides a login stored in the container without
    a warning, so declare one only when you want it used.
  - GitHub's page covers GitHub Enterprise Cloud with data residency (`*.ghe.com`,
    `copilot login --host`). It does not mention a self-hosted GitHub Enterprise
    Server.
- A token in a container is readable by anyone who can run a command there (see the
  limits below).

Limits:

- Secrets live in `/dev/shm`, and only as long as the session that placed them: when
  the shell lodge started ends, they are removed (unless another client has taken the
  container over, in which case they are its secrets).
  - A `file` secret's file is gone for every process. An `env` secret is not: every
    process the session started has it in its environment, so a multiplexer or any
    background process you started from that shell keeps it until it ends.
  - A session that ends without lodge getting to clean up (a killed terminal, a lost
    connection that is not restored) leaves the secrets until the next attach or until
    the container stops.
  - What is placed is what the client that attached last declares: any other value is
    removed, so removing some or *every* `[[secret]]` takes effect on the next attach,
    and so does `ldg clean`. A secret you declare whose source can't be read this time
    keeps the value already placed, with a warning.
- A container is one person's workspace, with one lodge session at a time. Attaching
  ends the session lodge opened earlier (the shell it started, found through a record
  in lodge's directory in the container, `~/.local/share/lodge/session`, that names the
  user and machine it was opened from), and the secrets become the new client's.
  - From the same user and machine, the earlier session is replaced without asking:
    it is nearly always one that was left behind, and a second terminal from the
    same machine ends the first, so use a multiplexer for several panes.
  - From another client, lodge asks first (ending it also takes its secrets away; a
    stale connection is fine to end). Without a terminal to ask on, it refuses and
    says whose session is there.
  - The earlier client's shell is hung up, and its `ldg` says who took over
    (`this session was ended: bob@desktop attached to <container>`) instead of
    reporting a failure. A client whose connection dropped does not reopen a session
    once another has taken over. A multiplexer started from the
    ended shell keeps running; a hand-run `docker exec` or an IDE terminal is not a
    lodge session and is left alone.
- What this protects, and what it does not. A value is never in a bundle, an image
  layer, a command line, lodge's output or the config. It is not hidden from anyone
  who can run a command in the container as the same user or as root, such as a
  `docker exec` or another ssh login: they can read the 0600 file, and an `env`
  secret is also in the environment (`/proc/<pid>/environ`) of the processes that
  have it. For a remote target the value crosses ssh and sits in the remote
  container's memory, so you trust that machine's root. Whoever controls the
  container engine controls its secrets.
- A tool that refreshes a credential file may replace the symlink with a real file in
  the home directory, which then stays on disk until the next attach replaces it with
  the link again. Declare only files the tool just reads (see above).
- `/dev/shm` is 64 MB by default in Docker: enough for tokens, not for large files.
