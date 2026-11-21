# Troubleshooting

Start with `ldg doctor` (it checks the engine, compose, helper tools, your config,
each listed binary and each secret's source), and re-run the failing command
with `-v` to see every docker/ssh command lodge runs.

## The config is rejected

```
[lodge] error: invalid config ~/.config/lodge/config.toml: unknown field `skip_if_exist`, … (line 4); did you mean `skip_if_exists`?
```

lodge rejects keys it doesn't know rather than ignoring them, so a typo can't
silently do nothing. Fix the line it names. Add the `#:schema` line from
[Configuration](configuration.md) to get completion and checking in your editor.

## "the remote SSH server refused a PTY"

The interactive shell needs a terminal on the remote side. Set `PermitTTY yes`
in the server's `sshd_config`, remove any `restrict` or `no-pty` option on your
key in `~/.ssh/authorized_keys`, and reload sshd. No client flag can override it.

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

## The devcontainer is recreated (or the question asked) every time

Change detection hashes every file under `.devcontainer/`. A hook that writes
there (for example `initializeCommand: touch .devcontainer/x`) changes the hash
on every run. Write generated files elsewhere. In scripts, `--no-recreate` keeps
the container without asking.

## A lifecycle hook or feature failed

Hooks and features are reported and skipped, never fatal, and a failed
create-time hook or feature is retried on the next `ldg up`. Hooks run as
`remoteUser`, feature scripts as root. Run `ldg up -v` to see the exact command.

## Removing lodge from a container

`ldg clean` removes lodge's directory (`~/.local/share/lodge` of the container
user, or `/tmp/lodge-<uid>`), the wrapper scripts it wrote and the placed secrets,
and lists the configs it left in place. With `--container NAME` it cleans for the
container's default user, or for the one given with `-u USER`. `ldg rm` removes the
whole container. lodge keeps nothing about a container on the host except the
bundle cache (`ldg cache clear`) and the forwarding logs in
`~/.local/share/lodge/`.
