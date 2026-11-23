//! Which session holds a container.
//!
//! A container is one person's workspace, and what is placed in it (secrets) is
//! what the client that attached last declares. A session of an earlier client
//! would be left running with its secrets gone, so a new attach ends it instead.
//!
//! The session claims the container from inside: its start-up script records
//! `<pid> <id> <client>` (the client is a name for the machine that opened the
//! session, to say whose it is) in a file in the user's prefix and exports `LODGE_SESSION=<id>`,
//! which every process of that session inherits. The pid is the shell's, since
//! the script only ever `exec`s. A later attach reads the record and ends that
//! process, but only if `/proc/<pid>/environ` still carries the same id, so a
//! stale record (a stopped container, a reused pid) ends nothing.
//!
//! When the session ends, [`release`] gives the container up: it removes the record
//! and whatever else the caller names (the secrets), but only if this session still
//! holds it. Secrets are therefore in the container only while their session is.
//!
//! Only sessions lodge opened can be ended; a hand-run `docker exec` or an IDE
//! terminal is not one. The record holds no secret and is the user's own file.

use anyhow::{Context, Result};

use crate::layout::Layout;
use crate::local::Local;
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

/// This machine's name: the `hostname` program, else what the kernel or
/// `/etc/hostname` says (a minimal Linux has no such program), else the
/// variables some shells and Windows set.
fn host_name(local: &dyn Local) -> Option<String> {
    let var = |n: &str| local.var(n).map(|v| v.to_string_lossy().into_owned());
    let file = |p: &str| {
        local
            .read_file(std::path::Path::new(p))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
    };
    local
        .run_program("hostname", &[], b"", std::time::Duration::from_secs(2))
        .ok()
        .filter(|o| o.success)
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| file("/proc/sys/kernel/hostname").filter(|h| !h.is_empty()))
        .or_else(|| file("/etc/hostname").filter(|h| !h.is_empty()))
        .or_else(|| var("HOSTNAME").filter(|h| !h.is_empty()))
        .or_else(|| var("COMPUTERNAME").filter(|h| !h.is_empty()))
}

/// A name for this machine and user, as `user@host`, kept to characters that are
/// safe in a record and on a terminal. Two sessions opened from the same machine
/// by the same user have the same name, and that is what lets the second replace
/// the first without asking. When the machine's name cannot be found, nothing is
/// known to be the same machine, so the name carries this process's id: every
/// attach is then another client's and is asked about, rather than ended silently.
pub fn client(local: &dyn Local) -> String {
    let var = |n: &str| local.var(n).map(|v| v.to_string_lossy().into_owned());
    let user = var("USER")
        .or_else(|| var("USERNAME"))
        .or_else(|| var("LOGNAME"))
        .unwrap_or_else(|| "unknown".into());
    let host = host_name(local).unwrap_or_else(|| format!("unknown-{}", std::process::id()));
    let name = format!("{user}@{host}");
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.@-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The shell lines, for the start of the session script, that claim the
/// container for session `id` on behalf of `client` (see [`client`]). Best
/// effort: without the record the session still opens, it just cannot be ended
/// later.
pub fn claim(layout: &Layout, id: &str, client: &str) -> String {
    format!(
        "LODGE_SESSION={id}; export LODGE_SESSION; \
         printf '%s %s %s\\n' \"$$\" {id} {} 2>/dev/null > {}; ",
        shell_quote(client),
        shell_quote(&layout.session())
    )
}

/// `$1` is the record. Ends the session it names if that is still running (it
/// carries the recorded id), says `cut` then, and removes the record either way.
const TAKEOVER_SCRIPT: &str = r#"[ -f "$1" ] || exit 0
read -r pid id rest < "$1" || exit 0
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

/// `$1` is the record. Says the client that holds the container when the session
/// it names is still running (it carries the recorded id), and nothing when it is
/// not, removing the stale record.
const WHO_SCRIPT: &str = r#"[ -f "$1" ] || exit 0
read -r pid id client < "$1" || exit 0
case "$pid$id" in *[!0-9a-f]*) rm -f "$1"; exit 0;; esac
[ -n "$pid" ] && [ -n "$id" ] || { rm -f "$1"; exit 0; }
tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null | grep -qx "LODGE_SESSION=$id" \
  || { rm -f "$1"; exit 0; }
echo "${client:-unknown}"
exit 0"#;

/// `$1` is the record, `$2` a session id. Says `ours` when the record names it.
const HOLDER_SCRIPT: &str = r#"[ -f "$1" ] || exit 0
read -r pid id rest < "$1" || exit 0
[ "$id" = "$2" ] && echo ours
exit 0"#;

/// `$1` is the record, `$2` the session's id, the rest are paths to remove. When
/// the record names the session it is removed with them, and `released` is said.
/// One script, so another client attaching at that moment is not undone: its
/// takeover removes the record first, and then nothing here matches.
const RELEASE_SCRIPT: &str = r#"rec=$1; me=$2; shift 2
[ -f "$rec" ] || exit 0
read -r pid id rest < "$rec" || exit 0
[ "$id" = "$me" ] || exit 0
rm -f "$rec"
for p in "$@"; do rm -rf "$p"; done
echo released
exit 0"#;

/// The client whose session is running in the container, if any. A record that no
/// running session answers to is removed, so a stopped container has none.
pub fn holder(env: &dyn Env, user: &ContainerUser, layout: &Layout) -> Result<Option<String>> {
    let out = env
        .exec_capture_with(
            &script_argv(WHO_SCRIPT, &[&layout.session()]),
            &ExecOpts::as_user(user.exec_as()).repeatable(),
        )
        .context("looking for an earlier session")?;
    let who = out.trim();
    Ok((!who.is_empty()).then(|| who.to_string()))
}

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

/// Gives the container up at the end of session `id`: removes the record and
/// `also_remove`, if the session still holds it. True when it did. A session that
/// was taken over leaves everything alone, since it is now the other client's.
pub fn release(
    env: &dyn Env,
    user: &ContainerUser,
    layout: &Layout,
    id: &str,
    also_remove: &[&str],
) -> Result<bool> {
    let record = layout.session();
    let mut args = vec![record.as_str(), id];
    args.extend(also_remove);
    let out = env
        .exec_capture_with(
            &script_argv(RELEASE_SCRIPT, &args),
            &ExecOpts::as_user(user.exec_as()).repeatable(),
        )
        .context("giving the container up")?;
    Ok(out.trim() == "released")
}

/// A fake environment in which `client` holds the container (nobody, for `None`),
/// and ending its session works.
#[cfg(test)]
pub fn with_holder(
    env: crate::runtime::fake::FakeEnv,
    layout: &Layout,
    client: Option<&str>,
) -> crate::runtime::fake::FakeEnv {
    let record = layout.session();
    env.with_output(
        &script_argv(WHO_SCRIPT, &[&record]),
        client.map(|c| format!("{c}\n")).as_deref().unwrap_or(""),
    )
    .with_output(&script_argv(TAKEOVER_SCRIPT, &[&record]), "cut\n")
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

    #[test]
    fn releasing_names_the_record_the_id_and_what_else_to_remove() {
        let record = layout().session();
        let argv = script_argv(RELEASE_SCRIPT, &[&record, "abc", "/dev/shm/s"]);
        let env = FakeEnv::new().with_output(&argv, "released\n");
        assert!(release(&env, &user(), &layout(), "abc", &["/dev/shm/s"]).unwrap());
        // Taken over meanwhile: the script said nothing, nothing was removed.
        let env = FakeEnv::new().with_output(&argv, "");
        assert!(!release(&env, &user(), &layout(), "abc", &["/dev/shm/s"]).unwrap());
        let env = FakeEnv::new().with_failure(&argv);
        assert!(release(&env, &user(), &layout(), "abc", &["/dev/shm/s"]).is_err());
    }

    #[test]
    fn the_release_script_removes_only_for_the_session_that_holds_the_container() {
        let tmp = TempDir::new("lease").unwrap();
        let record = tmp.path().join("session");
        let secrets = tmp.path().join("secrets");
        let (record, secrets) = (record.to_str().unwrap(), secrets.to_str().unwrap());
        let setup = || {
            std::fs::create_dir_all(secrets).unwrap();
            std::fs::write(format!("{secrets}/TOKEN"), "v").unwrap();
            std::fs::write(record, "1234 abc123\n").unwrap();
        };
        let exists = |p: &str| std::path::Path::new(p).exists();

        // Another client's session holds it now: nothing is touched.
        setup();
        assert_eq!(run(RELEASE_SCRIPT, &[record, "old777", secrets]), "");
        assert!(exists(record) && exists(&format!("{secrets}/TOKEN")));
        // No record at all (taken over and released, or never claimed): same.
        std::fs::remove_file(record).unwrap();
        assert_eq!(run(RELEASE_SCRIPT, &[record, "abc123", secrets]), "");
        assert!(exists(&format!("{secrets}/TOKEN")));
        // This session holds it: the record and the secrets go.
        setup();
        assert_eq!(
            run(RELEASE_SCRIPT, &[record, "abc123", secrets]),
            "released"
        );
        assert!(!exists(record) && !exists(secrets));
    }

    #[test]
    fn a_client_is_named_after_the_user_and_machine_in_safe_characters() {
        use crate::local::fake::FakeLocal;
        let local =
            FakeLocal::new()
                .with_var("USER", "kai")
                .with_program("hostname", true, b"lap top\n");
        assert_eq!(client(&local), "kai@lap_top");
        // No `hostname` program (a minimal Linux): the kernel's name, then the file,
        // then the variables.
        let local = FakeLocal::new()
            .with_var("USER", "kai")
            .with_file("/proc/sys/kernel/hostname", b"arch-box\n");
        assert_eq!(client(&local), "kai@arch-box");
        let local = FakeLocal::new()
            .with_var("USER", "kai")
            .with_file("/etc/hostname", b"etc-box\n");
        assert_eq!(client(&local), "kai@etc-box");
        let local = FakeLocal::new()
            .with_var("USERNAME", "kai")
            .with_var("COMPUTERNAME", "WIN-1");
        assert_eq!(client(&local), "kai@WIN-1");
        // No name for the machine at all: never equal to another attach's, so it
        // is asked about rather than replaced silently.
        let anon = client(&FakeLocal::new().with_var("USER", "kai"));
        assert!(anon.starts_with("kai@unknown-"), "{anon}");
        // Nothing that could end the quoting in a script.
        let local = FakeLocal::new().with_var("USER", "a'; rm -rf /; '");
        assert!(
            client(&local)
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.@-".contains(c))
        );
    }

    #[test]
    fn the_holder_is_the_client_the_script_names() {
        let record = layout().session();
        let argv = script_argv(WHO_SCRIPT, &[&record]);
        let env = FakeEnv::new().with_output(&argv, "kai@laptop\n");
        assert_eq!(
            holder(&env, &user(), &layout()).unwrap().as_deref(),
            Some("kai@laptop")
        );
        let none = FakeEnv::new().with_output(&argv, "");
        assert_eq!(holder(&none, &user(), &layout()).unwrap(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_who_script_names_a_running_session_and_drops_a_stale_record() {
        let tmp = TempDir::new("lease").unwrap();
        let record = tmp.path().join("session");
        let record = record.to_str().unwrap();
        let mut live = sleeper(Some("abc123"));
        std::fs::write(record, format!("{} abc123 kai@laptop\n", live.id())).unwrap();
        assert_eq!(run(WHO_SCRIPT, &[record]), "kai@laptop");
        assert!(std::path::Path::new(record).exists());
        // The record of an older format has no client: still a holder.
        std::fs::write(record, format!("{} abc123\n", live.id())).unwrap();
        assert_eq!(run(WHO_SCRIPT, &[record]), "unknown");
        // Not that session (a reused pid): nobody, and the record goes.
        std::fs::write(record, format!("{} other1 kai@laptop\n", live.id())).unwrap();
        assert_eq!(run(WHO_SCRIPT, &[record]), "");
        assert!(!std::path::Path::new(record).exists());
        live.kill().unwrap();
        live.wait().unwrap();
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
        let script = format!(
            "{}printf '%s' \"$LODGE_SESSION\"",
            claim(&l, "abc123", "kai@laptop")
        );
        let out = Command::new("sh").args(["-c", &script]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "abc123");
        let record = std::fs::read_to_string(l.session()).unwrap();
        let mut fields = record.trim().split(' ');
        let (pid, id, who) = (fields.next(), fields.next(), fields.next());
        assert!(pid.unwrap().parse::<u32>().is_ok(), "{record}");
        assert_eq!((id, who), (Some("abc123"), Some("kai@laptop")), "{record}");
        // Who holds it, as the container would ask.
        assert_eq!(run(HOLDER_SCRIPT, &[&l.session(), "abc123"]), "ours");
        assert_eq!(run(HOLDER_SCRIPT, &[&l.session(), "other"]), "");
    }

    #[test]
    fn a_claim_that_cannot_be_written_does_not_stop_the_session() {
        let l = Layout::new("/nonexistent-lodge-dir");
        let script = format!("{}echo started", claim(&l, "abc123", "kai@laptop"));
        let out = Command::new("sh").args(["-c", &script]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "started");
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
