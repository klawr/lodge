# Troubleshooting

Start with `ldg doctor` (it checks the engine, compose, helper tools, your config,
each listed binary and each secret's source), and re-run the failing command
with `-v` to see every docker/ssh command lodge runs.

## The config is rejected

```
[lodge] error: invalid config ~/.config/lodge/config.toml: unknown field `copy_dep`, … (line 4); did you mean `copy_deps`?
```

lodge rejects keys it doesn't know rather than ignoring them, so a typo can't
silently do nothing. Fix the line it names. Add the `#:schema` line from
[Configuration](configuration.md) to get completion and checking in your editor.

## "the remote SSH server refused a PTY"

The interactive shell needs a terminal on the remote side. Set `PermitTTY yes`
in the server's `sshd_config`, remove any `restrict` or `no-pty` option on your
key in `~/.ssh/authorized_keys`, and reload sshd. No client flag can override it.

## "lost the connection to <host>"

With `--remote`, lodge reconnects and reruns a step that is safe to repeat (up
to three tries); you see `retrying (2/3)` and nothing else if it works. A step
that is not safe to repeat (a lifecycle hook, a feature's `install.sh`) is not
rerun: run the same command again, which picks up where it stopped. If the
connection drops under your shell, `ldg` asks whether to reconnect and open a new
one; to land back in your work, keep a multiplexer running in the container (see
[Keeping a session alive](configuration.md#keeping-a-session-alive)).
`cannot connect to <host>` after the retries means the host is down or refused
the login: run with `-vv` for ssh's own output.

## Ports aren't forwarded

- `ldg ports` shows what the forwarding daemon has set up, `ldg logs` (or
  `ldg logs -f`) why something wasn't.
- "could not determine container IP": the container has no bridge address the
  engine host can reach. With Docker Desktop on macOS/Windows, container IPs
  aren't reachable from the host; use `appPort` (published on loopback) or
  `--remote`.
- "could not forward port N": something on this machine already uses that port.
- A service listening on `127.0.0.1` *inside* the container can't be reached
  from outside it; make it listen on `0.0.0.0`.

## git: "detected dubious ownership"

The workspace is owned by your host uid, which git in the container may not
trust. lodge registers `safe.directory = *` by default (`[git]
safe_directories`); if you set it to `[]`, add the directories you need.

## An injected tool fails with "not found" or a missing library

- `ldg doctor` checks each `binary` exists and shows its architecture; the
  container must run the same one.
- A library loaded at run time (`dlopen`, a plugin, an interpreter) is invisible
  to `ldd`. Add it as its own `[[inject]]` entry (`config` = the library or its
  folder, `target_config` = where the tool looks).
- Neovim needs its runtime at the path it was built with: inject
  `<prefix>/share/nvim/runtime` to the same path (`ldg init` adds this entry).
- Run `ldg cache clear` if you suspect a stale packed bundle.

## The devcontainer is reported as changed

Change detection hashes what the container is built from: `devcontainer.json`
(comments and formatting do not count), the Dockerfile, the compose files and
local feature folders. Other files under `.devcontainer/` (scripts, a hook's
output) are not part of it. Files the image build copies in from its build
context are not either; `ldg up --build` picks them up.

## A lifecycle hook or feature failed

Hooks and features are reported and skipped, never fatal, and a failed
create-time hook or feature is retried on the next `ldg up`. Hooks run as
`remoteUser`, feature scripts as root. Run `ldg up -v` to see the exact command.

## Removing lodge from a container

`ldg clean` removes lodge's directory (`~/.local/share/lodge` of the container
user, or `/tmp/lodge-<uid>`), the wrapper scripts it wrote, the git `safe.directory` entries it added and the
placed secrets, and lists the configs it left in place. `ldg rm` removes the whole container. lodge keeps nothing about a container on the host except the
bundle cache (`ldg cache clear`) and the forwarding logs in
`~/.local/share/lodge/`.
