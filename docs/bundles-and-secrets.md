# Bundles and secrets

## Bundles

Injection always works from a **bundle**: a plain tar holding your tools already
relinked to a bundled glibc, their libraries, your configs and git settings, and a
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
ldg pack -o lodge-x86_64.tar             # on the Linux/WSL machine: resolves, relinks, snapshots
ldg bundle verify lodge-x86_64.tar       # after copying it somewhere
ldg bundle inspect lodge-x86_64.tar      # what it holds
ldg up --bundle lodge-x86_64.tar         # anywhere with a container runtime (or just ssh with -r)
```

or set `bundle = "~/lodge-x86_64.tar"` at the top of `config.toml`. A configured
bundle replaces the `[[inject]]` entries on that machine; `[session]` stays local.

### Keeping a bundle in a registry

Instead of carrying a file, a bundle can live in any OCI registry (ghcr.io,
Docker Hub, your own), and `bundle =` and `--bundle` take its reference:

```toml
bundle = "oci://ghcr.io/me/lodge:1"
```

```sh
ldg up --bundle oci://ghcr.io/me/lodge:1
ldg up --bundle oci://ghcr.io/me/lodge@sha256:4f1c…   # pinned to exact content
```

A reference with a scheme is a registry (or whatever else lodge learns to read);
anything else is a file. lodge asks the registry what the tag points at on every
`up`, which is one small request, and downloads only when it changed: the bundle
is cached under the digest, so an unchanged tag costs no download. Nothing is
cached for the lookup itself, so a registry that cannot be reached fails the `up`
(use a file when you are offline).

lodge reads an image, or an index with one image per machine (each entry with
its `platform`, such as `linux/amd64` or `linux/arm64`; the one for the machine
lodge runs on is used). The image has **one layer: the bundle's tar**, as `ldg pack`
wrote it. Anything else is refused, and so is a tar that is not a bundle. The
layer's media type is not checked; `application/vnd.lodge.bundle.v1.tar` is
the suggested one. Any tool that makes such an artifact will do, for example:

```sh
ldg pack -o lodge-x86_64.tar
oras push ghcr.io/me/lodge:1 --artifact-type application/vnd.lodge.bundle.v1 \
  lodge-x86_64.tar:application/vnd.lodge.bundle.v1.tar
```

Without an index, publish one tag per architecture (`:1-x86_64`, `:1-aarch64`).
lodge does not publish bundles itself.

Two things to keep in mind:

- **A bundle is code that runs in your containers.** Pin by digest when the
  registry is not yours, and only use a registry you trust for what you publish.
- **A bundle holds your configs and git settings**, so publish it to a private
  repository (see the warning below about what a config directory can hold).
  Secrets are never in a bundle.

The login for a private registry is the one `docker login` stored in docker's
config file; a credential helper is not asked (see
[Troubleshooting](troubleshooting.md#a-bundle-or-a-feature-cannot-be-pulled)).
`ldg bundle inspect` and `ldg bundle verify` still take a file.

A machine that only *uses* a bundle needs just the tar and a small config of its
own: the `bundle =` line, `[session]`, and any `[[secret]]` (see below). Keep them
in one folder and point lodge at it with `-c`:

```toml
# deploy/config.toml
bundle = "bundle.tar"          # relative to this file's folder

[[secret]]
name = "ANTHROPIC_API_KEY"
from = { env = "ANTHROPIC_API_KEY" }
as   = "env"
```

```sh
ldg -c deploy/config.toml --remote me@host:2222 up /devel/project
```

`-c` replaces `~/.config/lodge/config.toml` for the run (the file must exist);
`--bundle` on the command line still overrides the config's `bundle`. A bundle is
for one CPU architecture and must match the container's (from a registry, the
image for the machine you run `ldg` on is used, so for a container of another
architecture use a file or a tag of its own). It can hold credentials
if you list the wrong directory, so `pack` warns about `~/.ssh`, `~/.config/gh`,
`~/.claude` and similar.

## Secrets

Things you want in the container but must **not** be in a bundle (an API key, a
GitHub token, a credential file) are declared as `[[secret]]` in your local
`config.toml`. A bundle is a shareable artifact; secrets never go into one. They
are declared in the config of the machine you attach from, so they work the same
with a live `ldg up` and with a bundle.

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

# A credential file, placed where the tool looks for it.
[[secret]]
name = "claude_login"
from = { file = "~/.claude/.credentials.json" }
as   = { file = "~/.claude/.credentials.json", mode = "0600" }
```

| field     | required | meaning |
| --------- | -------- | ------- |
| `name`    | yes      | letters, digits and `_`; the variable name for `as = "env"`, otherwise just a label. Unique. |
| `from`    | yes      | where the value comes from on **this** machine: exactly one of `{ env = "VAR" }`, `{ file = "path" }` or `{ command = "…" }` (stdout, trimmed; 30 s limit, stdin closed). |
| `as`      | yes      | `"env"`, or `{ file = "/path/in/container", mode = "0600" }` (`~/` is the container user's home; `mode` defaults to `0600`). |
| `targets` | no       | globs (`*`, `?`) matched against the container name and the host (`local`, or the `--remote` value and its host part). Omitted: any container. |

How they are delivered, when you attach (`ldg up`, not with
`--no-attach`):

- The value is read on your machine and sent over the container command's
  **stdin**, never as an argument, so it is in neither `ps` nor the `-v` trace.
- It is stored under `/dev/shm/lodge-secrets-<uid>/`: memory only, owner-only,
  never in an image layer, gone when the container stops.
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

Limits:

- Secrets live in `/dev/shm`, so they last until the container stops, and a
  multiplexer you started in the container outlives a detach. Removing *every*
  `[[secret]]` does not clear values already placed (nothing runs when none are
  declared); removing some does, and so does `ldg clean`.
- A tool that refreshes a credential file may replace the symlink with a real
  file in the home directory, which then stays on disk.
- `/dev/shm` is 64 MB by default in Docker: enough for tokens, not for large files.
