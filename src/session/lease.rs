//! Which session holds a container.
//!
//! A container is one person's workspace, and what is placed in it (secrets) is
//! what the client that attached last declares. A session of an earlier client
//! would be left running with its secrets gone, so a new attach ends it instead.
//!
//! The session claims the container from inside: its start-up script records
//! `<pid> <id>` in a file in the user's prefix and exports `LODGE_SESSION=<id>`,
//! which every process of that session inherits. The pid is the shell's, since
//! the script only ever `exec`s. A later attach reads the record and ends that
//! process, but only if `/proc/<pid>/environ` still carries the same id, so a
//! stale record (a stopped container, a reused pid) ends nothing.
//!
//! Only sessions lodge opened can be ended; a hand-run `docker exec` or an IDE
//! terminal is not one. The record holds no secret and is the user's own file.

use anyhow::{Context, Result};

use crate::layout::Layout;
use crate::runtime::{ContainerUser, Env, ExecOpts};
use crate::shell::{script_argv, shell_quote};

/// A new session's id: lower-case hex, so it is safe in a shell and in a file.
pub fn new_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{:x}{nanos:x}", std::process::id())
}

/// The shell lines, for the start of the session script, that claim the
/// container for session `id`. Best effort: without the record the session
/// still opens, it just cannot be ended later.
pub fn claim(layout: &Layout, id: &str) -> String {
    format!(
        "LODGE_SESSION={id}; export LODGE_SESSION; \
         printf '%s %s\\n' \"$$\" {id} 2>/dev/null > {}; ",
        shell_quote(&layout.session())
    )
}

/// `$1` is the record. Ends the session it names if that is still running (it
/// carries the recorded id), says `cut` then, and removes the record either way.
const TAKEOVER_SCRIPT: &str = r#"[ -f "$1" ] || exit 0
read -r pid id < "$1" || exit 0
case "$pid$id" in *[!0-9a-f]*) rm -f "$1"; exit 0;; esac
[ -n "$pid" ] && [ -n "$id" ] || { rm -f "$1"; exit 0; }
tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null | grep -qx "LODGE_SESSION=$id" \
  || { rm -f "$1"; exit 0; }
kill -HUP "$pid" 2>/dev/null
i=0
while [ "$i" -lt 30 ] && kill -0 "$pid" 2>/dev/null; do
  sleep 0.1 2>/dev/null || sleep 1
  i=$((i + 1))
done
kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
rm -f "$1"
echo cut"#;

/// `$1` is the record, `$2` a session id. Says `ours` when the record names it.
const HOLDER_SCRIPT: &str = r#"[ -f "$1" ] || exit 0
read -r pid id < "$1" || exit 0
[ "$id" = "$2" ] && echo ours
exit 0"#;

/// Ends the session another client holds in the container, if there is one.
/// True when one was ended.
pub fn take_over(env: &dyn Env, user: &ContainerUser, layout: &Layout) -> Result<bool> {
    let out = env
        .exec_capture_with(
            &script_argv(TAKEOVER_SCRIPT, &[&layout.session()]),
            // Ending a process and removing a file are the same when run again.
            &ExecOpts::as_user(user.exec_as()).repeatable(),
        )
        .context("looking for an earlier session")?;
    Ok(out.trim() == "cut")
}

/// Whether session `id` is still the one that holds the container, that is,
/// nobody has taken over since. An error when the container can't be asked.
pub fn holds(env: &dyn Env, user: &ContainerUser, layout: &Layout, id: &str) -> Result<bool> {
    let out = env
        .exec_capture_with(
            &script_argv(HOLDER_SCRIPT, &[&layout.session(), id]),
            &ExecOpts::as_user(user.exec_as()).repeatable(),
        )
        .context("asking who holds the container")?;
    Ok(out.trim() == "ours")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;
    use crate::tmpfile::TempDir;
    use std::process::Command;

    fn user() -> ContainerUser {
        ContainerUser {
            requested: None,
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        }
    }

    fn layout() -> Layout {
        Layout::new("/home/dev/.local/share/lodge")
    }

    #[test]
    fn an_id_is_hex_and_differs_between_calls() {
        let (a, b) = (new_id(), new_id());
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn taking_over_reports_what_the_script_said() {
        let record = layout().session();
        let argv = script_argv(TAKEOVER_SCRIPT, &[&record]);
        let env = FakeEnv::new().with_output(&argv, "cut\n");
        assert!(take_over(&env, &user(), &layout()).unwrap());
        let none = FakeEnv::new().with_output(&argv, "");
        assert!(!take_over(&none, &user(), &layout()).unwrap());
    }

    #[test]
    fn holds_is_true_only_when_the_record_names_the_session() {
        let record = layout().session();
        let argv = script_argv(HOLDER_SCRIPT, &[&record, "abc"]);
        let env = FakeEnv::new().with_output(&argv, "ours\n");
        assert!(holds(&env, &user(), &layout(), "abc").unwrap());
        let other = FakeEnv::new().with_output(&argv, "");
        assert!(!holds(&other, &user(), &layout(), "abc").unwrap());
        // Not being able to ask is not "someone took over".
        let env = FakeEnv::new().with_failure(&argv);
        assert!(holds(&env, &user(), &layout(), "abc").is_err());
    }

    /// Runs `script` with `args` the way the container does.
    fn run(script: &str, args: &[&str]) -> String {
        let argv = script_argv(script, args);
        let out = Command::new(argv[0]).args(&argv[1..]).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A long-lived process with `LODGE_SESSION=id` in its environment, if given.
    #[cfg(target_os = "linux")]
    fn sleeper(id: Option<&str>) -> std::process::Child {
        let mut c = Command::new("sleep");
        c.arg("60");
        if let Some(id) = id {
            c.env("LODGE_SESSION", id);
        }
        c.spawn().unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_running_session_with_the_recorded_id_is_ended_and_the_record_removed() {
        let tmp = TempDir::new("lease").unwrap();
        let record = tmp.path().join("session");
        let record = record.to_str().unwrap();
        let mut child = sleeper(Some("abc123"));
        std::fs::write(record, format!("{} abc123\n", child.id())).unwrap();

        assert_eq!(run(TAKEOVER_SCRIPT, &[record]), "cut");
        // It was signalled (SIGHUP ends `sleep`), not left running.
        assert!(child.wait().unwrap().code().is_none());
        assert!(!std::path::Path::new(record).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_without_the_recorded_id_is_left_alone() {
        let tmp = TempDir::new("lease").unwrap();
        let record = tmp.path().join("session");
        let record = record.to_str().unwrap();
        // A reused pid: alive, but not that session.
        let mut child = sleeper(Some("someoneelse"));
        std::fs::write(record, format!("{} abc123\n", child.id())).unwrap();

        assert_eq!(run(TAKEOVER_SCRIPT, &[record]), "");
        assert!(child.try_wait().unwrap().is_none(), "it must still run");
        assert!(!std::path::Path::new(record).exists(), "stale record goes");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_dead_pid_a_missing_or_a_garbled_record_end_nothing() {
        let tmp = TempDir::new("lease").unwrap();
        let record = tmp.path().join("session");
        let record = record.to_str().unwrap();
        assert_eq!(run(TAKEOVER_SCRIPT, &[record]), "", "no record");
        let mut gone = sleeper(None);
        let pid = gone.id();
        gone.kill().unwrap();
        gone.wait().unwrap();
        std::fs::write(record, format!("{pid} abc123\n")).unwrap();
        assert_eq!(run(TAKEOVER_SCRIPT, &[record]), "", "dead pid");
        // Anything but hex digits is never handed to `kill` or a path.
        std::fs::write(record, "1; rm -rf x $(id)\n").unwrap();
        assert_eq!(run(TAKEOVER_SCRIPT, &[record]), "");
        assert!(!std::path::Path::new(record).exists());
    }

    #[test]
    fn the_claim_records_the_shells_pid_and_exports_the_id() {
        let tmp = TempDir::new("lease").unwrap();
        let l = Layout::new(tmp.path().to_str().unwrap());
        let script = format!("{}printf '%s' \"$LODGE_SESSION\"", claim(&l, "abc123"));
        let out = Command::new("sh").args(["-c", &script]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "abc123");
        let record = std::fs::read_to_string(l.session()).unwrap();
        let (pid, id) = record.trim().split_once(' ').unwrap();
        assert!(pid.parse::<u32>().is_ok() && id == "abc123", "{record}");
        // Who holds it, as the container would ask.
        assert_eq!(run(HOLDER_SCRIPT, &[&l.session(), "abc123"]), "ours");
        assert_eq!(run(HOLDER_SCRIPT, &[&l.session(), "other"]), "");
    }

    #[test]
    fn a_claim_that_cannot_be_written_does_not_stop_the_session() {
        let l = Layout::new("/nonexistent-lodge-dir");
        let script = format!("{}echo started", claim(&l, "abc123"));
        let out = Command::new("sh").args(["-c", &script]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "started");
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
