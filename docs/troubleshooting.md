# Troubleshooting

Start with `ldg doctor` (engine, compose, helper tools, config, each binary and each
secret's source; for a pod, `kubectl` and the pod), and rerun the failing command with
`-v` to see every docker/ssh command.

## The config is rejected

```
[lodge] error: invalid config ~/.config/lodge/config.toml: unknown field `copy_dep`, … (line 4); did you mean `copy_deps`?
```

Unknown keys are errors, so a typo cannot silently do nothing. Fix the named line. The
`#:schema` line from [Configuration](configuration.md) gives you checking in your editor.

## "the remote SSH server refused a PTY"

The shell needs a terminal on the remote side. Set `PermitTTY yes` in the server's
`sshd_config`, remove any `restrict` or `no-pty` on your key in `authorized_keys`, and
reload sshd. No client flag can override it.

## "lost the connection to <host>"

With `--remote`, lodge retries a repeat-safe step up to three times (you see `retrying
(2/3)`); an unsafe step (a hook, a feature's `install.sh`) is not rerun: run the same
command again. If the connection drops under your shell, `ldg` reconnects (about two and
a half minutes; Ctrl-C gives up) and opens a new shell, which reads your rc file again,
so [keep a multiplexer](configuration.md#keeping-a-session-alive) to land back in your
work. Without a terminal it does not reconnect. `cannot connect to <host>` after the
retries means the host is down or refused the login: use `-vv`.

## Ports are not forwarded

`ldg ports` shows what is forwarded; `ldg logs -f` says why something is not.

- "could not determine container IP": no bridge address the engine host can reach
  (Docker Desktop, rootless Podman). Publish the port with `appPort`, or use `--remote`.
- "could not forward port N": something here already uses it.
- A service listening on `127.0.0.1` *inside* the container is unreachable from outside;
  make it listen on `0.0.0.0`.

## git: "detected dubious ownership"

The workspace is owned by your host uid. lodge sets `safe.directory = *` by default; if
you set `[git] safe_directories = []`, list the folders you need.

## An injected tool fails: "not found" or a missing library

- `ldg doctor` shows each binary's architecture; the container must run the same one.
- A library loaded at run time (`dlopen`, a plugin) is invisible to `ldd`: add it as a
  `[[data]]` entry.
- A program that runs from where it was installed (Neovim) needs the files beside its
  binary ("module 'vim.uri' not found" means they are missing): `ldg inject add data PATH
  --for NAME`.
- `ldg cache clear` if you suspect a stale packed bundle.

## The devcontainer is reported as changed

Change detection hashes `devcontainer.json` (comments and formatting do not count), the
Dockerfile, compose files and local feature folders. Other files under `.devcontainer/`
and the Dockerfile's build context are not inputs; `ldg up --build` picks those up.

## A lifecycle hook or feature failed

Hooks and features are reported and skipped, never fatal, and a failed create-time one
is retried on the next `ldg up`. Hooks run as `remoteUser`, features as root. `ldg up -v`
shows the exact command.

## A bundle or a feature cannot be pulled or published

lodge pulls and publishes itself ([features](devcontainers.md#features),
[bundles](bundles-and-secrets.md#keeping-a-bundle-in-a-registry)), and the error names
the step that failed.

- **`not found`**: no such repository or tag. Check the reference.
- **`refused … (401)`, `the token service refused`, `needs a login`**: private, or it
  wants a login. Run `ldg registry login <registry>` (with 2FA the password is an access
  token). At a terminal lodge asks on a refusal; with no terminal (a pipe, the forward
  daemon) keep a login first. Docker's own login is read from its config file or the
  credential helper it names (`docker-credential-<name>`); a missing or locked helper, or
  one holding only an identity token, fails with a message saying so.
- **`content is sha256:…, expected …`, `the registry stored <digest>, not …`**: something
  between you and the registry (a proxy, a mirror) changed the bytes. Nothing is kept;
  retry or look at what sits in between.
- **`cannot reach oci://… ; using the bundle it named last time`**: the registry is down
  and the cached bundle may be out of date (`ldg up --reinject` once online). `… not in
  the cache either`: never fetched here.
- **`no image for linux/<arch>`**, **`has N layers`**, **`names a digest`**: the artifact
  or reference is not shaped as lodge needs (an index without your architecture, more than
  one layer, a digest where a tag is needed).

`ldg up -v` prints each request. "Not now" answers (429, 5xx) are retried automatically;
on a flaky line run `ldg up` again (an installed feature is not installed twice). Behind
a proxy set `HTTPS_PROXY` (and `NO_PROXY`).

## Secrets and sessions

```
[lodge] warning: secret not delivered: secret CLAUDE_CODE_OAUTH_TOKEN: store entry claude_token: is not set; add it with `ldg secret set`
```

The credential store has nothing under that name on this machine. `ldg secret set
claude_token`, on every client that should deliver it; `ldg secret ls` shows what is set.
For `env`, `file` and `command` sources the message names that source (never the value).

```
no credential store is available here (…); on Linux that is a Secret Service …
```

A `store` source needs an OS credential store; a headless Linux server has none. Use
`env`, `file` or `command`.

```
[lodge] error: kai@desktop has a session open in <container>; attaching would end it and remove its secrets. Run this from a terminal to confirm
```

One lodge session at a time: the same user and machine is replaced silently, another
client is asked, and with no terminal there is nobody to ask. Run `ldg up` from a
terminal, or end the other session where it is.

**`this session was ended: bob@desktop attached to <container>`**: another client took
the container, and the secrets are now theirs. Attach again to take it back (they will be
asked).

**A tool lost its login after I attached from another machine.** Attaching replaces the
secrets with that client's, so a `file` secret the tool read is gone. A login the tool
made inside the container is not touched
([which credentials to inject](bundles-and-secrets.md#which-credentials-to-inject)).

**A secret is missing in a shell I left running.** Secrets go when their session ends;
an `env` secret stays in the environment of processes it started, a `file` secret does not.

## A plain host

`ldg --target host up` works as the user you connect as. "already exists and was not
injected by lodge; leaving it as it is" is on purpose: lodge never overwrites your files
on a real machine. Move the file, then `ldg up --reinject`. "The bundle's binaries are for
Linux" means the machine is not Linux; "cannot act as another user" means a
`remoteUser`-style request, which a host cannot honour.

## Removing lodge from a container

`ldg clean` removes lodge's folder (`~/.local/share/lodge`, or `/tmp/lodge-<uid>`), the
wrappers, the `safe.directory` entries it added and the placed secrets, and lists the data
it left. `ldg rm` removes the container. On the host lodge keeps only the bundle cache
(`ldg cache clear`), the history and the forwarding logs in `~/.local/share/lodge/`.
