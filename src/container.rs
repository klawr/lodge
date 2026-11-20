use crate::config::Config;
use crate::output::{info, note, warning};
use crate::runtime::Container;
use crate::shell::shell_quote;
use anyhow::{Result, bail};
use std::process::Command;
use std::time::{Duration, Instant};

pub fn enter(
    container: &dyn Container,
    session_name: &str,
    workdir: &str,
    remote_user: Option<&str>,
) -> Result<()> {
    let config = Config::load()?;
    let shell = &config.session.shell;

    // `container` may be a raw hex id for compose-based devcontainers, which
    // makes an unreadable title — resolve it back to the engine-assigned name
    // where possible.
    let title = container
        .name()
        .unwrap_or_else(|| container.id().to_string());

    // Setting the terminal title *before* `docker exec`/`ssh` runs doesn't
    // stick: kitty (and others) track the foreground process of the pty and
    // revert to showing its name (`docker`) the moment it becomes the active
    // job, discarding any title set beforehand. So the title has to be
    // (re)asserted from *inside* the exec'd command, once it's already the
    // foreground process, right before dropping into the interactive shell.
    let set_title = format!("printf '\\033]2;%s\\007' {}", shell_quote(&title));
    // `docker exec -it` doesn't pass the client's TERM on (the container would see
    // plain xterm), so the session sets it, and points ncurses at any terminfo
    // entry copied in.
    let term_setup =
        crate::terminfo::session_term(container, crate::terminfo::local_term().as_deref())
            .map(|t| crate::terminfo::session_setup(&t))
            .unwrap_or_default();
    let inner = format!("{term_setup}{set_title}; exec {} -l", shell_quote(shell));

    // The local command line that opens the interactive session; how it reaches
    // the container (docker exec, over ssh -tt for a remote engine) is the
    // environment's business.
    let run_cmd = container.interactive_command(remote_user, workdir, &inner);
    let container_id = container.id();
    let target = container.target();

    info!("entering {container_id}");
    if crate::output::is_verbose() {
        note!("$ {run_cmd}");
    }

    let started = Instant::now();
    let result = match config.session.multiplexer.as_str() {
        "tmux" if crate::shell::which("tmux").is_none() => {
            warning!(
                "tmux not found; opening the session without it \
                 (set `multiplexer = \"none\"` to silence this)"
            );
            enter_direct(&run_cmd)
        }
        "tmux" => enter_tmux(session_name, &run_cmd),
        "none" => enter_direct(&run_cmd),
        other => bail!("unknown session.multiplexer {other:?} (supported: \"tmux\", \"none\")"),
    };

    // A remote session that fails almost instantly is the signature of the SSH
    // server refusing a PTY: `docker exec -it` then has no tty on its stdin and
    // bails the moment the shell opens. Probe for it (only on a fast failure, so
    // a normal shell exiting non-zero after real use doesn't trigger an extra
    // connection) and point at the fix, which is server-side — no client flag,
    // not even `-tt`, can override it.
    if result.is_err()
        && started.elapsed() < Duration::from_secs(2)
        && let Some(host) = target.ssh_host()
        && crate::ssh::pty_denied(host)
    {
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

// Opens (or reuses) the named tmux session for this workspace. If lodge itself is being run
// from inside tmux, we must not nest — `attach-session` refuses that — so we
// `switch-client` the current client to the lodge session instead. Outside
// tmux we attach normally.
fn enter_tmux(session_name: &str, run_cmd: &str) -> Result<()> {
    let session = sanitize(session_name);

    // Create the session detached if it doesn't already exist (idempotent): a
    // failure just means the session is already there.
    tmux_best_effort(&["new-session", "-d", "-s", &session, run_cmd]);

    // New windows in this session also exec into the container.
    Command::new("tmux")
        .args(["set-option", "-t", &session, "default-command", run_cmd])
        .status()?;

    // `run_cmd` sets the pane title via OSC once it's running (see `enter`);
    // forward that into the real terminal's title instead of leaving tmux's
    // own title machinery (which defaults to showing the session/window name)
    // in charge of it.
    // Cosmetic: without these the title is merely less pretty.
    tmux_best_effort(&["set-option", "-t", &session, "set-titles", "on"]);
    tmux_best_effort(&["set-option", "-t", &session, "set-titles-string", "#T"]);

    let already_in_tmux = std::env::var_os("TMUX").is_some();
    let action = if already_in_tmux {
        "switch-client"
    } else {
        "attach-session"
    };
    let status = Command::new("tmux")
        .args([action, "-t", &session])
        .status()?;
    if !status.success() {
        bail!("tmux {action} -t {session} failed");
    }
    Ok(())
}

// Runs a tmux command whose failure doesn't matter to the session.
#[allow(clippy::let_underscore_must_use, reason = "failure is harmless here")]
fn tmux_best_effort(args: &[&str]) {
    let _ = Command::new("tmux").args(args).status();
}

// Execs straight into the container with no multiplexer.
fn enter_direct(run_cmd: &str) -> Result<()> {
    let status = Command::new("sh").arg("-c").arg(run_cmd).status()?;
    if !status.success() {
        bail!("session exited with failure");
    }
    Ok(())
}

/// Replaces characters tmux dislikes in session names with '-'.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn keeps_alphanumeric() {
        assert_eq!(sanitize("abc123DEF"), "abc123DEF");
    }

    #[test]
    fn replaces_punctuation() {
        // container ids/names with '.'/':' would be rejected by tmux as-is.
        assert_eq!(sanitize("svc.dev_1:tag/x"), "svc-dev-1-tag-x");
    }
}
