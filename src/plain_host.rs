//! A machine you can reach, as an endpoint: no container, no engine. lodge runs
//! commands on it and copies files into it through the [`Host`](crate::host::Host) it is given, as
//! the user the host connects as, and injects into that user's prefix the same
//! way it does into a container.
//!
//! What differs from a container is what the machine *is*: someone's real
//! computer, not a throwaway. So it can't be asked to switch user, it is
//! [not disposable](crate::runtime::Env::is_disposable) (injection leaves their dotfiles alone and
//! never loosens git's ownership check), and there is nothing to create, stop or
//! remove.

use anyhow::Result;
use std::path::Path;
use std::process::ExitStatus;

use crate::fingerprint::content_hash;
use crate::host::{HostRef, Job};
use crate::port::PortForward;
use crate::provision::{Attached, Endpoints, Found, Lifecycle, Status};
use crate::runtime::{
    Container, ExecOpts, Inside, LoginOnly, Reach, Runtime, User, in_dir_with_env,
};
use crate::shell::shell_quote;

/// A reachable machine, seen as an endpoint.
pub struct PlainHost {
    host: HostRef,
    /// What identifies this machine in lodge's own state (the forwarding
    /// daemon's files): a short, file-name-safe tag of how the host is reached.
    id: String,
    /// Only the user the host connects as can be acted as.
    acting: LoginOnly,
}

impl PlainHost {
    pub fn new(host: HostRef) -> Self {
        let label = host.label().unwrap_or_else(|| "local".into());
        Self {
            id: format!("host-{}", &content_hash(label.as_bytes())[..7]),
            host,
            acting: LoginOnly::new("a plain host"),
        }
    }
}

/// What is a plain host's own about working inside it: commands as they are (or in
/// a `cd … && env …` script, since there is no flag for either), `tar -x` to
/// unpack, and `sh -c` in a directory for a terminal. The rest of
/// [`Env`](crate::runtime::Env) is built on this in `runtime::inside`.
impl Inside for PlainHost {
    fn host(&self) -> &HostRef {
        &self.host
    }

    fn describe(&self) -> String {
        "a command on the host".into()
    }

    fn command<'a>(&self, argv: &[&str], opts: &ExecOpts, _stdin: bool) -> Result<Job<'a>> {
        self.acting.check(self, opts.user)?;
        Ok(match in_dir_with_env(argv, opts) {
            Some(script) => Job::shell(&script),
            None => Job::argv(argv),
        })
    }

    /// The host's own `tar`, which owns what it unpacks as the user it runs as (for
    /// root, the owner the archive names, which is that same user).
    fn extract<'a>(&self, dir: &str) -> Job<'a> {
        Job::argv(&["tar", "-xpf", "-", "-C", dir])
    }

    fn session(&self, user: User, workdir: &str, inner: &str) -> std::io::Result<ExitStatus> {
        self.acting
            .check(self, user)
            .map_err(std::io::Error::other)?;
        let script = format!("cd {} && {inner}", shell_quote(workdir));
        self.host.interactive(&["sh", "-c", &script])
    }

    fn disposable(&self) -> bool {
        false
    }
}

impl Container for PlainHost {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> Option<String> {
        Some(self.host.label().unwrap_or_else(|| "this machine".into()))
    }

    fn mounts(&self) -> Vec<String> {
        Vec::new()
    }

    fn is_running(&self) -> Result<bool> {
        Ok(true)
    }

    fn read_stored_file(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .host
            .read_file(Path::new(path))?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn configured_user(&self) -> Option<String> {
        None
    }

    /// Whatever listens on a remote machine is reachable from that machine's own
    /// loopback, which is where the tunnel ends. On this machine there is nothing
    /// to forward to.
    fn reach(&self, peer: Option<&str>) -> Option<Reach> {
        match peer {
            None if self.host.label().is_some() => Some(Reach::Address("127.0.0.1".into())),
            _ => None,
        }
    }
}

/// Makes a plain host ready for a project: there is nothing to make, so this
/// hands back the machine itself, to work in `project` (a folder on it).
pub struct PlainHostEndpoints {
    host: HostRef,
}

impl PlainHostEndpoints {
    pub fn new(host: HostRef) -> Self {
        Self { host }
    }

    fn found(&self) -> Found {
        Found {
            container: Box::new(PlainHost::new(self.host.clone())),
            remote_user: None,
        }
    }
}

impl Endpoints for PlainHostEndpoints {
    fn provision(&self, project: &Path, _rebuild: bool) -> Result<Attached> {
        Ok(Attached {
            container: Box::new(PlainHost::new(self.host.clone())),
            // Never made by lodge, so whether anything is injected is for the
            // record in its prefix to say.
            created: false,
            workdir: project.to_string_lossy().into_owned(),
            forward_ports: Vec::<PortForward>::new(),
            remote_user: None,
            post_attach: None,
        })
    }

    fn find(&self, _project: &Path) -> Result<Option<Found>> {
        Ok(Some(self.found()))
    }

    fn status(&self, _project: &Path) -> Result<Option<Status>> {
        Ok(Some(Status {
            found: self.found(),
            running: true,
            config: None,
        }))
    }

    fn lifecycle(&self) -> Option<&dyn Lifecycle> {
        // Nothing to stop, remove or list: the machine was there before lodge.
        None
    }

    fn container(&self, _id: &str) -> Box<dyn Container> {
        Box::new(PlainHost::new(self.host.clone()))
    }

    fn engine(&self) -> Option<&dyn Runtime> {
        None
    }

    fn needs_project_path(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::{FakeHost, Reply};
    use crate::runtime::{ContainerUser, Env as _};
    use std::sync::Arc;

    const PROBE: [&str; 3] = ["sh", "-c", crate::runtime::PROBE_SCRIPT];

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(|w| w.to_string()).collect()
    }

    /// A machine whose login user is `dev` (uid 1000).
    fn machine() -> (Arc<FakeHost>, PlainHost) {
        let host = Arc::new(FakeHost::remote("me@srv").on(&PROBE, "1000\n1000\ndev\n/home/dev\n"));
        let plain = PlainHost::new(host.clone());
        (host, plain)
    }

    #[test]
    fn a_command_is_run_on_the_host_as_it_is_or_in_a_directory_with_an_environment() {
        let (host, plain) = machine();
        plain.exec(&["true"], &ExecOpts::default()).unwrap();
        let env = vec![("A".to_string(), "x y".to_string())];
        let opts = ExecOpts {
            workdir: Some("/w d"),
            env: &env,
            ..ExecOpts::default()
        };
        plain.exec(&["echo", "hi"], &opts).unwrap();
        let argvs = host.argvs();
        assert_eq!(argvs[0], words(&["true"]));
        assert_eq!(
            argvs[1],
            words(&["sh", "-c", "cd '/w d' && exec env 'A=x y' 'echo' 'hi'"])
        );
    }

    #[test]
    fn output_reports_failure_as_a_value_and_exec_as_an_error() {
        let host = Arc::new(FakeHost::new().reply(&["false"], Reply::fail(1, "no")));
        let plain = PlainHost::new(host);
        let out = plain.output(&["false"], &ExecOpts::default()).unwrap();
        assert!(!out.success);
        assert_eq!(out.stderr, "no");
        assert!(plain.exec(&["false"], &ExecOpts::default()).is_err());
    }

    #[test]
    fn only_the_user_the_host_connects_as_can_be_acted_as() {
        let (host, plain) = machine();
        let as_user = |user| ExecOpts::as_user(user);
        plain.exec(&["true"], &as_user(User::Default)).unwrap();
        plain.exec(&["true"], &as_user(User::Named("dev"))).unwrap();
        let other = plain
            .exec(&["true"], &as_user(User::Named("alice")))
            .unwrap_err();
        assert!(other.to_string().contains("dev"), "{other}");
        let root = plain.exec(&["true"], &as_user(User::Root)).unwrap_err();
        assert!(root.to_string().contains("root"), "{root}");
        // The two allowed ones ran; the two refused ones never reached the host.
        let ran = host
            .argvs()
            .iter()
            .filter(|a| **a == words(&["true"]))
            .count();
        assert_eq!(ran, 2);
    }

    #[test]
    fn root_can_be_acted_as_when_that_is_who_connects() {
        let host = Arc::new(FakeHost::new().on(&PROBE, "0\n0\nroot\n/root\n"));
        let plain = PlainHost::new(host);
        plain.exec(&["true"], &ExecOpts::root()).unwrap();
    }

    #[test]
    fn files_go_in_as_a_tar_stream_unpacked_by_the_host_into_a_directory_made_first() {
        let (host, plain) = machine();
        let dir = crate::tmpfile::TempDir::new("plain-put").unwrap();
        std::fs::write(dir.path().join("rc"), "set x").unwrap();
        let user = ContainerUser::resolve(&plain, None).unwrap();
        plain
            .put(
                &dir.path().join("rc").to_string_lossy(),
                "/home/dev/.config/hello/rc",
                &user,
            )
            .unwrap();
        let calls = host.calls();
        let mkdir = calls
            .iter()
            .position(|c| c.argv == words(&["mkdir", "-p", "/home/dev/.config/hello"]))
            .expect("the directory is made first");
        let tar = calls
            .iter()
            .position(|c| c.argv == words(&["tar", "-xpf", "-", "-C", "/home/dev/.config/hello"]))
            .expect("the host unpacks it");
        assert!(mkdir < tar);
        let mut archive = tar::Archive::new(calls[tar].fed.as_slice());
        let mut entries = archive.entries().unwrap();
        let entry = entries.next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().to_string_lossy(), "rc");
        assert_eq!(entry.header().uid().unwrap(), 1000);
    }

    #[test]
    fn a_secret_travels_on_stdin_and_never_in_the_arguments() {
        let (host, plain) = machine();
        plain
            .write_secret("/dev/shm/s", b"hunter2", 0o600, User::Default)
            .unwrap();
        let call = host.calls().pop().unwrap();
        assert_eq!(call.fed, b"hunter2");
        assert!(call.argv.iter().all(|a| !a.contains("hunter2")), "{call:?}");
    }

    #[test]
    fn a_session_is_a_shell_in_the_directory_on_the_host() {
        let (host, plain) = machine();
        plain
            .interactive(User::Default, "/w", "exec bash -l")
            .unwrap();
        assert_eq!(
            host.argvs().last().unwrap(),
            &words(&["interactive", "sh", "-c", "cd '/w' && exec bash -l"])
        );
    }

    #[test]
    fn the_machine_is_a_container_that_is_always_up_with_no_mounts_and_is_not_disposable() {
        let (_, plain) = machine();
        assert!(!plain.is_disposable());
        assert!(plain.is_running().unwrap());
        assert!(plain.mounts().is_empty());
        assert_eq!(plain.configured_user(), None);
        assert_eq!(plain.display_name(), "me@srv");
        assert_eq!(
            PlainHost::new(Arc::new(FakeHost::new())).display_name(),
            "this machine"
        );
    }

    #[test]
    fn it_is_told_apart_by_how_it_is_reached_in_a_name_that_is_safe_for_a_file() {
        let id = |label: &str| {
            PlainHost::new(Arc::new(FakeHost::remote(label)))
                .id()
                .to_string()
        };
        assert_eq!(id("me@a"), id("me@a"));
        assert_ne!(id("me@a"), id("me@b"));
        assert!(
            id("me@a:22")
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        );
        assert_eq!(
            id("me@a").len(),
            12,
            "the forward state keeps 12 characters"
        );
    }

    #[test]
    fn ports_of_another_machine_are_reached_on_its_own_loopback_and_this_ones_are_not_forwarded() {
        let (_, remote) = machine();
        assert_eq!(remote.reach(None), Some(Reach::Address("127.0.0.1".into())));
        assert_eq!(remote.reach(Some("db")), None);
        let local = PlainHost::new(Arc::new(FakeHost::new()));
        assert_eq!(local.reach(None), None);
    }

    #[test]
    fn stored_files_are_read_through_the_host() {
        let host = Arc::new(FakeHost::remote("h").with_file("/p/injected", "bundle abc\n"));
        let plain = PlainHost::new(host);
        assert_eq!(
            plain.read_stored_file("/p/injected").unwrap().as_deref(),
            Some("bundle abc\n")
        );
        assert_eq!(plain.read_stored_file("/p/none").unwrap(), None);
    }

    #[test]
    fn provisioning_hands_back_the_machine_to_work_in_the_project_folder() {
        let endpoints = PlainHostEndpoints::new(Arc::new(FakeHost::remote("h")));
        let attached = endpoints.provision(Path::new("/srv/app"), true).unwrap();
        assert_eq!(attached.workdir, "/srv/app");
        assert!(
            !attached.created,
            "lodge made nothing, even when asked to rebuild"
        );
        assert!(attached.remote_user.is_none() && attached.post_attach.is_none());
        let found = endpoints.find(Path::new("/srv/app")).unwrap().unwrap();
        assert_eq!(found.container.id(), attached.container.id());
        let status = endpoints.status(Path::new("/srv/app")).unwrap().unwrap();
        assert!(status.running);
        assert_eq!(status.config, None);
        assert_eq!(
            endpoints.container("whatever").id(),
            attached.container.id()
        );
    }

    #[test]
    fn a_machine_has_no_lifecycle_and_no_engine_and_needs_no_project_path() {
        let endpoints = PlainHostEndpoints::new(Arc::new(FakeHost::new()));
        assert!(endpoints.lifecycle().is_none());
        assert!(endpoints.engine().is_none());
        assert!(!endpoints.needs_project_path());
    }
}
