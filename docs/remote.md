# Remote containers

`ldg -r <dest> up PATH` (or `--remote`) runs the docker CLI *on the remote host*
over SSH (`ssh <dest> docker …`), so **no local docker is required**, only an
`ssh` client. The project path is the path *on the remote host*; the
devcontainer config is read there, `initializeCommand` and builds run there, and
features are installed in the remote container. Port forwarding tunnels ports
back to `127.0.0.1` on your machine.

The destination is anything your SSH client understands:

- `user@host`: explicit user and host.
- `myhost`: a `Host` alias from `~/.ssh/config`; its `User`, `Port`,
  `IdentityFile`, jump hosts and so on all apply.
- `user@host:2222`: a non-standard SSH port (becomes `-p 2222`).

`docker` and `docker compose` must be installed on the remote host. Your
injected environment lives locally and is streamed into the remote container
over the SSH connection: lodge packs directories into a tar stream itself (no
local `tar` needed) and the remote daemon unpacks it.

Change detection reads the `.devcontainer/` files from the remote host and
hashes them here, the same way as for a local project, so a comment-only edit
doesn't count as a change either.

lodge opens one SSH connection and reuses it for the dozens of commands a single
`ldg up` issues (a `ControlMaster` socket under `$TMPDIR/lodge-ssh/`).
Connections must be non-interactive (key or agent auth). If something fails, run
with `-v` to see each command, and `-vv`/`-vvv` for ssh's own debug output.

From a machine that isn't your Linux workstation (macOS, Windows, a CI runner),
combine `--remote` with a bundle: see [Bundles and secrets](bundles-and-secrets.md).
