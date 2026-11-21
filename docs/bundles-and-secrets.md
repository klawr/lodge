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

A machine that only *uses* a bundle needs just the tar and a small config of its
own: the `bundle =` line, `[session]`, and any `[[secret]]` (see below). Keep them
in one folder and point lodge at it with `-c`:

```toml
# deploy/config.toml
bundle = "bundle.tar"          # relative to this file's folder

[session]
multiplexer = "none"

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
for one CPU architecture and must match the container's. It can hold credentials
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

- Secrets live in `/dev/shm`, so they last until the container stops, and a tmux
  session outlives a detach. Removing *every* `[[secret]]` does not clear values
  already placed (nothing runs when none are declared); removing some does, and
  so does `ldg clean`.
- A tool that refreshes a credential file may replace the symlink with a real
  file in the home directory, which then stays on disk.
- `/dev/shm` is 64 MB by default in Docker: enough for tokens, not for large files.
