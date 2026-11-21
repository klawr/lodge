//! The docker-compatible command line (`docker`, `podman`) as a [`Runtime`],
//! [`Compose`], [`Container`] and [`Env`](super::Env). Podman's CLI is docker's
//! apart from a few places, which each engine's [`Flavor`] answers; everything else
//! is shared.
//!
//! It runs `docker …` (or `podman …`) *on a [`Host`](crate::host::Host)* it is given: this machine, or another one
//! the host reaches (so a remote engine needs no local docker CLI, only whatever
//! the host needs). It never starts a process itself and does not know which kind
//! of host it has.
//!
//! The trade-off versus `DOCKER_HOST=ssh://…` is that the client does not run
//! locally, so file copies can't be passed by path: the source files live on
//! *this* machine, not the host's. `cp_in` therefore streams the bytes through
//! the host's pipe instead.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::ExitStatus;

use super::{BuildSpec, Compose, Container, Containers, Listed, RunSpec, Runtime};
use crate::host::{HostRef, Input, Job, Repeat, Stream};

mod container;
mod copy;
mod docker;
mod podman;

pub use container::CliContainer;
pub use docker::{DockerFlavor, DockerOptions};
pub use podman::{PodmanFlavor, PodmanOptions};

/// What one engine of the docker-compatible family does its own way: its
/// program, and the few places where its command line or its behaviour differs
/// from what [`CliEngine`] does for all of them. Docker and Podman are different
/// engines that share the plumbing; each is a `Flavor`, and a [`CliEngine`] of it
/// asks the flavor instead of checking which one it is.
pub trait Flavor: Clone + Send + Sync + 'static {
    /// The program, and what the user calls the target (`--target`).
    fn program(&self) -> &'static str;

    /// Arguments that go right after the program on every command, to say which
    /// engine to talk to (a docker context, a podman connection): from the user's
    /// `[docker]` or `[podman]` settings.
    fn global_args(&self) -> Vec<String>;

    /// Flags for `run`, after the common ones and before the devcontainer's own
    /// `runArgs`, which can override them.
    fn extra_run_flags(&self, cli: &CliEngine<Self>, spec: &RunSpec) -> Vec<String>
    where
        Self: Sized;

    /// The arguments of the command that streams a tar into a container,
    /// keeping the archive's owners.
    fn cp_in_args<'a>(&self, target: &'a str) -> Vec<&'a str>;

    /// Whether `cp`'s error says the path is not in the container (as opposed to
    /// the container, the engine or the connection being the problem).
    fn says_path_is_missing(&self, stderr: &str) -> bool;

    /// The arguments that ask the compose provider for its version.
    fn compose_version_args(&self) -> &'static [&'static str];

    /// Whether a container's own address (from `inspect`) can be connected to
    /// from the host the engine runs on.
    fn addresses_are_reachable(&self, cli: &CliEngine<Self>) -> bool
    where
        Self: Sized;
}

/// A docker-compatible command line, run on a [`Host`](crate::host::Host).
#[derive(Clone)]
pub struct CliEngine<F: Flavor> {
    host: HostRef,
    flavor: F,
}

/// The `docker` CLI.
pub type Docker = CliEngine<DockerFlavor>;
/// The `podman` CLI.
pub type Podman = CliEngine<PodmanFlavor>;

impl<F: Flavor> CliEngine<F> {
    fn with(host: HostRef, flavor: F) -> Self {
        Self { host, flavor }
    }

    /// The program's name, for messages.
    pub(super) fn name(&self) -> &'static str {
        self.flavor.program()
    }

    /// The program, the flavor's own arguments for every command (which engine to
    /// talk to) and then `args`, as a vector.
    fn argv(&self, args: &[&str]) -> Vec<String> {
        let mut v = vec![self.flavor.program().to_string()];
        v.extend(self.flavor.global_args());
        v.extend(args.iter().map(|a| a.to_string()));
        v
    }

    /// A job that runs `<program> <args>` on the host. Its input is closed so a
    /// remote hop doesn't swallow the terminal; callers that need to stream data
    /// override it. Every container-engine command lodge runs is built here, in
    /// [`job_in`](Self::job_in) or in [`interactive`](Self::interactive).
    pub(super) fn job<'a>(&self, args: &[&str]) -> Job<'a> {
        let argv = self.argv(args);
        Job::argv(&argv.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// Like [`job`](Self::job) for operations that read *files* from disk (a
    /// compose project, a build context). Those are read client-side, i.e. on the
    /// host where the CLI runs, so the working directory has to be the project
    /// dir *there*, and the terminal stays available.
    fn job_in<'a>(&self, dir: Option<&'a Path>, args: &[&str]) -> Job<'a> {
        self.job(args).in_dir(dir).stdin(Input::Inherit)
    }

    /// Opens an interactive `<program> <args>` on the host, with a terminal, and
    /// waits for it.
    pub(super) fn interactive(&self, args: &[&str]) -> std::io::Result<ExitStatus> {
        let argv = self.argv(args);
        self.host
            .interactive(&argv.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// `compose -p <project> -f <file>… <tail…>`, run in `dir`.
    fn compose_job<'a>(
        &self,
        dir: &'a Path,
        project: &str,
        files: &[String],
        tail: &[&str],
    ) -> Job<'a> {
        let mut args = vec!["compose", "-p", project];
        for f in files {
            args.extend(["-f", f.as_str()]);
        }
        args.extend_from_slice(tail);
        self.job_in(Some(dir), &args)
    }

    /// Runs `<program> <args>` with inherited stdio; errors if it exits non-zero.
    /// Over ssh a dropped connection reruns it only if `repeat` says that is safe.
    fn run(&self, args: &[&str], repeat: Repeat) -> Result<()> {
        self.run_job(self.job(args), args, repeat)
    }

    /// Like [`run`](Self::run) for a job built by the caller; `args` name it in
    /// messages.
    fn run_job(&self, job: Job, args: &[&str], repeat: Repeat) -> Result<()> {
        let what = subcommand(args);
        let ran = self
            .host
            .exec(job.repeat(repeat))
            .with_context(|| format!("spawning {} {what}", self.name()))?;
        if !ran.status.success() {
            bail!("{} {what} failed ({})", self.name(), ran.status);
        }
        Ok(())
    }

    /// Runs a job, capturing its output, and returns stdout; a non-zero exit is an
    /// error carrying the engine's stderr.
    fn checked(&self, job: Job, args: &[&str], repeat: Repeat) -> Result<String> {
        let what = subcommand(args);
        let out = self
            .host
            .exec(job.capture().repeat(repeat))
            .with_context(|| format!("spawning {} {what}", self.name()))?;
        if !out.status.success() {
            bail!(
                "{} {what} failed: {}",
                self.name(),
                out.stderr_text().trim()
            );
        }
        Ok(out.stdout_text())
    }

    /// Runs `<program> <args>` quietly and returns stdout, or `None` if it could
    /// not run or exited non-zero. For probes whose failure is an answer, not an
    /// error. A probe changes nothing, so a dropped connection reruns it.
    fn capture_ok(&self, args: &[&str]) -> Option<String> {
        let job = self
            .job(args)
            .stdout(Stream::Capture)
            .stderr(Stream::Null)
            .repeat(Repeat::Safe);
        let out = self.host.exec(job).ok()?;
        out.status.success().then(|| out.stdout_text())
    }
}

/// "compose up", "ps", …: the words before the first flag, for messages.
fn subcommand(args: &[&str]) -> String {
    let words: Vec<&str> = args
        .iter()
        .copied()
        .take_while(|a| !a.starts_with('-'))
        .collect();
    match words.as_slice() {
        ["compose", ..] => {
            // `compose -p x -f y up …`: name the compose verb after the flags.
            let verb = args
                .iter()
                .skip(1)
                .scan(false, |skip, a| {
                    let take = !*skip && !a.starts_with('-');
                    *skip = matches!(*a, "-p" | "-f");
                    Some(take.then_some(*a))
                })
                .flatten()
                .next()
                .unwrap_or("");
            format!("compose {verb}")
        }
        _ => words.first().copied().unwrap_or("").to_string(),
    }
}

/// The `run` flags for `spec`, ahead of the image: the common ones, then the
/// engine's own (`extra`), then the devcontainer's `engine_args`, last so they can
/// override the rest.
fn run_flags(spec: &RunSpec, extra: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    for (k, v) in &spec.labels {
        args.extend(["--label".into(), format!("{k}={v}")]);
    }
    for (source, target) in &spec.binds {
        args.extend(["-v".into(), format!("{source}:{target}")]);
    }
    for mount in &spec.mounts {
        args.extend(["--mount".into(), mount.clone()]);
    }
    if let Some(dir) = &spec.workdir {
        args.extend(["-w".into(), dir.clone()]);
    }
    if spec.privileged {
        args.push("--privileged".into());
    }
    if spec.init {
        args.push("--init".into());
    }
    for cap in &spec.cap_add {
        args.extend(["--cap-add".into(), cap.clone()]);
    }
    for opt in &spec.security_opt {
        args.extend(["--security-opt".into(), opt.clone()]);
    }
    if let Some(user) = &spec.user {
        args.extend(["-u".into(), user.clone()]);
    }
    for port in &spec.publish {
        args.extend(["-p".into(), port.clone()]);
    }
    args.extend(extra.iter().cloned());
    args.extend(spec.engine_args.iter().cloned());
    args
}

impl<F: Flavor> Runtime for CliEngine<F> {
    fn container(&self, id: &str) -> Box<dyn Container> {
        Box::new(CliContainer {
            cli: self.clone(),
            id: id.to_string(),
            disposable: true,
        })
    }

    fn existing(&self, id: &str) -> Box<dyn Container> {
        Box::new(CliContainer {
            cli: self.clone(),
            id: id.to_string(),
            disposable: false,
        })
    }

    fn version(&self) -> Result<String> {
        match self.capture_ok(&["version", "--format", "{{.Server.Version}}"]) {
            Some(v) if !v.trim().is_empty() => Ok(format!("{} {}", self.name(), v.trim())),
            _ => bail!(
                "cannot reach {} (is it running, and do you have access?)",
                self.name()
            ),
        }
    }

    fn compose(&self) -> Result<&dyn Compose> {
        Ok(self)
    }

    fn list_labeled(&self, label: &str) -> Result<Vec<Listed>> {
        let format =
            format!("{{{{.ID}}}}\t{{{{.Names}}}}\t{{{{.State}}}}\t{{{{.Label \"{label}\"}}}}");
        let filter = format!("label={label}");
        let args = ["ps", "-a", "--filter", &filter, "--format", &format];
        Ok(parse_listed(&self.checked(
            self.job(&args),
            &args,
            Repeat::Safe,
        )?))
    }

    fn find_by_name(&self, name: &str) -> Result<Option<String>> {
        let filter = format!("name=^{name}$");
        let args = ["ps", "-aq", "--filter", &filter];
        Ok(first_line(&self.checked(
            self.job(&args),
            &args,
            Repeat::Safe,
        )?))
    }

    fn find_by_labels(&self, labels: &[(String, String)]) -> Result<Option<String>> {
        let mut args: Vec<String> = ["ps", "-aq"].map(String::from).to_vec();
        for (k, v) in labels {
            args.push("--filter".into());
            args.push(format!("label={k}={v}"));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        Ok(first_line(&self.checked(
            self.job(&args),
            &args,
            Repeat::Safe,
        )?))
    }

    fn build_image(&self, spec: &BuildSpec) -> Result<()> {
        let mut a: Vec<String> = vec!["build".into(), "-t".into(), spec.tag.clone()];
        if let Some(df) = &spec.dockerfile {
            a.push("-f".into());
            a.push(spec.context.join(df).to_string_lossy().into_owned());
        }
        if let Some(t) = &spec.stage {
            a.push("--target".into());
            a.push(t.clone());
        }
        for (k, v) in &spec.args {
            a.push("--build-arg".into());
            a.push(format!("{k}={v}"));
        }
        a.push(spec.context.to_string_lossy().into_owned());
        // The build context (and -f dockerfile) are read client-side, so the
        // build runs on the host that has them (through `job_in`).
        let args: Vec<&str> = a.iter().map(String::as_str).collect();
        // A build is cached layer by layer, so doing it again is harmless.
        self.run_job(self.job_in(None, &args), &args, Repeat::Safe)
    }

    fn run_detached(&self, spec: &RunSpec) -> Result<()> {
        let name = spec.name.as_str();
        let flags = run_flags(spec, &self.flavor.extra_run_flags(self, spec));
        let mut args: Vec<&str> = vec!["run", "-d", "--name", name];
        args.extend(flags.iter().map(String::as_str));
        args.push(&spec.image);
        args.extend(spec.command.iter().map(String::as_str));
        // `docker run` is not repeatable as such: a second one fails on the name
        // the first took, if it got that far. So a try after a lost connection
        // first looks for the container, and starts what it finds (it may have
        // been created but not started) instead of running again.
        let mut first = true;
        let ran = self
            .host
            .attempt(Repeat::Safe, || {
                if !std::mem::take(&mut first)
                    && let Ok(Some(_)) = self.find_by_name(name)
                {
                    return self.host.run(&mut self.job(&["start", name]));
                }
                self.host.run(&mut self.job(&args))
            })
            .with_context(|| format!("spawning {} run", self.name()))?;
        if !ran.status.success() {
            bail!("{} run failed ({})", self.name(), ran.status);
        }
        Ok(())
    }

    fn start(&self, id: &str) -> Result<()> {
        self.run(&["start", id], Repeat::Safe)
    }

    fn stop(&self, id: &str) -> Result<()> {
        self.run(&["stop", id], Repeat::Safe)
    }

    fn remove(&self, id: &str) -> Result<()> {
        // `-f` makes removing what is already gone a success.
        self.run(&["rm", "-f", id], Repeat::Safe)
    }
}

/// `docker compose`, which every current docker CLI has (as a plugin).
impl<F: Flavor> Compose for CliEngine<F> {
    fn version(&self) -> Result<String> {
        match self.capture_ok(self.flavor.compose_version_args()) {
            Some(v) if !v.trim().is_empty() => Ok(format!(
                "{} compose {}",
                self.name(),
                v.lines().next().unwrap_or("").trim()
            )),
            _ => bail!(
                "`{} compose` is not available (install a compose provider)",
                self.name()
            ),
        }
    }

    fn up(&self, dir: &Path, project: &str, files: &[String], extra: &[&str]) -> Result<()> {
        let mut tail = vec!["up", "-d"];
        tail.extend_from_slice(extra);
        // `up -d` converges on the same state when run again (with
        // `--force-recreate` it recreates once more, which costs time only).
        self.run_job(
            self.compose_job(dir, project, files, &tail),
            &["compose", "up"],
            Repeat::Safe,
        )
    }

    fn down(&self, dir: &Path, project: &str, files: &[String]) -> Result<()> {
        self.run_job(
            self.compose_job(dir, project, files, &["down"]),
            &["compose", "down"],
            Repeat::Safe,
        )
    }

    fn ps(
        &self,
        dir: &Path,
        project: &str,
        files: &[String],
        service: &str,
        which: Containers,
    ) -> Result<Option<String>> {
        let mut tail = vec!["ps"];
        if which == Containers::All {
            tail.push("-a");
        }
        tail.extend(["-q", service]);
        let out = self.checked(
            self.compose_job(dir, project, files, &tail),
            &["compose", "ps"],
            Repeat::Safe,
        )?;
        Ok(first_line(&out))
    }
}

/// The first non-empty line of `out`, trimmed.
fn first_line(out: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(String::from)
}

/// Parses `docker ps` lines of `id<TAB>name<TAB>state<TAB>label`.
fn parse_listed(out: &str) -> Vec<Listed> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            Some(Listed {
                id: f.next()?.trim().to_string(),
                name: f.next()?.trim().to_string(),
                running: f.next()?.trim() == "running",
                label: f.next().unwrap_or("").trim().to_string(),
            })
        })
        .filter(|l| !l.id.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Docker, Podman, RunSpec, parse_listed, run_flags, subcommand};
    use crate::host::LocalHost;
    use std::path::Path;
    use std::sync::Arc;

    #[test]
    fn a_run_spec_becomes_docker_flags_with_the_pass_through_ones_last() {
        let spec = RunSpec {
            labels: vec![("a".into(), "b".into())],
            binds: vec![("/src".into(), "/dst".into())],
            mounts: vec!["type=volume,source=v,target=/d".into()],
            workdir: Some("/w".into()),
            privileged: true,
            init: true,
            cap_add: vec!["SYS_PTRACE".into()],
            security_opt: vec!["seccomp=unconfined".into()],
            user: Some("dev".into()),
            publish: vec!["127.0.0.1:3000:3000".into()],
            engine_args: vec!["--cap-drop=ALL".into()],
            ..Default::default()
        };
        assert_eq!(
            run_flags(&spec, &[]),
            [
                "--label",
                "a=b",
                "-v",
                "/src:/dst",
                "--mount",
                "type=volume,source=v,target=/d",
                "-w",
                "/w",
                "--privileged",
                "--init",
                "--cap-add",
                "SYS_PTRACE",
                "--security-opt",
                "seccomp=unconfined",
                "-u",
                "dev",
                "-p",
                "127.0.0.1:3000:3000",
                "--cap-drop=ALL"
            ]
        );
        assert!(run_flags(&RunSpec::default(), &[]).is_empty());
    }

    #[test]
    fn parses_listed_containers() {
        let l = parse_listed("abc\tlodge-x-1\trunning\t/home/me/x\ndef\ty\texited\t/p q\n\n");
        assert_eq!(l.len(), 2);
        assert_eq!(
            (
                l[0].id.as_str(),
                l[0].name.as_str(),
                l[0].running,
                l[0].label.as_str()
            ),
            ("abc", "lodge-x-1", true, "/home/me/x")
        );
        assert!(!l[1].running);
        assert_eq!(l[1].label, "/p q");
    }

    #[test]
    fn the_program_is_the_first_word_of_the_argument_vector() {
        let args = ["exec", "my container", "sh"];
        assert_eq!(
            Docker::new(Arc::new(LocalHost)).argv(&args),
            ["docker", "exec", "my container", "sh"]
        );
        assert_eq!(
            Podman::new(Arc::new(LocalHost)).argv(&args),
            ["podman", "exec", "my container", "sh"]
        );
    }

    #[test]
    fn the_engines_own_flags_come_after_the_common_ones_and_before_runargs() {
        let spec = RunSpec {
            user: Some("dev".into()),
            engine_args: vec!["--cap-drop=ALL".into()],
            ..Default::default()
        };
        assert_eq!(
            run_flags(&spec, &["--mine".into()]),
            ["-u", "dev", "--mine", "--cap-drop=ALL"]
        );
    }

    #[test]
    fn messages_name_the_subcommand() {
        assert_eq!(subcommand(&["ps", "-a"]), "ps");
        assert_eq!(subcommand(&["compose", "up"]), "compose up");
        assert_eq!(
            subcommand(&["compose", "-p", "x", "-f", "a.yml", "down"]),
            "compose down"
        );
    }

    use crate::host::fake::{FakeHost, Reply};
    use crate::runtime::{BuildSpec, Containers, Runtime};

    fn docker(host: &Arc<FakeHost>) -> Docker {
        Docker::new(host.clone())
    }

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn containers_are_listed_and_found_through_the_host() {
        let host = Arc::new(
            FakeHost::new()
                .on(
                    &["docker", "ps", "-a"],
                    "abc\tlodge-x\trunning\t/p\ndef\ty\texited\t/q\n",
                )
                .on(&["docker", "ps", "-aq"], "abc\n"),
        );
        let rt = docker(&host);
        let listed = rt.list_labeled("dev.lodge.project").unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(rt.find_by_name("lodge-x").unwrap().as_deref(), Some("abc"));
        assert_eq!(
            host.argvs()[0],
            words(&[
                "docker",
                "ps",
                "-a",
                "--filter",
                "label=dev.lodge.project",
                "--format",
                "{{.ID}}\t{{.Names}}\t{{.State}}\t{{.Label \"dev.lodge.project\"}}"
            ])
        );
        assert_eq!(
            host.argvs()[1],
            words(&["docker", "ps", "-aq", "--filter", "name=^lodge-x$"])
        );
    }

    #[test]
    fn a_failing_engine_command_is_an_error_that_carries_its_message() {
        let host =
            Arc::new(FakeHost::new().reply(&["docker", "ps"], Reply::fail(1, "permission denied")));
        let err = docker(&host).find_by_name("x").unwrap_err();
        assert!(format!("{err:#}").contains("permission denied"), "{err:#}");
    }

    #[test]
    fn a_build_runs_where_the_context_is_with_the_terminal_and_is_repeatable() {
        let host =
            Arc::new(FakeHost::remote("h").reply_once(&["docker", "build"], Reply::fail(255, "")));
        let spec = BuildSpec {
            context: "/p/.devcontainer".into(),
            dockerfile: Some("Dockerfile".into()),
            stage: None,
            args: Default::default(),
            tag: "t".into(),
        };
        docker(&host).build_image(&spec).unwrap();
        let calls = host.calls();
        assert_eq!(
            calls.len(),
            2,
            "the dropped connection is retried: {calls:?}"
        );
        assert_eq!(
            calls[0].argv,
            words(&[
                "docker",
                "build",
                "-t",
                "t",
                "-f",
                "/p/.devcontainer/Dockerfile",
                "/p/.devcontainer"
            ])
        );
    }

    #[test]
    fn a_run_after_a_lost_connection_starts_the_container_it_finds_instead_of_running_again() {
        let host = Arc::new(
            FakeHost::remote("h")
                .reply_once(&["docker", "run"], Reply::fail(255, "connection lost"))
                .on(&["docker", "ps", "-aq"], "abc\n"),
        );
        let spec = RunSpec {
            name: "lodge-x".into(),
            image: "debian".into(),
            ..Default::default()
        };
        docker(&host).run_detached(&spec).unwrap();
        let argvs = host.argvs();
        assert_eq!(argvs[0][..4], words(&["docker", "run", "-d", "--name"])[..]);
        assert_eq!(argvs[1][..2], words(&["docker", "ps"])[..]);
        assert_eq!(argvs[2], words(&["docker", "start", "lodge-x"]));
        assert_eq!(argvs.len(), 3);
    }

    #[test]
    fn a_run_that_never_got_that_far_is_run_again() {
        let host = Arc::new(
            FakeHost::remote("h").reply_once(&["docker", "run"], Reply::fail(255, "refused")),
        );
        let spec = RunSpec {
            name: "n".into(),
            image: "debian".into(),
            ..Default::default()
        };
        docker(&host).run_detached(&spec).unwrap();
        let runs = host.argvs().iter().filter(|a| a[1] == "run").count();
        assert_eq!(runs, 2);
    }

    #[test]
    fn compose_runs_in_the_project_folder_with_its_files() {
        let host = Arc::new(FakeHost::new().on(
            &["docker", "compose", "-p", "p", "-f", "a.yml", "ps"],
            "svc1\n",
        ));
        let rt = docker(&host);
        let compose = Runtime::compose(&rt).unwrap();
        compose
            .up(
                Path::new("/p/.devcontainer"),
                "p",
                &["a.yml".into()],
                &["--build"],
            )
            .unwrap();
        let found = compose
            .ps(
                Path::new("/p/.devcontainer"),
                "p",
                &["a.yml".into()],
                "app",
                Containers::All,
            )
            .unwrap();
        assert_eq!(found.as_deref(), Some("svc1"));
        let calls = host.calls();
        assert_eq!(calls[0].dir.as_deref(), Some(Path::new("/p/.devcontainer")));
        assert_eq!(
            calls[0].argv,
            words(&[
                "docker", "compose", "-p", "p", "-f", "a.yml", "up", "-d", "--build"
            ])
        );
        assert_eq!(calls[1].argv.last().unwrap(), "app");
    }

    #[test]
    fn an_unreachable_engine_is_reported_by_name() {
        let host = Arc::new(FakeHost::new().reply(&["docker", "version"], Reply::fail(1, "")));
        let err = Runtime::version(&docker(&host)).unwrap_err();
        assert!(err.to_string().contains("docker"), "{err}");
        let ok = Arc::new(FakeHost::new().on(&["podman", "version"], "5.0.1\n"));
        assert_eq!(Runtime::version(&Podman::new(ok)).unwrap(), "podman 5.0.1");
    }

    #[test]
    fn the_engine_the_user_chose_is_named_on_every_command() {
        use super::{DockerOptions, PodmanOptions};
        let host = Arc::new(FakeHost::new());
        let docker = Docker::with_options(
            host.clone(),
            DockerOptions {
                context: Some("colima".into()),
            },
        );
        docker.find_by_name("x").unwrap();
        docker.start("c").unwrap();
        assert_eq!(
            host.argvs()[0][..4],
            words(&["docker", "--context", "colima", "ps"])[..]
        );
        assert_eq!(
            host.argvs()[1],
            words(&["docker", "--context", "colima", "start", "c"])
        );
        let host = Arc::new(FakeHost::new());
        let podman = Podman::with_options(
            host.clone(),
            PodmanOptions {
                connection: Some("vm".into()),
            },
        );
        podman.stop("c").unwrap();
        assert_eq!(
            host.argvs()[0],
            words(&["podman", "--connection", "vm", "stop", "c"])
        );
        // Nothing said: nothing added.
        let host = Arc::new(FakeHost::new());
        Docker::new(host.clone()).start("c").unwrap();
        assert_eq!(host.argvs()[0], words(&["docker", "start", "c"]));
    }
}
