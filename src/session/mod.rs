//! The interactive session: a login shell in the container.
//!
//! lodge starts a shell and nothing else. Keeping a session alive across a
//! disconnect is the user's business: a multiplexer they inject and start from
//! their shell's rc file is just another program in that shell.
//!
//! The session is what puts lodge's tools within reach: it makes the user's
//! prefix `bin/` the first directory on `PATH` ([`startup_script`]), so that
//! nothing outside the prefix needs a wrapper or a hook in a startup file.

pub mod terminfo;

use crate::config::SessionConfig;
use crate::host::Host;
use crate::layout::Layout;
use crate::local::Local;
use crate::output::{info, note, warning};
use crate::runtime::{Acting, Container, ContainerUser};
use crate::shell::shell_quote;
use anyhow::Result;
use std::io::IsTerminal as _;
use std::time::{Duration, Instant};

/// Where and how to open the session.
pub struct Session<'a> {
    pub workdir: &'a str,
    /// Who the shell runs as.
    pub user: &'a ContainerUser,
    /// Where lodge's tools are for that user.
    pub layout: &'a Layout,
    /// Shell lines run before the login shell (secret exports).
    pub setup: &'a str,
}

/// The `sh -c` script that starts the session's shell.
///
/// `preamble` runs first (secret exports, `TERM`, the window title). Then the
/// shell is started *as a login shell*, as before, so `/etc/profile`, the
/// profile scripts and the user's own profile still run. Those may reset `PATH`
/// (Debian's `/etc/profile` does), which would hide lodge's tools and drop the
/// directories the image put on `PATH`, so once they have run, a small POSIX
/// script puts the prefix's `bin/` first and appends whatever the container had
/// that the profile dropped, and only then executes the shell, interactively. A
/// shell started that way reads its interactive startup files (`.bashrc`,
/// `.zshrc`) as usual. The script is plain `sh`, run through the login shell with
/// `exec sh -c`, so it works with any shell that has `-l` and `-c`.
pub(crate) fn startup_script(layout: &Layout, shell: &str, preamble: &str) -> String {
    let finish = path_script(layout);
    let after_login = format!(
        "exec sh -c {} sh {} -i",
        shell_quote(&finish),
        shell_quote(shell)
    );
    format!(
        "{preamble}LODGE_ORIG_PATH=\"$PATH\"; export LODGE_ORIG_PATH; exec {} -lc {}",
        shell_quote(shell),
        shell_quote(&after_login)
    )
}

/// The script that runs after the login shell has set up its environment, with
/// the command to run as its arguments: `PATH` becomes the prefix's `bin/`, then
/// what the login shell made it, then the container's own entries that are
/// missing from that (`$LODGE_ORIG_PATH`, set before the login shell started).
fn path_script(layout: &Layout) -> String {
    format!(
        "set -f; p=$PATH; IFS=:; \
         for d in $LODGE_ORIG_PATH; do case \":$p:\" in *\":$d:\"*) ;; *) p=\"$p:$d\";; esac; done; \
         unset IFS LODGE_ORIG_PATH; PATH={}:$p; export PATH; exec \"$@\"",
        shell_quote(&layout.bin())
    )
}

/// Opens an interactive login shell in `container` (on the engine on `host`),
/// wrapped as `config` says. `local` is the terminal it is opened from.
pub fn enter(
    container: &dyn Container,
    host: &dyn Host,
    local: &dyn Local,
    config: &SessionConfig,
    s: &Session,
) -> Result<()> {
    let shell = &config.shell;

    // `container` may be a raw hex id for compose-based devcontainers, which
    // makes an unreadable title — resolve it back to the engine-assigned name
    // where possible.
    let title = container.display_name();

    // Setting the terminal title *before* `docker exec`/`ssh` runs doesn't
    // stick: kitty (and others) track the foreground process of the pty and
    // revert to showing its name (`docker`) the moment it becomes the active
    // job, discarding any title set beforehand. So the title has to be
    // (re)asserted from *inside* the exec'd command, once it's already the
    // foreground process, right before dropping into the interactive shell.
    let set_title = format!("printf '\\033]2;%s\\007' {}", shell_quote(&title));
    // `docker exec -it` doesn't pass the client's TERM on (the container would see
    // plain xterm), so the session sets it, after copying in the entry for the
    // terminal this session is opened from when the container lacks it. Best
    // effort: a missing entry only costs some colours.
    let env = Acting::new(container, s.user);
    let terminfo_dir = s.layout.terminfo();
    let term_setup = terminfo::prepare(
        &env,
        &terminfo_dir,
        terminfo::local_term(local).as_deref(),
        local,
    )
    .map(|t| terminfo::session_setup(&terminfo_dir, &t))
    .unwrap_or_default();
    let inner = startup_script(
        s.layout,
        shell,
        &format!("{}{term_setup}{set_title}; ", s.setup),
    );

    // Opens the interactive session; how it reaches the container (docker exec,
    // through the host's terminal) is the environment's business. Can be started
    // again.
    let start = || container.interactive(s.user.exec_as(), s.workdir, &inner);
    let container_id = container.id();

    info!("entering {container_id}");

    let mut started = Instant::now();
    let mut status = start()?;
    // The connection dropped under a session that had been in use. The shell on
    // the other side is gone (or going), so a new one is opened, without asking:
    // lodge cannot know whether the user's shell setup attaches to something that
    // outlived the disconnect (it knows no multiplexer), but if it does, that is
    // where they land, and if it does not, they are where a new shell puts them.
    // Not after an instant failure, which is the connection never coming up, and
    // not without a terminal, where nobody is there to want it. A connection that
    // does not come back at once is tried for a while, and Ctrl-C gives up.
    while host.link_lost(status, &[])
        && started.elapsed() >= Duration::from_secs(2)
        && std::io::stdin().is_terminal()
    {
        let to = host.label().map(|h| format!(" to {h}")).unwrap_or_default();
        note!("lost the connection{to}; reconnecting (Ctrl-C gives up)");
        reconnect(
            || host.connect(),
            RECONNECT_TRIES,
            RECONNECT_WAIT,
            std::thread::sleep,
        )?;
        info!("entering {container_id}");
        started = Instant::now();
        status = start()?;
    }
    let result = if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("session exited with failure"))
    };

    // A remote session that fails almost instantly is the signature of the host
    // refusing a PTY: `docker exec -it` then has no tty on its stdin and
    // bails the moment the shell opens. Probe for it (only on a fast failure, so
    // a normal shell exiting non-zero after real use doesn't trigger an extra
    // connection) and point at the fix, which is server-side — no client flag,
    // not even `-tt`, can override it.
    if result.is_err() && started.elapsed() < Duration::from_secs(2) && host.pty_denied() {
        warning!(
            "the remote SSH server refused a PTY for this connection, so the\n\
             interactive shell (`docker exec -it`) was dropped immediately. Fix it\n\
             on the server: set `PermitTTY yes` in sshd_config, and remove any\n\
             `restrict`/`no-pty` option on this key in ~/.ssh/authorized_keys, then\n\
             reload sshd."
        );
    }
    result
}

/// How often, and how far apart, a lost connection is tried again: about two
/// and a half minutes, which rides out a laptop changing networks.
const RECONNECT_TRIES: u32 = 30;
const RECONNECT_WAIT: Duration = Duration::from_secs(5);

/// Calls `connect` until it succeeds, at most `tries` times with `wait` between
/// them (given to `sleep`, which a test replaces); the last failure is the answer.
fn reconnect(
    mut connect: impl FnMut() -> Result<()>,
    tries: u32,
    wait: Duration,
    sleep: impl Fn(Duration),
) -> Result<()> {
    let mut attempt = 1;
    loop {
        match connect() {
            Ok(()) => return Ok(()),
            Err(e) if attempt >= tries => {
                return Err(e.context(format!("could not reconnect after {tries} tries")));
            }
            Err(_) => {
                attempt += 1;
                sleep(wait);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::tmpfile::TempDir;
    use std::process::Command;

    fn layout() -> Layout {
        Layout::new("/home/dev/.local/share/lodge")
    }

    /// Runs `script` with `sh -c`, the way the container does.
    #[cfg(unix)]
    fn sh(script: &str, path: &str) -> String {
        let out = Command::new("sh")
            .args(["-c", script])
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The `PATH` a command run by [`path_script`] sees.
    fn path_after(login: &str, orig: &str) -> String {
        let out = Command::new("sh")
            .args(["-c", &path_script(&layout()), "sh", "printenv", "PATH"])
            .env("PATH", login)
            .env("LODGE_ORIG_PATH", orig)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[cfg(unix)]
    #[test]
    fn the_prefix_comes_first_then_the_login_path_then_what_the_login_dropped() {
        let bin = "/home/dev/.local/share/lodge/bin";
        assert_eq!(
            path_after("/usr/bin:/bin", "/opt/go/bin:/usr/bin:/bin"),
            format!("{bin}:/usr/bin:/bin:/opt/go/bin"),
            "an entry the profile dropped is put back, the rest is not repeated"
        );
        assert_eq!(
            path_after("/home/dev/.nvm/bin:/usr/bin", "/usr/bin"),
            format!("{bin}:/home/dev/.nvm/bin:/usr/bin"),
            "what the profile added stays ahead of the image's own"
        );
        assert_eq!(path_after("/usr/bin", ""), format!("{bin}:/usr/bin"));
    }

    #[cfg(unix)]
    #[test]
    fn path_entries_with_spaces_or_wildcards_are_kept_literally() {
        let bin = "/home/dev/.local/share/lodge/bin";
        let got = path_after("/usr/bin", "/opt/my tools/bin:/opt/*/bin");
        assert_eq!(got, format!("{bin}:/usr/bin:/opt/my tools/bin:/opt/*/bin"));
    }

    #[test]
    fn the_helper_variable_does_not_leak_into_the_shell() {
        let out = Command::new("sh")
            .args([
                "-c",
                &path_script(&layout()),
                "sh",
                "sh",
                "-c",
                "printf %s \"${LODGE_ORIG_PATH-unset}\"",
            ])
            .env("LODGE_ORIG_PATH", "/x")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "unset");
    }

    #[cfg(unix)]
    #[test]
    fn a_login_shell_that_resets_path_still_ends_up_with_the_tools_first() {
        // A stand-in shell: as a login shell (`-lc CMD`) it resets PATH like
        // Debian's /etc/profile does and runs CMD; as `-i` it reports its PATH.
        let dir = TempDir::new("fake-shell").unwrap();
        let shell = dir.path().join("fakeshell");
        std::fs::write(
            &shell,
            "#!/bin/sh\ncase \"$1\" in\n  -lc) export PATH=/usr/bin:/bin; eval \"$2\" ;;\n  -i) printf '%s\\n' \"$PATH\" ;;\nesac\n",
        )
        .unwrap();
        crate::platform::set_mode(&shell, 0o755).unwrap();

        let script = startup_script(&layout(), &shell.to_string_lossy(), "");
        assert_eq!(
            sh(&script, "/opt/go/bin:/usr/bin:/bin"),
            "/home/dev/.local/share/lodge/bin:/usr/bin:/bin:/opt/go/bin"
        );
    }

    #[test]
    fn the_preamble_runs_first_and_the_shell_is_a_login_shell() {
        let s = startup_script(&layout(), "/bin/bash", "export A=1; ");
        assert!(s.starts_with("export A=1; "), "{s}");
        assert!(s.contains("exec '/bin/bash' -lc "), "{s}");
        assert!(s.contains("-i"), "{s}");
    }

    #[test]
    fn a_shell_path_with_odd_characters_is_quoted() {
        let s = startup_script(&layout(), "/opt/my shell/bash", "");
        assert!(s.contains("'/opt/my shell/bash'"), "{s}");
    }

    #[test]
    fn a_connection_that_comes_back_is_used_and_one_that_does_not_gives_up() {
        use std::cell::{Cell, RefCell};
        let slept = RefCell::new(Vec::new());
        let calls = Cell::new(0);
        let wait = Duration::from_secs(5);
        // Fails twice, then works: two waits, and the session goes on.
        reconnect(
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    anyhow::bail!("no route")
                }
                Ok(())
            },
            30,
            wait,
            |d| slept.borrow_mut().push(d),
        )
        .unwrap();
        assert_eq!((calls.get(), slept.borrow().len()), (3, 2));
        assert!(slept.borrow().iter().all(|d| *d == wait));
        // Never works: tried the given number of times, the last reason kept.
        calls.set(0);
        let err = reconnect(
            || {
                calls.set(calls.get() + 1);
                anyhow::bail!("no route {}", calls.get())
            },
            4,
            wait,
            |_| {},
        )
        .unwrap_err();
        assert_eq!(calls.get(), 4);
        let text = format!("{err:#}");
        assert!(
            text.contains("could not reconnect after 4 tries") && text.contains("no route 4"),
            "{text}"
        );
    }
}
