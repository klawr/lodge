# Remote containers

`ldg -r <dest> up PATH` (or `--remote`) runs the docker CLI *on the remote host*
over SSH (`ssh <dest> docker …`), so **no local docker is required**, only an
`ssh` client. The project path is the path *on the remote host*; the
devcontainer config is read there, `initializeCommand` and builds run there, and
features are installed in the remote container. Port forwarding tunnels ports
back to `127.0.0.1` on your machine.

`n` in a bare `ldg` finds a new project on this machine or on another one: it asks
where (this machine, a machine you used before, or `another machine…`, typed as below),
connects, and lists that machine's projects exactly as it lists local ones (one `find`
on the remote host, so the list comes at the cost of one round trip). The projects are
those with a devcontainer config under the remote home, `~` and relative paths mean
the remote home, and `--remote` on the command line settles the question. A machine
that can't be reached is asked for again.

The destination is anything your SSH client understands:

- `user@host`: explicit user and host.
- `myhost`: a `Host` alias from `~/.ssh/config`; its `User`, `Port`,
  `IdentityFile`, jump hosts and so on all apply.
- `user@host:2222`: a non-standard SSH port (becomes `-p 2222`).

`docker` and `docker compose` must be installed on the remote host. Your
injected environment lives locally and is streamed into the remote container
over the SSH connection: lodge packs directories into a tar stream itself (no
local `tar` needed) and the remote daemon unpacks it.

Change detection reads the config, Dockerfile, compose files and local
features from the remote host and hashes them here, the same way as for a local project, so a comment-only edit
doesn't count as a change either.

lodge opens one SSH connection first and reuses it for the dozens of commands a
single `ldg up` issues (a `ControlMaster` socket under `$TMPDIR/lodge-ssh/`, kept
for 10 minutes after the last use; Windows has none, see [From Windows](#from-windows)). It opens it before anything else, with your
terminal available, so a password, passphrase or 2FA prompt is answered once.
The port-forwarding daemon has no terminal: it rides on that connection, or on
key or agent auth if it has to reconnect. None of its ssh commands ever prompts. If the
connection is lost for good on a host that needs a password or a second factor,
the daemon cannot log in again. It says so in its log and waits instead of
exiting, looking every 20 s (each look is a failed login, which a server may
count against you); the next `ldg` command you run in a terminal opens the
connection again, and the forwards come back by themselves.

A link that goes quiet is dropped after about 45 seconds (`ServerAliveInterval=15`,
`ServerAliveCountMax=3`). When the connection is lost during a step that is safe
to repeat (a read, `mkdir -p`, copying files in, starting a container), lodge
reconnects and runs it again, up to three tries, waiting 1 s then 2 s. A step that
is not safe to repeat (your lifecycle hooks, a feature's `install.sh`) is not
rerun: lodge says so, and running the same command again picks up where it
stopped. A dead forwarded port is restarted with a growing wait, up to a minute.
Rejected logins and host-key failures are not retried.

If something fails, run with `-v` to see each command, and `-vv`/`-vvv` for ssh's
own debug output. That goes to a file beside the connection's socket (`ldg` says
where), not to the terminal: the connection's background process would keep the
terminal's output open for as long as it lives.

From a machine that isn't your Linux workstation (a CI runner, or Windows),
combine `--remote` with a bundle: see [Bundles and secrets](bundles-and-secrets.md).

## From Windows

`ldg` runs natively on Windows as a client for a Linux host (see the
[README](../README.md#windows)). It uses the `ssh` that ships with Windows, whose
OpenSSH has no `ControlMaster`, so lodge does not open a shared connection: each of
the dozens of commands an `ldg up` issues connects, authenticates and ends on its
own. That is slower than on Linux, and it means:

- Log in with a **key or an agent** (`ssh-agent` with the key added). A password,
  a passphrase that is not in the agent or a second factor would be asked for by
  every command, and the port-forwarding daemon, which has no terminal, could
  never answer it.
- `ldg` still makes one connection first, with the terminal available, to tell you
  early if the host cannot be reached.
- The forwarding daemon runs in the background without a console window; `ldg
  ports` and `ldg logs` show and explain it as elsewhere.

The `Host` aliases, `IdentityFile`, jump hosts and ports of `%USERPROFILE%\.ssh\config`
apply as on any ssh.
