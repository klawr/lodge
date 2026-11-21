//! Working inside an endpoint, built once.
//!
//! Every endpoint lodge acts in (a container, a plain host, later a pod) is
//! reached the same way: run a command there as someone, stream a tar into it,
//! send bytes on stdin, open a terminal. What differs is only *how that is spelled*:
//! `docker exec -u dev -w /w`, a `cd … && env …` script, `kubectl exec`. [`Inside`]
//! is that spelling and nothing else; the [`Env`] every endpoint needs, with the
//! user checks, the retry marking, the secrets that stay out of argv and the tar
//! that is packed in-process, is built on it here, so a new endpoint writes five
//! small methods and gets the rest.
//!
//! `Inside` is the convenience for endpoints reached by running commands on a
//! [`Host`](crate::host::HostRef); it is not the seam. The seam is [`Env`], and an
//! endpoint that is reached some other way (an API client, say) implements `Env`
//! itself, as the in-memory `FakeEnv` does.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::ExitStatus;

use super::{ContainerUser, Env, ExecOpts, Output, User, tarball};
use crate::host::{HostRef, Input, Job, Repeat};
use crate::shell::shell_quote;

/// What is particular to reaching inside one endpoint.
pub trait Inside {
    /// The host the commands are run on (where the engine or the machine is).
    fn host(&self) -> &HostRef;

    /// What running a command inside is called, for messages (`docker exec`).
    fn describe(&self) -> String;

    /// The job that runs `argv` inside as `opts` say (user, directory,
    /// environment), with stdin to be fed when `stdin`. An error when the endpoint
    /// cannot act as that user. The job's streams are the caller's to set.
    fn command<'a>(&self, argv: &[&str], opts: &ExecOpts, stdin: bool) -> Result<Job<'a>>;

    /// The job that unpacks a tar read from stdin into `dir`, which exists, keeping
    /// the archive's owners where the endpoint can.
    fn extract<'a>(&self, dir: &str) -> Job<'a>;

    /// Opens a terminal inside, running `inner` (a `sh -c` script) as `user` from
    /// `workdir`, and waits until it ends.
    fn session(&self, user: User, workdir: &str, inner: &str) -> std::io::Result<ExitStatus>;

    /// Whether the endpoint is thrown away, so replacing what is there is fine.
    fn disposable(&self) -> bool {
        true
    }
}

/// The `sh -c` script that runs `argv` from `opts`'s directory with its extra
/// environment, for an endpoint whose command line has no flag for either; `None`
/// when `opts` asks for neither, so the command can be run as it is.
pub fn in_dir_with_env(argv: &[&str], opts: &ExecOpts) -> Option<String> {
    if opts.workdir.is_none() && opts.env.is_empty() {
        return None;
    }
    let mut script = String::new();
    if let Some(dir) = opts.workdir {
        script.push_str(&format!("cd {} && ", shell_quote(dir)));
    }
    script.push_str("exec ");
    if !opts.env.is_empty() {
        script.push_str("env");
        for (k, v) in opts.env {
            script.push_str(&format!(" {}", shell_quote(&format!("{k}={v}"))));
        }
        script.push(' ');
    }
    script.push_str(
        &argv
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" "),
    );
    Some(script)
}

impl<T: Inside> Env for T {
    fn is_disposable(&self) -> bool {
        self.disposable()
    }

    fn exec(&self, argv: &[&str], opts: &ExecOpts) -> Result<()> {
        let job = self.command(argv, opts, false)?.repeat(opts.repeat);
        let ran = self
            .host()
            .exec(job)
            .with_context(|| format!("spawning {}", self.describe()))?;
        if !ran.status.success() {
            bail!("{} failed: {argv:?}", self.describe());
        }
        Ok(())
    }

    fn output(&self, argv: &[&str], opts: &ExecOpts) -> Result<Output> {
        let job = self
            .command(argv, opts, false)?
            .capture()
            .repeat(opts.repeat);
        let out = self
            .host()
            .exec(job)
            .with_context(|| format!("spawning {}", self.describe()))?;
        Ok(Output {
            success: out.status.success(),
            stdout: out.stdout_text(),
            stderr: out.stderr_text(),
        })
    }

    /// The directory it unpacks into is made first, *as the owner*, so what is
    /// made there is theirs and root is never needed; then a tar built here is
    /// streamed to the endpoint's own unpacker.
    fn put(&self, src: &str, dest: &str, owner: &ContainerUser) -> Result<()> {
        let source = Path::new(src);
        let meta = std::fs::metadata(source).with_context(|| format!("stat {src}"))?;
        // A directory is unpacked *into* `dest`; a file into its parent, under
        // its own name.
        let (into, name) = if meta.is_dir() {
            (dest.to_string(), String::new())
        } else {
            let dest_path = Path::new(dest);
            let parent = dest_path
                .parent()
                .and_then(Path::to_str)
                .filter(|p| !p.is_empty())
                .with_context(|| format!("{dest} has no parent directory"))?;
            let file = dest_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .with_context(|| format!("{dest} names no file"))?;
            (parent.to_string(), file)
        };
        self.exec(
            &["mkdir", "-p", &into],
            &ExecOpts::as_user(owner.exec_as()).repeatable(),
        )
        .with_context(|| format!("making {into} as {}", owner.name))?;

        // Unpacking the same archive again leaves the same files, so a lost
        // connection reruns the whole send, packing the tar afresh.
        let mut packed = Ok(());
        let mut feed = |stdin: &mut dyn std::io::Write| {
            // Dropping the writer closes the pipe, so the unpacker sees the end of
            // its input.
            packed = tarball::pack(source, &name, owner, stdin);
            Ok(())
        };
        let ran = self
            .host()
            .exec(
                self.extract(&into)
                    .stdin(Input::Feed(&mut feed))
                    .repeat(Repeat::Safe),
            )
            .with_context(|| format!("unpacking into {into}"))?;
        if !ran.status.success() {
            bail!("unpacking {src} into {dest} failed ({})", ran.status);
        }
        packed.with_context(|| format!("packing {src}"))
    }

    fn write_secret(&self, dest: &str, contents: &[u8], mode: u32, user: User) -> Result<()> {
        let script = super::WRITE_SECRET_SCRIPT.replace("%MODE%", &format!("{mode:o}"));
        // The value goes on stdin; argv carries only the script and the path.
        let argv = crate::shell::script_argv(&script, &[dest]);
        let job = self.command(&argv, &ExecOpts::as_user(user), true)?;
        let mut feed = |stdin: &mut dyn std::io::Write| stdin.write_all(contents);
        // The script writes to a temporary file and renames it into place, so
        // sending the secret again after a lost connection is harmless.
        let ran = self
            .host()
            .exec(job.stdin(Input::Feed(&mut feed)).repeat(Repeat::Safe))
            .with_context(|| format!("running {}", self.describe()))?;
        if !ran.status.success() {
            bail!("writing secret file {dest} failed ({})", ran.status);
        }
        ran.fed
            .with_context(|| format!("sending a secret to {}", self.describe()))
    }

    fn interactive(&self, user: User, workdir: &str, inner: &str) -> std::io::Result<ExitStatus> {
        self.session(user, workdir, inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::{FakeHost, Reply};
    use std::sync::Arc;

    /// An endpoint that is only the five things [`Inside`] asks for: everything an
    /// [`Env`] does is built on them. Commands are `toy run <argv…>`.
    struct Toy {
        host: HostRef,
    }

    impl Inside for Toy {
        fn host(&self) -> &HostRef {
            &self.host
        }

        fn describe(&self) -> String {
            "toy run".into()
        }

        fn command<'a>(&self, argv: &[&str], opts: &ExecOpts, _stdin: bool) -> Result<Job<'a>> {
            if let User::Named("nobody-here") = opts.user {
                bail!("a toy has no such user");
            }
            let mut line = vec!["toy", "run"];
            line.extend_from_slice(argv);
            Ok(Job::argv(&line))
        }

        fn extract<'a>(&self, dir: &str) -> Job<'a> {
            Job::argv(&["toy", "unpack", dir])
        }

        fn session(&self, _: User, workdir: &str, _: &str) -> std::io::Result<ExitStatus> {
            self.host.interactive(&["toy", "shell", workdir])
        }

        fn disposable(&self) -> bool {
            false
        }
    }

    fn toy(host: &Arc<FakeHost>) -> Toy {
        Toy { host: host.clone() }
    }

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn a_new_endpoint_gets_commands_output_and_errors_for_free() {
        let host = Arc::new(FakeHost::new().reply(&["toy", "run", "bad"], Reply::fail(2, "no")));
        let toy = toy(&host);
        toy.exec(&["good"], &ExecOpts::default()).unwrap();
        let out = toy.output(&["bad"], &ExecOpts::default()).unwrap();
        assert!(!out.success);
        assert_eq!(out.stderr, "no");
        let err = toy.exec(&["bad"], &ExecOpts::default()).unwrap_err();
        assert!(err.to_string().contains("toy run failed"), "{err}");
        assert!(!toy.is_disposable());
    }

    #[test]
    fn the_endpoint_decides_who_can_be_acted_as_and_nothing_runs_for_the_others() {
        let host = Arc::new(FakeHost::new());
        let toy = toy(&host);
        let refused = ExecOpts::as_user(User::Named("nobody-here"));
        assert!(toy.exec(&["x"], &refused).is_err());
        assert!(toy.output(&["x"], &refused).is_err());
        assert!(
            toy.write_secret("/s", b"v", 0o600, User::Named("nobody-here"))
                .is_err()
        );
        assert!(host.argvs().is_empty());
    }

    #[test]
    fn only_a_step_marked_repeatable_is_run_again_after_a_lost_connection() {
        let lost = || Reply::fail(255, "connection lost");
        let host = Arc::new(FakeHost::remote("h").reply_once(&["toy", "run"], lost()));
        let toy = toy(&host);
        toy.exec(&["probe"], &ExecOpts::default().repeatable())
            .unwrap();
        assert_eq!(host.argvs().len(), 2, "a probe is tried again");

        let host = Arc::new(FakeHost::remote("h").reply_once(&["toy", "run"], lost()));
        let toy = self::toy(&host);
        assert!(toy.exec(&["install"], &ExecOpts::default()).is_err());
        assert_eq!(host.argvs().len(), 1, "an installer is run once");
    }

    #[test]
    fn files_go_in_as_a_tar_through_the_endpoints_unpacker_into_a_directory_made_first() {
        let host = Arc::new(FakeHost::new());
        let toy = toy(&host);
        let dir = crate::tmpfile::TempDir::new("inside-put").unwrap();
        std::fs::write(dir.path().join("f"), "hi").unwrap();
        let owner = ContainerUser {
            requested: None,
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        };
        toy.put(&dir.path().join("f").to_string_lossy(), "/h/f", &owner)
            .unwrap();
        let calls = host.calls();
        assert_eq!(calls[0].argv, words(&["toy", "run", "mkdir", "-p", "/h"]));
        assert_eq!(calls[1].argv, words(&["toy", "unpack", "/h"]));
        let mut archive = tar::Archive::new(calls[1].fed.as_slice());
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().to_string_lossy(), "f");
        assert_eq!(entry.header().uid().unwrap(), 1000);
    }

    #[test]
    fn a_secret_travels_on_stdin_and_a_session_is_the_endpoints_own() {
        let host = Arc::new(FakeHost::new());
        let toy = toy(&host);
        toy.write_secret("/s", b"hunter2", 0o600, User::Default)
            .unwrap();
        let call = &host.calls()[0];
        assert_eq!(call.fed, b"hunter2");
        assert!(call.argv.iter().all(|a| !a.contains("hunter2")));
        toy.interactive(User::Default, "/w", "bash").unwrap();
        assert_eq!(
            host.argvs().last().unwrap(),
            &words(&["interactive", "toy", "shell", "/w"])
        );
    }
}
