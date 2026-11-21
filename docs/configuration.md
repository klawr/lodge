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
shell       = "/bin/bash"   # shell launched inside the container (default)
multiplexer = "tmux"        # "tmux" or "none" (default: "tmux")

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

| field         | default     | meaning |
| ------------- | ----------- | ------- |
| `shell`       | `/bin/bash` | the shell started in the container; it must exist there. bash, zsh, sh and dash are supported. It is started once as a login shell, so the profile scripts run, then as the interactive shell you work in |
| `multiplexer` | `"tmux"`    | `"tmux"`: a per-container tmux session on this machine (from inside tmux, lodge switches to it instead of nesting; without tmux installed it falls back to `"none"`). `"none"`: exec straight into the container |

## `[git]`

| field              | default | meaning |
| ------------------ | ------- | ------- |
| `safe_directories` | `["*"]` | directories registered as git `safe.directory` for the container user; `[]` disables it |

## `bundle`

A top-level `bundle = "path/to/lodge.tar"` injects from that bundle instead of
packing the `[[inject]]` entries. A relative path is relative to the config
file's folder. `--bundle FILE` on the command line overrides it. See
[Bundles and secrets](bundles-and-secrets.md).

## `[[secret]]`

Values (API keys, tokens, credential files) delivered when you attach, and
never stored in a bundle. See [Bundles and secrets](bundles-and-secrets.md#secrets)
for the fields and how delivery works.
