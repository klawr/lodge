# Troubleshooting

Start with `ldg doctor` (it checks the engine (for a pod: `kubectl` and the pod), compose, helper tools, your config,
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
connection drops under your shell, `ldg` reconnects by itself (trying for about
two and a half minutes; Ctrl-C gives up) and opens a new shell, which reads your
rc file again; to land back in your work, keep a multiplexer running in the
container and attach to it from there (see
[Keeping a session alive](configuration.md#keeping-a-session-alive)). Without a
terminal (a script) it does not reconnect.
`cannot connect to <host>` after the retries means the host is down or refused
the login: run with `-vv` for ssh's own output.

## Ports aren't forwarded

- `ldg ports` shows what the forwarding daemon has set up, `ldg logs` (or
  `ldg logs -f`) why something wasn't.
- "could not determine container IP": the container has no bridge address the
  engine host can reach. With Docker Desktop (macOS, Windows), container IPs
  aren't reachable from the host; use `appPort` (published on loopback) or
  `--remote`.
- With rootless Podman a container has no address the host can reach, so only the
  ports it publishes are forwarded (publish them with `appPort`); the log says once
  per other port that it is not reachable.
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

## A bundle or a feature cannot be pulled or published

A devcontainer feature or an `oci://` bundle is pulled from its registry, and a
bundle is published to one, by lodge itself (see [Devcontainers](devcontainers.md#features) and
[Bundles](bundles-and-secrets.md#keeping-a-bundle-in-a-registry)), and the error
names the step that failed:

- `not found: ghcr.io/owner/name`: the registry has no such repository or tag.
  Check the reference, including the tag.
- `… refused owner/name (401)`: the repository is private, or needs a login. Run
  `docker login <registry>`. lodge reads that login only if docker stored it in
  its config file; if `credsStore` or `credHelpers` is set there, the login lives
  in a helper that lodge does not ask, and the registry is reached anonymously.
  `the token service refused (…)` is the same thing one step earlier.
- `… needs a login (docker login <registry>)`: the registry asks for basic auth and
  there is no login.
- `content is sha256:…, expected sha256:…`, or `is N bytes, the manifest says M`:
  what came is not what the registry's manifest promised (a broken proxy or
  mirror). Nothing is kept; retry, or check what sits between you and the registry.
- `cannot reach oci://… (…); using the bundle it named last time`: the registry
  could not be reached, so the bundle this tag named when it last could be was used
  from the cache. It may be out of date; `ldg up --reinject` takes the new one once
  you are online. `… is not in the cache either, so there is nothing to use
  offline`: this tag was never fetched on this machine; use a file, or get online.
- `… refused to write <repository> (401)`, when publishing with `ldg pack -o
  oci://…`: run `docker login <registry>` with an account that may write there.
  As for a pull, the login is read from docker's config file, or from the
  credential helper it names (`credsStore`, `credHelpers`).
- `docker-credential-<name> get for <registry> failed: …` or `asking
  docker-credential-<name> …`: the helper the config names is not installed, or it
  failed (a locked keyring). Fix it, or `docker login` again. A helper that holds
  an identity token (`<token>`, as some cloud logins do) cannot be used: log in with
  a user name and an access token. A helper with nothing for the registry means an
  anonymous request.
- `the registry stored <digest>, not the <digest> that was sent`: the registry
  changed the content it was given (a proxy in between). Nothing is usable under
  that tag; publish again, or find what sits between you and the registry.
- `names a digest`: a bundle is published to a tag (`oci://…/name:1`), not to a
  digest.
- `has N layers, a feature has exactly one`, or `has more than one layer, a bundle
  has exactly one`: the artifact is not shaped as lodge needs.
- `no image for linux/<arch> (it has …)`: an index with no entry for this
  machine's architecture. `the reference is an index of N images and no
  architecture was asked for` means lodge did not know this machine's CPU.
- `cannot read a bundle from "<x>"; it is a file path or oci://…`: the
  reference has a scheme lodge does not read.
- A pull that stops moving fails after 30 seconds without data. Every other
  failure to connect or answer is reported as it came (`requesting <url>`).

`ldg up -v` prints each registry request as `GET <url>`. A request the registry
answers with "not now" (429, 502, 503, 504) is retried three times, 1, 3 and 9
seconds apart; any other failure is not, so on a flaky connection run `ldg up`
again: a feature that was already installed is not installed twice. A registry
reached through a proxy needs `HTTPS_PROXY` (and `NO_PROXY` for what is not).

## A plain host

`ldg --target host up` works on the machine itself, as the user you connect as. If
it says a config "already exists and was not injected by lodge; leaving it as it
is", that is on purpose: lodge does not overwrite your files on a real machine.
Remove or move the file if you want the bundle's version, then `ldg up --reinject`.
"The bundle's binaries are for Linux" means the machine is not Linux; "cannot act as
another user" means a `remoteUser`-style request was made of a host, which only
has the login user.

## Removing lodge from a container

`ldg clean` removes lodge's directory (`~/.local/share/lodge` of the container
user, or `/tmp/lodge-<uid>`), the wrapper scripts it wrote, the git `safe.directory` entries it added and the
placed secrets, and lists the configs it left in place. `ldg rm` removes the whole container. lodge keeps nothing about a container on the host except the
bundle cache (`ldg cache clear`) and the forwarding logs in
`~/.local/share/lodge/`.
