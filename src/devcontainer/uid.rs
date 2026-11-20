//! Line the container user's uid/gid up with the host user's.
//!
//! A bind-mounted workspace keeps its host ownership. If the container user has
//! a different uid than the host user (an image typically bakes in 1000, a host
//! may have 1001, or 1500), the user cannot write its own project. Like VS Code
//! on Linux, lodge changes the container user's uid, and its group's gid when
//! that group is private to the user, and fixes up the user's home directory.
//!
//! The decision is made in Rust from `/etc/passwd` and `/etc/group` ([`plan`]),
//! so it can be tested; the container only runs a small script to apply it.

use crate::output::{info, warning};
use anyhow::{Context, Result};

use crate::runtime::{ContainerUser, Env};
use crate::shell::shell_quote;

/// What to do about one user's ids.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Already matches the host.
    Nothing,
    /// Leave it alone, and say why.
    Skip(String),
    Change(Change),
}

/// The ids to move a user from and to.
#[derive(Debug, PartialEq, Eq)]
struct Change {
    old_uid: u32,
    old_gid: u32,
    new_uid: u32,
    new_gid: u32,
    home: String,
}

struct PasswdEntry<'a> {
    name: &'a str,
    uid: u32,
    gid: u32,
    home: &'a str,
}

fn passwd_entries(passwd: &str) -> Vec<PasswdEntry<'_>> {
    passwd
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            Some(PasswdEntry {
                name: f.first()?,
                uid: f.get(2)?.parse().ok()?,
                gid: f.get(3)?.parse().ok()?,
                home: f.get(5)?,
            })
        })
        .collect()
}

/// `(group name, gid)` for every line of `/etc/group`.
fn group_entries(group: &str) -> Vec<(&str, u32)> {
    group
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            Some((*f.first()?, f.get(2)?.parse().ok()?))
        })
        .collect()
}

fn plan(passwd: &str, group: &str, user: &str, host_uid: u32, host_gid: u32) -> Plan {
    let users = passwd_entries(passwd);
    let Some(me) = users.iter().find(|u| u.name == user) else {
        return Plan::Skip(format!("user '{user}' not found in /etc/passwd"));
    };
    if me.uid == host_uid && me.gid == host_gid {
        return Plan::Nothing;
    }
    if me.uid != host_uid
        && let Some(other) = users.iter().find(|u| u.uid == host_uid && u.name != user)
    {
        return Plan::Skip(format!(
            "uid {host_uid} is already used by '{}'; leaving '{user}' as {}",
            other.name, me.uid
        ));
    }

    // The gid only moves when the user's primary group is theirs alone and the
    // host's gid is free: changing a shared group would change it for everyone.
    let private_group = users.iter().filter(|u| u.gid == me.gid).count() == 1;
    let gid_free = !group_entries(group).iter().any(|&(_, gid)| gid == host_gid);
    let new_gid = if me.gid != host_gid && private_group && gid_free {
        host_gid
    } else {
        me.gid
    };

    Plan::Change(Change {
        old_uid: me.uid,
        old_gid: me.gid,
        new_uid: host_uid,
        new_gid,
        home: me.home.to_string(),
    })
}

/// Escapes `s` for use as a literal in a basic regular expression.
fn sed_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if matches!(c, '.' | '*' | '[' | ']' | '\\' | '^' | '$' | '/') {
                vec!['\\', c]
            } else {
                vec![c]
            }
        })
        .collect()
}

/// The script, run as root in the container, that applies a [`Change`].
fn script(user: &str, change: &Change) -> String {
    let Change {
        old_uid,
        old_gid,
        new_uid,
        new_gid,
        home,
    } = change;
    let u = sed_escape(user);
    let mut s = String::from("set -e\n");
    s.push_str(&format!(
        "sed -i 's/^{u}:\\([^:]*\\):{old_uid}:{old_gid}:/{u}:\\1:{new_uid}:{new_gid}:/' /etc/passwd\n"
    ));
    if old_gid != new_gid {
        s.push_str(&format!(
            "sed -i 's/^\\([^:]*\\):\\([^:]*\\):{old_gid}:/\\1:\\2:{new_gid}:/' /etc/group\n"
        ));
    }
    // Only files on the home directory's own filesystem: a bind mount below it
    // belongs to the host and must not be re-owned.
    let h = shell_quote(home);
    s.push_str(&format!("if [ -d {h} ]; then\n"));
    s.push_str(&format!(
        "  find {h} -xdev -uid {old_uid} -exec chown -h {new_uid} {{}} +\n"
    ));
    if old_gid != new_gid {
        s.push_str(&format!(
            "  find {h} -xdev -gid {old_gid} -exec chgrp -h {new_gid} {{}} +\n"
        ));
    }
    s.push_str("fi\n");
    s
}

/// Aligns `remote_user`'s uid/gid (or the container's default user's) with the
/// host's. Does nothing for root.
pub(super) fn sync_user_uid(
    env: &dyn Env,
    remote_user: Option<&str>,
    host_uid: u32,
    host_gid: u32,
) -> Result<()> {
    if remote_user == Some("root") || host_uid == 0 {
        return Ok(());
    }
    let resolved = ContainerUser::resolve(env, remote_user)?;
    if resolved.is_root() {
        return Ok(());
    }
    let user = resolved.name;
    if !env.has_command("sed") || !env.has_command("find") {
        warning!("cannot align uid with the host: sed/find missing in the container");
        return Ok(());
    }

    // A failed read is an error: an empty /etc/group would make every gid look
    // free. A missing /etc/group really has no groups in it.
    let passwd = env
        .read_file("/etc/passwd")?
        .context("the container has no /etc/passwd")?;
    let group = env.read_file("/etc/group")?.unwrap_or_default();
    match plan(&passwd, &group, &user, host_uid, host_gid) {
        Plan::Nothing => Ok(()),
        Plan::Skip(why) => {
            warning!("not changing the uid of '{user}': {why}");
            Ok(())
        }
        Plan::Change(change) => {
            info!(
                "aligning '{user}' with the host user: uid {} → {}",
                change.old_uid, change.new_uid
            );
            env.exec_root(&["sh", "-c", &script(&user, &change)])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
                          dev:x:1500:1500::/home/dev:/bin/bash\n";
    const GROUP: &str = "root:x:0:\ndev:x:1500:\n";

    #[test]
    fn nothing_to_do_when_ids_already_match() {
        let passwd = "dev:x:1000:1000::/home/dev:/bin/bash\n";
        assert_eq!(
            plan(passwd, "dev:x:1000:\n", "dev", 1000, 1000),
            Plan::Nothing
        );
    }

    #[test]
    fn uid_and_a_private_group_follow_the_host() {
        assert_eq!(
            plan(PASSWD, GROUP, "dev", 1000, 1000),
            Plan::Change(Change {
                old_uid: 1500,
                old_gid: 1500,
                new_uid: 1000,
                new_gid: 1000,
                home: "/home/dev".into()
            })
        );
    }

    #[test]
    fn a_uid_used_by_someone_else_is_left_alone() {
        let passwd = "node:x:1000:1000::/home/node:/bin/bash\n\
                      dev:x:1500:1500::/home/dev:/bin/bash\n";
        let p = plan(passwd, GROUP, "dev", 1000, 1000);
        assert!(
            matches!(p, Plan::Skip(ref why) if why.contains("'node'")),
            "{p:?}"
        );
    }

    #[test]
    fn a_shared_primary_group_keeps_its_gid() {
        let passwd = "dev:x:1500:100::/home/dev:/bin/bash\n\
                      other:x:1600:100::/home/other:/bin/bash\n";
        match plan(passwd, "users:x:100:\n", "dev", 1000, 1000) {
            Plan::Change(Change {
                new_uid, new_gid, ..
            }) => {
                assert_eq!(new_uid, 1000);
                assert_eq!(new_gid, 100, "a group other users share must not change");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_host_gid_that_is_taken_keeps_the_users_gid() {
        let group = "docker:x:1000:\ndev:x:1500:\n";
        match plan(PASSWD, group, "dev", 1000, 1000) {
            Plan::Change(Change { new_gid, .. }) => assert_eq!(new_gid, 1500),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unknown_user_is_skipped() {
        assert!(matches!(
            plan(PASSWD, GROUP, "ghost", 1000, 1000),
            Plan::Skip(_)
        ));
    }

    #[test]
    fn the_script_edits_the_files_and_stays_on_the_homes_filesystem() {
        let Plan::Change(change) = plan(PASSWD, GROUP, "dev", 1000, 1000) else {
            panic!("expected a change");
        };
        let s = script("dev", &change);
        assert!(
            s.contains("s/^dev:\\([^:]*\\):1500:1500:/dev:\\1:1000:1000:/"),
            "{s}"
        );
        assert!(s.contains("/etc/group"), "{s}");
        assert!(
            s.contains("find '/home/dev' -xdev -uid 1500 -exec chown -h 1000"),
            "{s}"
        );
    }

    #[test]
    fn user_names_are_escaped_for_sed() {
        assert_eq!(sed_escape("a.b"), "a\\.b");
        assert_eq!(sed_escape("plain-name_1"), "plain-name_1");
    }

    /// The commands that applied a change: `sh -c <script>` editing /etc/passwd.
    fn change_scripts(env: &FakeEnv) -> Vec<crate::runtime::fake::Who> {
        env.calls()
            .into_iter()
            .filter(|(argv, _)| argv.len() == 3 && argv[0] == "sh" && argv[2].contains("sed -i"))
            .map(|(_, who)| who)
            .collect()
    }

    fn probe(answer: &str) -> FakeEnv {
        FakeEnv::new()
            .with_output(&["sh", "-c", crate::runtime::PROBE_SCRIPT], answer)
            .with_commands(&["sed", "find"])
            .with_file("/etc/passwd", PASSWD)
            .with_file("/etc/group", GROUP)
    }

    #[test]
    fn root_is_never_changed() {
        // Named root: nothing is even asked.
        let env = FakeEnv::new();
        sync_user_uid(&env, Some("root"), 1000, 1000).unwrap();
        assert!(env.calls().is_empty());
        // A default user that turns out to be root: asked, then left alone.
        let env = probe("0\n0\nroot\n/root\n");
        sync_user_uid(&env, None, 1000, 1000).unwrap();
        assert!(change_scripts(&env).is_empty());
    }

    #[test]
    fn a_change_runs_one_script_as_root() {
        let env = probe("1500\n1500\ndev\n/home/dev\n");
        sync_user_uid(&env, Some("dev"), 1000, 1000).unwrap();
        assert_eq!(change_scripts(&env), [crate::runtime::fake::Who::Root]);
    }

    #[test]
    fn an_unreadable_group_file_stops_the_change() {
        let env = probe("1500\n1500\ndev\n/home/dev\n").with_failure(&crate::shell::script_argv(
            crate::runtime::READ_FILE_SCRIPT,
            &["/etc/group"],
        ));
        assert!(sync_user_uid(&env, Some("dev"), 1000, 1000).is_err());
        assert!(change_scripts(&env).is_empty());
    }
}
