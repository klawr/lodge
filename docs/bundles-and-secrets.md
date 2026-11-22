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
ldg pack -o lodge-x86_64.tar             # on the Linux/WSL machine: resolves, relinks, snapshots
ldg bundle verify lodge-x86_64.tar       # after copying it somewhere
ldg bundle inspect lodge-x86_64.tar      # what it holds (both also take oci://…)
ldg up --bundle lodge-x86_64.tar         # anywhere with a container runtime (or just ssh with -r)
```

or set `bundle = "~/lodge-x86_64.tar"` at the top of `config.toml`. A configured
bundle replaces the `[[binary]]` and `[[data]]` entries on that machine; `[session]` stays local.

A machine that only *uses* a bundle needs just the bundle (a file, or a registry
reference as below) and a small config of its own: the `bundle =` line,
`[session]`, and any `[[secret]]` (see below). Keep them in one folder and point
lodge at it with `-c`:

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
bundle = "oci://ghcr.io/me/lodge:1"
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
Publishing needs `docker login <registry>` with an account that may write there;
lodge reads that login from docker's config file or its credential helper (see
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
