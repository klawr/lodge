# Configuration

lodge reads `~/.config/lodge/config.toml`, or the file given with the global
`-c FILE` (which must exist). Without a config nothing is injected, and you
still get a shell. `ldg init` writes a commented starting point, with the tools
it finds on your `PATH` filled in.

The config is checked as a whole before anything starts: an unknown key (with a
suggestion for a likely typo), a duplicate tool name, an entry with nothing to
inject, a target path that is neither absolute nor under `~/`, and a malformed
secret are all reported together. `ldg doctor` shows the
same report.

**Editor completion.** A JSON schema describes the file:
[`config.schema.json`](config.schema.json). Configs written by `ldg init` start
with a `#:schema` line that taplo / Even Better TOML pick up; add it by hand to
an older one:

```toml
#:schema https://raw.githubusercontent.com/klawr/lodge/master/docs/config.schema.json
```

## Example

```toml
# Inject Neovim plus its runtime and plugins.
[[inject]]
name           = "nvim"
binary         = "/usr/local/bin/nvim"     # host binary to copy in
config         = "~/.config/nvim"          # host config dir/file to copy in
target_binary  = "~/.local/bin/nvim"       # also write a wrapper script here (optional)
target_config  = "~/.config/nvim"          # where the config goes (optional, default shown)
copy_deps      = true                      # copy shared libs via ldd when packing (default: true)

# Neovim needs its runtime at the compiled-in $VIMRUNTIME path.
[[inject]]
name          = "nvim-runtime"
config        = "/usr/local/share/nvim/runtime"
target_config = "/usr/local/share/nvim/runtime"

# Ship installed plugins so the container uses your exact host versions instead
# of re-cloning them at HEAD (which drifts into incompatible versions).
[[inject]]
name          = "nvim-plugins"
config        = "~/.local/share/nvim/lazy"
target_config = "~/.local/share/nvim/lazy"

[session]
shell = "/bin/bash"   # shell launched inside the container (default)

[git]
# Registered as git `safe.directory` in the container, so git-backed tools
# (lazy.nvim, gitsigns, …) don't trip over "detected dubious ownership" on the
# host-owned workspace/plugins. Default: ["*"]; set [] to disable.
safe_directories = ["*"]
```

## `[[inject]]`

Each entry brings a binary (with its libraries), a config file or directory,
or both.

| field            | required | default                 | meaning |
| ---------------- | -------- | ----------------------- | ------- |
| `name`           | yes      | —                       | identifier, unique; used for default paths |
| `binary`         | no       | —                       | host path to a binary to inject |
| `config`         | no       | —                       | host path to a config file/dir to inject |
| `target_binary`  | no       | none                    | also write a wrapper script that runs the binary at this path in the container (absolute or `~/…`, no `..`). Without it the binary is on the `PATH` of the sessions lodge opens |
| `target_config`  | no       | `~/.config/<name>`      | config location in the container (`~` is the container user's home; absolute or `~/…`, no `..`) |
| `copy_deps`      | no       | `true`                  | copy shared-library dependencies via `ldd` when packing |

At least one of `binary` and `config` must be set. Host paths may start with `~/`.

lodge writes as the container user and never uses root, so a target must be a place
that user can write (under `~`, say). A target elsewhere fails with the path and the
user named.

Shell aliases and other shell setup are your shell's own config: inject your
`~/.bashrc` (or your shell and its config) with an `[[inject]]` entry whose
`target_config` is where the container user's shell reads it.

## `[session]`

lodge opens a shell in the container and nothing more. For a session that
outlives a disconnect, use a multiplexer: see
[Keeping a session alive](#keeping-a-session-alive).

| field         | default     | meaning |
| ------------- | ----------- | ------- |
| `shell`       | `/bin/bash` | the shell started in the container; it must exist there. bash, zsh, sh and dash are supported. It is started once as a login shell, so the profile scripts run, then as the interactive shell you work in |

## Keeping a session alive

A multiplexer (tmux, zellij, screen) is a tool like any other: inject it, and
start it from your shell's rc file. The container keeps it running when you
leave, so the next `ldg up` (or `ldg attach`) lands in the same session, with
your editor as you left it. The same holds after a dropped `--remote` connection: `ldg` asks whether
to reconnect and open a new shell, which reads the rc file again and attaches.

A tmux setup, with three `[[inject]]` entries:

```toml
[[inject]]
name   = "tmux"
binary = "/usr/bin/tmux"

# ~/.bashrc in the container: start tmux once, attaching if it is already there.
[[inject]]
name          = "tmux-rc"
config        = "~/.config/lodge/tmux-rc"
target_config = "~/.bashrc"

# ~/.tmux.conf in the container.
[[inject]]
name          = "tmux-conf"
config        = "~/.config/lodge/tmux.conf"
target_config = "~/.tmux.conf"
```

`~/.config/lodge/tmux-rc` (use your own `~/.bashrc` and add these lines, if you
already inject one):

```sh
if [ -z "$TMUX" ] && command -v tmux >/dev/null; then
  exec tmux new-session -A -s main
fi
```

`~/.config/lodge/tmux.conf`:

```
set -g default-command bash
```

The last line matters. tmux starts each pane as a *login* shell, and on Debian
and Ubuntu `/etc/profile` resets `PATH`, which would hide everything lodge
injected (`tmux detach` itself would be "not found"). lodge's own session puts
its tools back first on `PATH`; a pane started as a plain `bash` instead inherits
that `PATH` from the tmux server. Use your shell if it isn't bash. Any other
multiplexer needs the same care: its panes must not be login shells that reset
`PATH`.

This is covered by an integration test, so it keeps working.

## `[git]`

| field              | default | meaning |
| ------------------ | ------- | ------- |
| `safe_directories` | `["*"]` | directories registered as git `safe.directory` for the container user; `[]` disables it |

## `target`

A top-level `target = "docker"` says where lodge works: a devcontainer on `docker` or
`podman`, or `host` for the machine itself (see below). `--target` on the command line
overrides it, and without either it is `docker`. It is on the machine `--remote`
names, or on this one. A value that is not a target is an error.

Which engine to talk to, when you have several, is a setting of the target:

```toml
[targets.docker]
context = "colima"      # docker --context colima …

[targets.podman]
connection = "vm"       # podman --connection vm …
```

Both are optional; without them the engine's own default is used. Only the table of
the chosen target is read.

With `podman`, lodge runs `podman` the way it runs `docker`, and adds what a
project bind-mounted into a Podman container needs: `--userns=keep-id` when
Podman runs without root (so your files are yours inside), and
`--security-opt label=disable` (so SELinux does not block the mount). A
`--userns` in `runArgs`, or a `label=` in `securityOpt`, replaces them. What
differs from docker:

- Compose needs a provider that `podman compose` finds (docker-compose or
  podman-compose). lodge adds no user-namespace flag to a compose project; set
  `userns_mode` in the compose file if you need one.
- Without root, a container's address is not reachable from the host, so only the
  ports it publishes (`appPort`) are forwarded. With root, or on
  another machine through `--remote` with a rootful Podman, it works as with docker.

### `host`: no container

`target = "host"` (or `--target host`) makes lodge work on the machine itself:
this one, or another through `--remote`. There is no devcontainer and no engine.
`ldg up [PATH]` injects your bundle into the user's prefix on that machine, delivers
secrets, and opens a session in `PATH` (the machine's login directory if you
leave it out over `--remote`).

Because it is a real machine, lodge is careful with it:

- It acts as the user the machine connects as, and cannot act as another (`root` or a
  named user is refused).
- A config lodge did not place is **left as it is**, with a warning, instead of
  replaced: `ldg up` never overwrites your own dotfiles. One it placed earlier is
  updated.
- git's `safe.directory = *` is not set. List folders in `[git] safe_directories`
  if you want some.
- The machine must be Linux, and have `tar`. The bundle holds Linux binaries.
- Nothing is created, so `down` and `rm` say so; `ls` has nothing to list;
  `ldg clean` removes the prefix and the wrappers, as for a container.
- Ports listening on a remote machine are forwarded through ssh to this one, as
  for a container; on this machine there is nothing to forward.

## `bundle`

A top-level `bundle = "path/to/lodge.tar"` injects from that bundle instead of
packing the `[[inject]]` entries. A relative path is relative to the config
file's folder. `--bundle FILE` on the command line overrides it. See
[Bundles and secrets](bundles-and-secrets.md).

## `[[secret]]`

Values (API keys, tokens, credential files) delivered when you attach, and
never stored in a bundle. See [Bundles and secrets](bundles-and-secrets.md#secrets)
for the fields and how delivery works.
