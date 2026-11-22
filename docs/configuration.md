# Configuration

lodge reads `~/.config/lodge/config.toml`, or the file given with the global
`-c FILE` (which must exist). Without a config nothing is injected, and you
still get a shell. `ldg init` writes a commented starting point, with the tools
it finds on your `PATH` filled in.

The config is checked as a whole before anything starts: an unknown key (with a
suggestion for a likely typo), a duplicate name, a target path that is neither absolute nor under `~/`, and a malformed
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
# Programs: the binary, relinked, with the libraries it needs.
[[binary]]
name      = "nvim"
path      = "/usr/local/bin/nvim"     # host binary to copy in
target    = "~/.local/bin/nvim"       # also write a wrapper script here (optional)
copy_deps = true                      # copy shared libs via ldd when packing (default: true)

# Data: files and directories, copied as they are, to the same place in the
# container. `for` only says which program one belongs to.
[[data]]
path = "/usr/local/share/nvim"        # what a program keeps beside its binary
for  = "nvim"

[[data]]
path = "~/.config/nvim"               # `~/…` stays under the container user's home
for  = "nvim"

# Ship installed plugins so the container uses your exact host versions instead
# of re-cloning them at HEAD (which drifts into incompatible versions).
[[data]]
path = "~/.local/share/nvim/lazy"
for  = "nvim"

[session]
shell = "/bin/bash"   # shell launched inside the container (default)

[git]
# Registered as git `safe.directory` in the container, so git-backed tools
# (lazy.nvim, gitsigns, …) don't trip over "detected dubious ownership" on the
# host-owned workspace/plugins. Default: ["*"]; set [] to disable.
safe_directories = ["*"]
```

## `[[binary]]`

A program to bring: its binary, relinked to the bundled glibc, and the shared
libraries it needs.

| field       | required | default | meaning |
| ----------- | -------- | ------- | ------- |
| `name`      | yes      | —       | identifier, unique among the binaries |
| `path`      | yes      | —       | host path to the binary (may start with `~/`) |
| `target`    | no       | none    | also write a wrapper script that runs the binary at this path in the container (absolute or `~/…`, no `..`). Without it the binary is on the `PATH` of the sessions lodge opens |
| `copy_deps` | no       | `true`  | copy shared-library dependencies via `ldd` when packing |

## `[[data]]`

A file or directory, copied as it is: what a program needs beside its binary (an
editor's runtime files), a config, a plugin folder, shell setup. A program may have
several, or none.

| field    | required | default                  | meaning |
| -------- | -------- | ------------------------ | ------- |
| `path`   | yes      | —                        | host path to the file or directory (may start with `~/`) |
| `target` | no       | the same place as `path` | where it goes in the container (`~` is the container user's home; absolute or `~/…`, no `..`) |
| `for`    | no       | none                     | the `name` of the `[[binary]]` it belongs to. It only says what the data is for (the screen and `ldg doctor` use it): it is injected the same either way |

A path under your home directory goes under the container user's home (`~/.config/nvim`
stays `~/.config/nvim`); any other path goes to the same absolute path. Data is told
apart by where it goes, so no two entries may go to the same place, and `for` must
name a `[[binary]]` of the config.

A program built to run from where it was installed looks for its files there. For a
binary at `<prefix>/bin/<name>`, lodge looks for `<prefix>/share/<name>` and
`<prefix>/lib/<name>`, and offers them as data for it when you add the program (`ldg
inject add binary`, and `b` in the config screen add them with it; `--no-data` leaves
them out) and when you ask `ldg init`. It does not need to know the program: it is
the layout every installed program follows. A program that keeps its files somewhere
else needs its `[[data]]` written, and `ldg doctor` says when it finds files beside a
binary that the config does not bring.

lodge writes as the container user and never uses root, so a target must be a place
that user can write (under `~`, say). A target elsewhere fails with the path and the
user named.

Shell aliases and other shell setup are your shell's own config: inject your
`~/.bashrc` (or your shell and its config) with a `[[data]]` whose `target` is
where the container user's shell reads it.

## Editing the config

Run `ldg` alone and press `c` for the config screen: the whole file as the form it
is in the TOML, with every value a line you can move to (↑/↓ or j/k): `target` and
`bundle`, `[session]` and `[git]`, then each `[[binary]]`, `[[data]]` and
`[[secret]]` with all of its fields under it, the ones that are not set shown as
such (`target = (the same place as here)`). Enter changes the selected one: `path`
opens the list of what this machine has, the rest is typed or chosen from a short
list (`for`, `target`, how a secret is read and placed), `copy_deps` is turned over.
`x` puts a value back to its default or clears it, and `r` removes the entry. What
each change did is on the first line, and a refused one says why and changes nothing.
A binary that is renamed takes its data with it, and one that is removed leaves its
data in place, no longer for anything.

| key | |
| --- | --- |
| `b` | add programs |
| `d` | add data |
| `s` | add a secret (a name, where its value comes from) |
| `p` | pack the bundle into a file (Enter takes `lodge-<arch>.tar` here) |
| `P` | publish the bundle to a registry (Enter takes the one `bundle =` names) |

Enter on the `bundle` line chooses the bundle in use from those on offer (the default, `bundles`, and the config as it is where it can be packed), or lets you type one; the one that was in use stays on offer. A pack or a publish from here lists what it wrote, as `ldg pack` does.

| `o` | work on another config file: the ones beside the default and in the folder `ldg` was started in, or a path typed in full |

`b` and `d` open what this machine has (for programs, the tools it knows that are
installed, then every other program on `PATH`; for data, what sits beside the
programs in your config, then your `~/.config` and the dotfiles in `~`), narrowing as
you type. Enter adds the selected one and marks it `[x]`, and the list stays open for
the next (Esc when done). A program is added with what sits beside it. What is in your
config already is in the list too, marked from the start; a path typed in full (`/…`,
`~/…`) is offered as it is, and goes where it is on this machine. Credentials and
state (`~/.ssh`, `~/.config/gh`, caches) are never listed.

A config that does not load (after an upgrade that renamed a table, say) is still
shown, with why on top, and what this version of lodge does not know (a table it
dropped, a misspelled key) grouped at the end as lines of their own. `r` removes
one, and the rest of the file, its comments included, stays as it was. Any other
change is kept while it leaves the file with fewer problems than it had, so a config
with several can be mended one at a time; one that would add a problem is refused, as
for a config that loads. Only a file that is not TOML at all has no form.

Not in the screen yet: the `[targets.<name>]` tables, which each target defines for
itself.

The same from the command line, without opening the file:

```sh
ldg inject                                    # list the entries
ldg inject add binary rg                      # a program found on PATH
ldg inject add binary nvim /usr/local/bin/nvim --target '~/.local/bin/nvim'
ldg inject add data ~/.config/nvim --for nvim
ldg inject rm binary rg
ldg inject rm data ~/.config/nvim
```

The file is edited in place (comments and order stay), and nothing is written
unless the result is a valid config. With `bundle =` set, the entries are not
used until you pack again or drop that line.

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
your editor as you left it. The same holds after a dropped `--remote` connection: `ldg` reconnects
by itself and opens a new shell, which reads the rc file again and attaches.

A tmux setup, with a `[[binary]]` and two `[[data]]` entries:

```toml
[[binary]]
name = "tmux"
path = "/usr/bin/tmux"

# ~/.bashrc in the container: start tmux once, attaching if it is already there.
[[data]]
path   = "~/.config/lodge/tmux-rc"
target = "~/.bashrc"

# ~/.tmux.conf in the container.
[[data]]
path   = "~/.config/lodge/tmux.conf"
target = "~/.tmux.conf"
for    = "tmux"
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
`podman`, a running pod (`kubernetes`), or `host` for the machine itself (see below). `--target` on the command line
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

### Attaching to a container lodge did not make

`ldg attach CONTAINER` (a name or an id) works in a running container that something
else started, on the engine of the target (`--target docker|podman`, and `--remote`).
It injects, delivers secrets and opens a shell as `up` does, in the user's home or
`--workdir DIR`, but treats the container as somebody's workload, as `host` and
`kubernetes` do:

- Nothing is created, started or stopped: a container that is not running is refused.
- It acts as the user the container runs as.
- Data lodge did not place is left as it is, with a warning, and git's
  `safe.directory = *` is not set.
- Ports it listens on are forwarded as for any container.

`ldg clean --container CONTAINER` removes what was injected. A project's `up`, `down`
and `rm` do not know about it: it belongs to whoever made it.

### `host`: no container

`target = "host"` (or `--target host`) makes lodge work on the machine itself:
this one, or another through `--remote`. There is no devcontainer and no engine.
`ldg up [PATH]` injects your bundle into the user's prefix on that machine, delivers
secrets, and opens a session in `PATH` (the machine's login directory if you
leave it out over `--remote`).

Because it is a real machine, lodge is careful with it:

- It acts as the user the machine connects as, and cannot act as another (`root` or a
  named user is refused).
- Data lodge did not place is **left as it is**, with a warning, instead of
  replaced: `ldg up` never overwrites your own dotfiles. One it placed earlier is
  updated.
- git's `safe.directory = *` is not set. List folders in `[git] safe_directories`
  if you want some.
- The machine must be Linux, and have `tar`. The bundle holds Linux binaries.
- Nothing is created, so `down` and `rm` say so; `ls` has nothing to list;
  `ldg clean` removes the prefix and the wrappers, as for a container.
- Ports listening on a remote machine are forwarded through ssh to this one, as
  for a container; on this machine there is nothing to forward.

### `kubernetes`: a running pod

`target = "kubernetes"` (or `--target kubernetes`) makes lodge work in a pod that is
already running, through `kubectl`. It is run on the machine `--remote` names, or on
this one, so that machine needs `kubectl` and the cluster's credentials. Which pod
is a setting of the target:

```toml
[targets.kubernetes]
pod = "dev-0"            # kubectl exec dev-0 … (this or `selector`)
selector = "app=api"     # instead of `pod`: the one running pod with this label
container = "main"       # kubectl exec -c main (default: kubectl's own pick)
namespace = "work"       # kubectl --namespace work (default: the context's)
context = "staging"      # kubectl --context staging (default: the current one)
workdir = "/srv/app"     # where a session starts (default: the user's home)
```

**Per project.** A project says which pod it uses in a `lodge.toml` in its folder
(`ldg up [PATH]`'s PATH, else where you run `ldg`), read on this machine even with
`--remote`:

```toml
# my-project/lodge.toml
[targets.kubernetes]
pod = "api-dev-0"
namespace = "my-team"
```

It may hold nothing but `[targets.<name>]` tables, and only `kubernetes` takes one per
project. What it sets replaces the same key in your own `[targets.kubernetes]`, and
the rest of your table stands (a `pod` or `selector` in the project replaces how your
config found a pod, as the two are one setting). `-c FILE` still works for a
whole config of its own. The background port forwarder is told the folder, so it
works on the same pod.

Only `pod` or `selector` is needed, not both. A selector (`kubectl get pods -l`) is
for pods whose names change: it must match **exactly one running pod**, and more than
one is an error that lists them, because lodge does not choose which workload to
inject into. The pod is looked up once per `ldg` run (so the background port
forwarder, which keeps running, keeps the pod it found: run `ldg up` again after the
pod is replaced). The project path of `ldg up [PATH]` is a folder on the host, so
it plays no part: use `workdir`. `ldg up` checks that the pod is `Running`, injects
your bundle into the home of the user the pod runs as, delivers secrets, and opens a
session. A pod is someone's workload, so, as for `host`:

- It acts as the user the pod runs as, and cannot act as another: `kubectl exec` has
  no way to switch user, so `root` or a named user is refused.
- Data lodge did not place is left as it is, with a warning, and git's
  `safe.directory = *` is not set.
- The image needs `sh` and `tar`, and `cat` for the forwarder to see which ports
  listen. The bundle holds Linux binaries, so the pod's architecture must be one it
  was packed for.
- Nothing is created, so `down` and `rm` say so, and `ls` has nothing to list. A
  pod's files are lost when it is replaced: `ldg up` injects again into a new one.
  `--build` has nothing to rebuild and is ignored.
- Ports listening in the pod are forwarded to the same ports here with
  `kubectl port-forward`, one per port, started by the forward daemon and stopped
  with it. Ports of other pods (peers) are not forwarded. A port that is already in
  use here fails to forward, which the daemon's log (`ldg logs`) says.

## `bundle`

A top-level `bundle = "path/to/lodge.tar"` injects from that bundle instead of
packing the `[[binary]]` and `[[data]]` entries. A relative path is relative to the config
file's folder. A reference with a scheme is not a path: `bundle =
"oci://ghcr.io/me/lodge:1"` reads the bundle from a registry. `--bundle` on the
command line (a file or such a reference) overrides it. See
[Bundles and secrets](bundles-and-secrets.md).

`bundles = ["oci://ghcr.io/me/lodge:2", "~/work.tar"]` lists others to choose
from, written like `bundle`. With `bundle` (or the packed config, when it is not
set) they are what `ldg up` offers when it injects into a project it has not opened
before. `ldg bundle use|add|rm|ls` change both keys without opening the file, and
`ldg pack -o` adds what it writes. See
[Several bundles](bundles-and-secrets.md#several-bundles).

## `[[secret]]`

Values (API keys, tokens, credential files) delivered when you attach, and
never stored in a bundle. See [Bundles and secrets](bundles-and-secrets.md#secrets)
for the fields and how delivery works.
