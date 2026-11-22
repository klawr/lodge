//! A Kubernetes pod, as an endpoint: lodge runs commands in it and copies files
//! into it with `kubectl`, run on the [`Host`](crate::host::Host) it is given (this
//! machine, or another one over ssh, which then needs `kubectl` and the cluster's
//! credentials), and injects into the user the pod runs as.
//!
//! Like a plain host it is something that was there before lodge: nothing is
//! created, stopped or removed, no engine is involved, and since `kubectl exec`
//! has no flag to switch user, only the pod's own user can be acted as. Which pod
//! it is comes from the `[targets.kubernetes]` settings ([`KubeOptions`]).
//!
//! Its ports are reached by `kubectl port-forward`, which is a process rather than
//! an address: the forwarder asks [`Container::relay`] to start it
//! ([`Reach::Relayed`]).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::ExitStatus;

use crate::fingerprint::content_hash;
use crate::host::{Child, HostRef, Job, Repeat, Stream};
use crate::port::PortForward;
use crate::provision::{Attached, Endpoints, Found, Lifecycle, Probe, Status};
use crate::runtime::{
    Container, ContainerUser, ExecOpts, Inside, LoginOnly, Reach, Runtime, User, in_dir_with_env,
};
use crate::shell::shell_quote;

/// What the user can say about Kubernetes, under `[targets.kubernetes]` in the
/// config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct KubeOptions {
    /// The pod to work in (`kubectl exec POD`). Every command that uses the
    /// target needs this or `selector`.
    #[serde(default)]
    pub pod: Option<String>,
    /// A label selector (`kubectl get pods -l SELECTOR`, such as `app=api`) that
    /// finds the pod instead of its name, for pods whose names change. It must
    /// match exactly one running pod: lodge does not pick among several. The pod
    /// is looked up once per run of `ldg`; use it instead of `pod`, not with it.
    #[serde(default)]
    pub selector: Option<String>,
    /// The container of the pod to work in (`kubectl exec -c NAME`). Default: the
    /// one kubectl picks, as for `kubectl exec`.
    #[serde(default)]
    pub container: Option<String>,
    /// The namespace of the pod (`kubectl --namespace NAME`). Default: the
    /// context's.
    #[serde(default)]
    pub namespace: Option<String>,
    /// The kubectl context to use (`kubectl --context NAME`): which cluster to
    /// talk to when you have several. Default: the current one.
    #[serde(default)]
    pub context: Option<String>,
    /// The directory in the pod a session starts in. Default: the home of the
    /// user the pod runs as. The project path of `ldg up` is a folder on the host,
    /// so it plays no part here.
    #[serde(default)]
    pub workdir: Option<String>,
}

impl KubeOptions {
    /// `kubectl` and what comes right after it on every command: which cluster
    /// and namespace to talk to.
    fn kubectl(&self) -> Vec<String> {
        let mut line = vec!["kubectl".to_string()];
        for (flag, value) in [
            ("--context", &self.context),
            ("--namespace", &self.namespace),
        ] {
            if let Some(value) = value {
                line.push(flag.to_string());
                line.push(value.clone());
            }
        }
        line
    }
}

/// Which pod the settings point at.
enum Aim<'a> {
    Named(&'a str),
    Selected(&'a str),
}

impl KubeOptions {
    /// The pod, or what to do about it not being set.
    fn aim(&self) -> Result<Aim<'_>> {
        match (&self.pod, &self.selector) {
            (Some(pod), None) => Ok(Aim::Named(pod)),
            (None, Some(selector)) => Ok(Aim::Selected(selector)),
            (Some(_), Some(_)) => {
                bail!("set `pod` or `selector` under [targets.kubernetes], not both")
            }
            (None, None) => bail!(
                "no pod to work in: set `pod` (or `selector`) under [targets.kubernetes] in the config"
            ),
        }
    }
}

/// Which pod a selector found.
enum Lookup {
    Found(String),
    /// No running pod matches; the words for it.
    Nothing(String),
}

/// What the cluster says about the pod.
enum Seen {
    /// There is no such pod (or namespace); what kubectl said.
    Gone(String),
    /// Its phase: `Running`, `Pending`, `Succeeded`, `Failed`, `Unknown`.
    Phase(String),
}

/// A pod, seen as an endpoint.
pub struct KubePod {
    host: HostRef,
    options: KubeOptions,
    /// What identifies this pod in lodge's own state (the forwarding daemon's
    /// files): a short, file-name-safe tag of cluster, namespace and name.
    id: String,
    /// Only the user the pod runs as can be acted as.
    acting: LoginOnly,
    /// The pod a selector found, kept for the rest of the run: every command goes
    /// to the same pod, whatever happens to the others.
    selected: std::sync::Mutex<Option<String>>,
}

impl KubePod {
    pub fn new(host: HostRef, options: KubeOptions) -> Self {
        let name = format!(
            "{}/{}/{}/{}",
            host.label().unwrap_or_default(),
            options.context.as_deref().unwrap_or_default(),
            options.namespace.as_deref().unwrap_or_default(),
            // A selector names the work, not one pod, so what the forwarding
            // daemon keeps stays the same when the pod is replaced.
            match (&options.pod, &options.selector) {
                (Some(pod), _) => pod.clone(),
                (None, Some(selector)) => format!("-l {selector}"),
                (None, None) => String::new(),
            }
        );
        Self {
            id: format!("kube-{}", &content_hash(name.as_bytes())[..7]),
            host,
            options,
            acting: LoginOnly::new("a pod"),
            selected: std::sync::Mutex::new(None),
        }
    }

    /// What a selector found, if it has. A lock that a panic poisoned still holds
    /// a name that is good.
    fn selected(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.selected.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The pod to act on: the one named, or the one the selector finds.
    fn lookup(&self) -> Result<Lookup> {
        let selector = match self.options.aim()? {
            Aim::Named(pod) => return Ok(Lookup::Found(pod.to_string())),
            Aim::Selected(selector) => selector,
        };
        if let Some(pod) = self.selected().clone() {
            return Ok(Lookup::Found(pod));
        }
        let mut line = self.options.kubectl();
        line.extend(
            [
                "get",
                "pods",
                "-l",
                selector,
                "--field-selector=status.phase=Running",
                "-o",
                "jsonpath={.items[*].metadata.name}",
            ]
            .map(String::from),
        );
        let words: Vec<&str> = line.iter().map(String::as_str).collect();
        let out = self
            .host
            .exec(Job::argv(&words).capture().repeat(Repeat::Safe))
            .context("spawning kubectl get")?;
        if !out.status.success() {
            bail!(
                "kubectl get pods -l {selector} failed: {}",
                out.stderr_text().trim()
            );
        }
        let text = out.stdout_text();
        let names: Vec<&str> = text.split_whitespace().collect();
        match names.as_slice() {
            [] => Ok(Lookup::Nothing(format!(
                "no running pod matches the selector {selector}"
            ))),
            [one] => {
                *self.selected() = Some(one.to_string());
                Ok(Lookup::Found(one.to_string()))
            }
            many => bail!(
                "the selector {selector} matches {} running pods ({}): narrow it to one, or \
                 name the pod",
                many.len(),
                many.iter().take(5).copied().collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// The pod to act on, or why there is none.
    fn pod(&self) -> Result<String> {
        match self.lookup()? {
            Lookup::Found(pod) => Ok(pod),
            Lookup::Nothing(why) => bail!("{why}"),
        }
    }

    /// `kubectl exec` into the pod, up to the `--` before the command. `--quiet`
    /// keeps its "defaulted container" remark out of the command's own output.
    fn exec_line(&self, flags: &[&str]) -> Result<Vec<String>> {
        let mut line = self.options.kubectl();
        line.push("exec".into());
        line.push("--quiet".into());
        line.extend(flags.iter().map(|f| f.to_string()));
        line.push(self.pod()?);
        if let Some(container) = &self.options.container {
            line.push("-c".into());
            line.push(container.clone());
        }
        line.push("--".into());
        Ok(line)
    }

    /// What the cluster says about the pod. An error when it could not be asked,
    /// which is not "gone".
    fn seen(&self) -> Result<Seen> {
        let pod = match self.lookup()? {
            Lookup::Found(pod) => pod,
            Lookup::Nothing(why) => return Ok(Seen::Gone(why)),
        };
        let mut line = self.options.kubectl();
        line.extend([
            "get".into(),
            "pod".into(),
            pod,
            "-o".into(),
            "jsonpath={.status.phase}".into(),
        ]);
        let words: Vec<&str> = line.iter().map(String::as_str).collect();
        let out = self
            .host
            .exec(Job::argv(&words).capture().repeat(Repeat::Safe))
            .context("spawning kubectl get")?;
        if out.status.success() {
            return Ok(Seen::Phase(out.stdout_text().trim().to_string()));
        }
        let err = out.stderr_text();
        if err.contains("(NotFound)") {
            return Ok(Seen::Gone(err.trim().to_string()));
        }
        bail!("kubectl get failed: {}", err.trim())
    }

    /// The pod, for a message.
    fn pod_name(&self) -> String {
        match self.options.aim() {
            Ok(Aim::Named(pod)) => pod.to_string(),
            Ok(Aim::Selected(selector)) => format!("selected by {selector}"),
            Err(_) => "(no pod set)".to_string(),
        }
    }
}

/// What is a pod's own about working inside it: `kubectl exec` for a command (in
/// a `cd … && env …` script when there is a directory or an environment, since it
/// has no flag for either), `tar -x` in the pod to unpack, and `exec -it` for a
/// terminal. The rest of [`Env`](crate::runtime::Env) is built on this in
/// `runtime::inside`.
impl Inside for KubePod {
    fn host(&self) -> &HostRef {
        &self.host
    }

    fn describe(&self) -> String {
        "kubectl exec".into()
    }

    fn command<'a>(&self, argv: &[&str], opts: &ExecOpts, stdin: bool) -> Result<Job<'a>> {
        self.acting.check(self, opts.user)?;
        let mut line = self.exec_line(if stdin { &["-i"] } else { &[] })?;
        match in_dir_with_env(argv, opts) {
            Some(script) => line.extend(["sh".into(), "-c".into(), script]),
            None => line.extend(argv.iter().map(|a| a.to_string())),
        }
        let words: Vec<&str> = line.iter().map(String::as_str).collect();
        Ok(Job::argv(&words))
    }

    /// The pod's own `tar`, which owns what it unpacks as the user it runs as. Not
    /// `kubectl cp`, which would need the same `tar` and cannot take a stream.
    fn extract<'a>(&self, dir: &str) -> Job<'a> {
        match self.exec_line(&["-i"]) {
            Ok(mut line) => {
                line.extend(["tar", "-xpf", "-", "-C", dir].map(String::from));
                let words: Vec<&str> = line.iter().map(String::as_str).collect();
                Job::argv(&words)
            }
            // Without a pod there is nothing to unpack into: a job that fails, and
            // says why where the caller reports it.
            Err(e) => Job::shell(&format!("echo {} >&2; exit 1", shell_quote(&e.to_string()))),
        }
    }

    fn session(&self, user: User, workdir: &str, inner: &str) -> std::io::Result<ExitStatus> {
        self.acting
            .check(self, user)
            .map_err(std::io::Error::other)?;
        let mut line = self
            .exec_line(&["-it"])
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        line.extend([
            "sh".into(),
            "-c".into(),
            format!("cd {} && {inner}", shell_quote(workdir)),
        ]);
        let words: Vec<&str> = line.iter().map(String::as_str).collect();
        self.host.interactive(&words)
    }

    /// A pod is someone's workload: lodge leaves the dotfiles it did not place
    /// alone and does not loosen git's ownership check in it.
    fn disposable(&self) -> bool {
        false
    }
}

impl Container for KubePod {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> Option<String> {
        self.options.pod.clone().or_else(|| self.selected().clone())
    }

    fn mounts(&self) -> Vec<String> {
        Vec::new()
    }

    fn is_running(&self) -> Result<bool> {
        Ok(matches!(self.seen()?, Seen::Phase(p) if p == "Running"))
    }

    /// Through a command in the pod: a pod that does not run cannot be read.
    fn read_stored_file(&self, path: &str) -> Result<Option<String>> {
        crate::runtime::Env::read_file(self, path)
    }

    fn configured_user(&self) -> Option<String> {
        None
    }

    /// Only the pod's own ports, by `kubectl port-forward`.
    fn reach(&self, peer: Option<&str>) -> Option<Reach> {
        match peer {
            None => Some(Reach::Relayed),
            Some(_) => None,
        }
    }

    /// `kubectl port-forward` on the host, listening on loopback port `local`.
    fn relay(
        &self,
        local: u16,
        port: u16,
        chatter: Stream,
    ) -> Option<std::io::Result<Box<dyn Child>>> {
        let pod = match self.pod() {
            Ok(pod) => pod,
            Err(e) => return Some(Err(std::io::Error::other(format!("{e:#}")))),
        };
        let mut line = self.options.kubectl();
        line.extend([
            "port-forward".into(),
            "--address".into(),
            "127.0.0.1".into(),
            format!("pod/{pod}"),
            format!("{local}:{port}"),
        ]);
        let words: Vec<&str> = line.iter().map(String::as_str).collect();
        Some(self.host.spawn(&words, chatter))
    }
}

/// Makes a pod ready for a project: there is nothing to make, so this checks that
/// it is there and running and hands it back.
pub struct KubeEndpoints {
    host: HostRef,
    options: KubeOptions,
}

impl KubeEndpoints {
    pub fn new(host: HostRef, options: KubeOptions) -> Self {
        Self { host, options }
    }

    fn pod(&self) -> KubePod {
        KubePod::new(self.host.clone(), self.options.clone())
    }

    fn found(&self) -> Found {
        Found {
            container: Box::new(self.pod()),
            remote_user: None,
        }
    }
}

impl Endpoints for KubeEndpoints {
    fn provision(&self, _project: &Path, _rebuild: bool) -> Result<Attached> {
        let pod = self.pod();
        match pod.seen()? {
            Seen::Gone(why) => bail!("there is no pod ({}): {why}", pod.pod_name()),
            Seen::Phase(phase) if phase != "Running" => {
                bail!("the pod {} is {phase}, not Running", pod.pod_name())
            }
            Seen::Phase(_) => {}
        }
        let workdir = match &self.options.workdir {
            Some(dir) => dir.clone(),
            None => ContainerUser::resolve(&pod, None)?.home,
        };
        Ok(Attached {
            container: Box::new(pod),
            // Never made by lodge, so whether anything is injected is for the
            // record in its prefix to say.
            created: false,
            workdir,
            name: None,
            forward_ports: Vec::<PortForward>::new(),
            remote_user: None,
            post_attach: None,
        })
    }

    fn find(&self, _project: &Path) -> Result<Option<Found>> {
        Ok(match self.pod().seen()? {
            Seen::Gone(_) => None,
            Seen::Phase(_) => Some(self.found()),
        })
    }

    fn status(&self, _project: &Path) -> Result<Option<Status>> {
        Ok(match self.pod().seen()? {
            Seen::Gone(_) => None,
            Seen::Phase(phase) => Some(Status {
                found: self.found(),
                running: phase == "Running",
                config: None,
            }),
        })
    }

    fn lifecycle(&self) -> Option<&dyn Lifecycle> {
        // Nothing to stop, remove or list: the pod was there before lodge.
        None
    }

    fn container(&self, _id: &str) -> Box<dyn Container> {
        Box::new(self.pod())
    }

    fn engine(&self) -> Option<&dyn Runtime> {
        None
    }

    fn needs_project_path(&self) -> bool {
        false
    }

    fn checks(&self) -> Vec<Probe> {
        let mut line = self.options.kubectl();
        line.extend(["version", "--client"].map(String::from));
        let words: Vec<&str> = line.iter().map(String::as_str).collect();
        let version = self.host.exec(Job::argv(&words).capture());
        let kubectl = match version {
            Ok(out) if out.status.success() => Ok(out
                .stdout_text()
                .lines()
                .next()
                .unwrap_or("kubectl")
                .trim()
                .to_string()),
            Ok(out) => Err(format!(
                "kubectl version failed: {}",
                out.stderr_text().trim()
            )),
            Err(e) => Err(format!(
                "kubectl cannot be run ({e:#}); install it (https://kubernetes.io/docs/tasks/tools/)"
            )),
        };
        let usable = kubectl.is_ok();
        let mut probes = vec![Probe {
            name: "kubectl".into(),
            outcome: kubectl,
        }];
        if usable {
            let pod = self.pod();
            probes.push(Probe {
                name: "pod".into(),
                outcome: match pod.seen() {
                    Ok(Seen::Phase(phase)) if phase == "Running" => {
                        Ok(format!("{} is Running", pod.pod_name()))
                    }
                    Ok(Seen::Phase(phase)) => {
                        Err(format!("{} is {phase}, not Running", pod.pod_name()))
                    }
                    Ok(Seen::Gone(why)) => Err(format!("there is no pod: {why}")),
                    Err(e) => Err(format!("{e:#}")),
                },
            });
        }
        probes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::{FakeHost, Reply};
    use crate::runtime::Env as _;
    use std::sync::Arc;

    const PROBE: &str = crate::runtime::PROBE_SCRIPT;

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(|w| w.to_string()).collect()
    }

    fn options() -> KubeOptions {
        KubeOptions {
            pod: Some("dev-0".into()),
            ..KubeOptions::default()
        }
    }

    /// A pod whose user is `dev` (uid 1000), on a host that reaches the cluster.
    fn pod_on(host: &Arc<FakeHost>, options: KubeOptions) -> KubePod {
        KubePod::new(host.clone(), options)
    }

    fn cluster_host() -> FakeHost {
        FakeHost::remote("me@srv").on(
            &[
                "kubectl", "exec", "--quiet", "dev-0", "--", "sh", "-c", PROBE,
            ],
            "1000\n1000\ndev\n/home/dev\n",
        )
    }

    fn cluster() -> Arc<FakeHost> {
        Arc::new(cluster_host())
    }

    #[test]
    fn a_command_is_kubectl_exec_in_the_pod_with_the_clusters_flags_first() {
        let host = cluster();
        let options = KubeOptions {
            context: Some("prod".into()),
            namespace: Some("work".into()),
            container: Some("main".into()),
            ..options()
        };
        pod_on(&host, options)
            .exec(&["true"], &ExecOpts::default())
            .unwrap();
        assert_eq!(
            host.argvs()[0],
            words(&[
                "kubectl",
                "--context",
                "prod",
                "--namespace",
                "work",
                "exec",
                "--quiet",
                "dev-0",
                "-c",
                "main",
                "--",
                "true"
            ])
        );
    }

    #[test]
    fn a_directory_and_an_environment_become_a_script_since_exec_has_no_flag_for_them() {
        let host = cluster();
        let pod = pod_on(&host, options());
        let env = vec![("A".to_string(), "x y".to_string())];
        let opts = ExecOpts {
            workdir: Some("/w d"),
            env: &env,
            ..ExecOpts::default()
        };
        pod.exec(&["echo", "hi"], &opts).unwrap();
        assert_eq!(
            host.argvs()[0],
            words(&[
                "kubectl",
                "exec",
                "--quiet",
                "dev-0",
                "--",
                "sh",
                "-c",
                "cd '/w d' && exec env 'A=x y' 'echo' 'hi'"
            ])
        );
    }

    #[test]
    fn output_reports_failure_as_a_value_and_exec_as_an_error() {
        let host = Arc::new(FakeHost::new().reply(&["kubectl", "exec"], Reply::fail(1, "no")));
        let pod = pod_on(&host, options());
        let out = pod.output(&["false"], &ExecOpts::default()).unwrap();
        assert!(!out.success);
        assert_eq!(out.stderr, "no");
        assert!(pod.exec(&["false"], &ExecOpts::default()).is_err());
    }

    #[test]
    fn only_the_user_the_pod_runs_as_can_be_acted_as() {
        let host = cluster();
        let pod = pod_on(&host, options());
        let as_user = |user| ExecOpts::as_user(user);
        pod.exec(&["true"], &as_user(User::Default)).unwrap();
        pod.exec(&["true"], &as_user(User::Named("dev"))).unwrap();
        let other = pod
            .exec(&["true"], &as_user(User::Named("alice")))
            .unwrap_err();
        assert!(other.to_string().contains("a pod") && other.to_string().contains("dev"));
        assert!(
            pod.exec(&["true"], &as_user(User::Root))
                .unwrap_err()
                .to_string()
                .contains("root")
        );
        let ran = host
            .argvs()
            .iter()
            .filter(|a| a.ends_with(&words(&["--", "true"])))
            .count();
        assert_eq!(ran, 2, "the refused ones never reached kubectl");
    }

    #[test]
    fn root_can_be_acted_as_when_that_is_who_the_pod_runs_as() {
        let host = Arc::new(FakeHost::new().on(&["kubectl", "exec"], "0\n0\nroot\n/root\n"));
        pod_on(&host, options())
            .exec(&["true"], &ExecOpts::root())
            .unwrap();
    }

    #[test]
    fn files_go_in_as_a_tar_stream_unpacked_by_the_pods_own_tar_into_a_directory_made_first() {
        let host = cluster();
        let pod = pod_on(&host, options());
        let dir = crate::tmpfile::TempDir::new("kube-put").unwrap();
        std::fs::write(dir.path().join("rc"), "set x").unwrap();
        let user = ContainerUser::resolve(&pod, None).unwrap();
        pod.put(
            &dir.path().join("rc").to_string_lossy(),
            "/home/dev/.config/hello/rc",
            &user,
        )
        .unwrap();
        let calls = host.calls();
        let unpack = [
            "kubectl", "exec", "--quiet", "-i", "dev-0", "--", "tar", "-xpf", "-",
        ];
        let mkdir = calls
            .iter()
            .position(|c| {
                c.argv
                    .ends_with(&words(&["mkdir", "-p", "/home/dev/.config/hello"]))
            })
            .expect("the directory is made first");
        let tar = calls
            .iter()
            .position(|c| {
                c.argv == words(&[unpack.as_slice(), &["-C", "/home/dev/.config/hello"]].concat())
            })
            .expect("the pod unpacks it, reading its input");
        assert!(mkdir < tar);
        let mut archive = tar::Archive::new(calls[tar].fed.as_slice());
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().to_string_lossy(), "rc");
        assert_eq!(entry.header().uid().unwrap(), 1000);
    }

    #[test]
    fn a_secret_travels_on_stdin_and_never_in_the_arguments() {
        let host = cluster();
        pod_on(&host, options())
            .write_secret("/dev/shm/s", b"hunter2", 0o600, User::Default)
            .unwrap();
        let call = host.calls().pop().unwrap();
        assert!(call.argv.contains(&"-i".to_string()), "stdin is attached");
        assert_eq!(call.fed, b"hunter2");
        assert!(call.argv.iter().all(|a| !a.contains("hunter2")), "{call:?}");
    }

    #[test]
    fn a_session_is_a_shell_with_a_terminal_in_the_directory_in_the_pod() {
        let host = cluster();
        pod_on(&host, options())
            .interactive(User::Default, "/w", "exec bash -l")
            .unwrap();
        assert_eq!(
            host.argvs().last().unwrap(),
            &words(&[
                "interactive",
                "kubectl",
                "exec",
                "--quiet",
                "-it",
                "dev-0",
                "--",
                "sh",
                "-c",
                "cd '/w' && exec bash -l"
            ])
        );
    }

    #[test]
    fn running_means_the_phase_is_running_and_a_pod_that_is_gone_is_not_an_error() {
        let get = [
            "kubectl",
            "get",
            "pod",
            "dev-0",
            "-o",
            "jsonpath={.status.phase}",
        ];
        let on = |reply: Reply| pod_on(&Arc::new(FakeHost::new().reply(&get, reply)), options());
        assert!(on(Reply::ok("Running")).is_running().unwrap());
        assert!(!on(Reply::ok("Pending")).is_running().unwrap());
        let gone = Reply::fail(1, "Error from server (NotFound): pods \"dev-0\" not found");
        assert!(!on(gone).is_running().unwrap());
        // Not being able to ask is not "stopped".
        let refused = Reply::fail(1, "Unable to connect to the server: dial tcp: refused");
        assert!(on(refused).is_running().is_err());
    }

    #[test]
    fn its_ports_are_reached_by_a_relay_on_the_host_and_it_has_no_peers() {
        let host = Arc::new(FakeHost::remote("me@srv"));
        let options = KubeOptions {
            context: Some("prod".into()),
            ..options()
        };
        let pod = pod_on(&host, options);
        assert_eq!(pod.reach(None), Some(Reach::Relayed));
        assert_eq!(pod.reach(Some("db")), None);
        let relay = pod.relay(9000, 80, Stream::Null).unwrap();
        assert!(relay.is_ok());
        assert_eq!(
            host.argvs(),
            [words(&[
                "spawn",
                "kubectl",
                "--context",
                "prod",
                "port-forward",
                "--address",
                "127.0.0.1",
                "pod/dev-0",
                "9000:80"
            ])]
        );
    }

    #[test]
    fn it_is_told_apart_by_cluster_namespace_and_name_in_a_name_safe_for_a_file() {
        let id = |context: &str, pod: &str| {
            KubePod::new(
                Arc::new(FakeHost::new()),
                KubeOptions {
                    context: Some(context.into()),
                    pod: Some(pod.into()),
                    ..KubeOptions::default()
                },
            )
            .id()
            .to_string()
        };
        assert_eq!(id("a", "p"), id("a", "p"));
        assert_ne!(id("a", "p"), id("b", "p"));
        assert_ne!(id("a", "p"), id("a", "q"));
        assert_eq!(
            id("a", "p").len(),
            12,
            "the forward state keeps 12 characters"
        );
        assert!(
            id("a", "p")
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        );
    }

    #[test]
    fn a_pod_is_not_disposable_and_has_no_mounts_and_no_configured_user() {
        let pod = pod_on(&cluster(), options());
        assert!(!pod.is_disposable());
        assert!(pod.mounts().is_empty());
        assert_eq!(pod.configured_user(), None);
        assert_eq!(pod.display_name(), "dev-0");
    }

    #[test]
    fn no_pod_in_the_settings_is_said_where_it_is_needed_not_when_the_target_is_built() {
        let host = Arc::new(FakeHost::new());
        let pod = pod_on(&host, KubeOptions::default());
        let err = pod.exec(&["true"], &ExecOpts::default()).unwrap_err();
        assert!(err.to_string().contains("set `pod`"), "{err}");
        let endpoints = KubeEndpoints::new(host.clone(), KubeOptions::default());
        assert!(endpoints.provision(Path::new("."), false).is_err());
        assert!(host.argvs().is_empty(), "nothing was run without a pod");
    }

    #[test]
    fn provisioning_checks_the_pod_runs_and_works_in_its_users_home_or_the_directory_set() {
        let get = ["kubectl", "get", "pod", "dev-0"];
        let running = || cluster_host().reply(&get, Reply::ok("Running"));
        let host = Arc::new(running());
        let attached = KubeEndpoints::new(host, options())
            .provision(Path::new("/on/the/host"), true)
            .unwrap();
        assert_eq!(attached.workdir, "/home/dev");
        assert!(
            !attached.created,
            "lodge made nothing, even when asked to rebuild"
        );
        assert!(attached.remote_user.is_none() && attached.post_attach.is_none());

        let set = KubeOptions {
            workdir: Some("/srv/app".into()),
            ..options()
        };
        let attached = KubeEndpoints::new(Arc::new(running()), set)
            .provision(Path::new("."), false)
            .unwrap();
        assert_eq!(attached.workdir, "/srv/app");

        let pending = Arc::new(FakeHost::new().reply(&get, Reply::ok("Pending")));
        let err = KubeEndpoints::new(pending, options())
            .provision(Path::new("."), false)
            .err()
            .unwrap();
        assert!(err.to_string().contains("Pending"), "{err}");

        let gone = Reply::fail(1, "Error from server (NotFound): pods \"dev-0\" not found");
        let host = Arc::new(FakeHost::new().reply(&get, gone));
        let err = KubeEndpoints::new(host, options())
            .provision(Path::new("."), false)
            .err()
            .unwrap();
        assert!(err.to_string().contains("no pod (dev-0)"), "{err}");
    }

    #[test]
    fn find_and_status_follow_the_cluster_and_there_is_no_lifecycle_and_no_engine() {
        let get = ["kubectl", "get", "pod", "dev-0"];
        let endpoints = |reply: Reply| {
            KubeEndpoints::new(Arc::new(FakeHost::new().reply(&get, reply)), options())
        };
        let project = Path::new(".");
        let up = endpoints(Reply::ok("Running"));
        assert!(up.find(project).unwrap().is_some());
        let status = up.status(project).unwrap().unwrap();
        assert!(status.running);
        assert_eq!(status.config, None);
        let down = endpoints(Reply::ok("Failed"));
        assert!(!down.status(project).unwrap().unwrap().running);
        let gone = endpoints(Reply::fail(1, "Error from server (NotFound): not found"));
        assert!(gone.find(project).unwrap().is_none());
        assert!(gone.status(project).unwrap().is_none());
        assert!(up.lifecycle().is_none() && up.engine().is_none());
        assert!(!up.needs_project_path());
        assert_eq!(up.container("whatever").id(), up.pod().id());
    }

    // ---- a pod found by a selector ----

    const BY_LABEL: [&str; 8] = [
        "kubectl",
        "get",
        "pods",
        "-l",
        "app=api",
        "--field-selector=status.phase=Running",
        "-o",
        "jsonpath={.items[*].metadata.name}",
    ];

    fn selecting() -> KubeOptions {
        KubeOptions {
            selector: Some("app=api".into()),
            ..KubeOptions::default()
        }
    }

    #[test]
    fn a_selector_finds_the_one_running_pod_and_every_command_goes_to_it_after_one_lookup() {
        let host = Arc::new(FakeHost::new().reply(&BY_LABEL, Reply::ok("api-7d9f\n")));
        let pod = pod_on(&host, selecting());
        let _ = pod.exec(&["true"], &ExecOpts::default());
        let _ = pod.exec(&["true"], &ExecOpts::default());
        let argvs = host.argvs();
        assert_eq!(
            argvs
                .iter()
                .filter(|a| a.contains(&"pods".to_string()))
                .count(),
            1,
            "looked up once: {argvs:?}"
        );
        for exec in argvs.iter().filter(|a| a.contains(&"exec".to_string())) {
            assert!(exec.contains(&"api-7d9f".to_string()), "{exec:?}");
        }
        assert_eq!(pod.display_name(), "api-7d9f");
    }

    #[test]
    fn a_selector_that_matches_many_or_none_or_fails_is_never_guessed_at() {
        let many = Arc::new(FakeHost::new().reply(&BY_LABEL, Reply::ok("api-a api-b")));
        let err = pod_on(&many, selecting())
            .exec(&["true"], &ExecOpts::default())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("matches 2 running pods (api-a, api-b)"),
            "{err}"
        );
        assert_eq!(many.argvs().len(), 1, "nothing ran in either pod");

        let none = Arc::new(FakeHost::new().reply(&BY_LABEL, Reply::ok("")));
        let pod = pod_on(&none, selecting());
        let err = pod.exec(&["true"], &ExecOpts::default()).unwrap_err();
        assert!(err.to_string().contains("no running pod matches"), "{err}");
        // Not there is an answer for `find`, not an error.
        assert!(!pod.is_running().unwrap());

        let broken = Arc::new(FakeHost::new().reply(&BY_LABEL, Reply::fail(1, "forbidden")));
        let err = pod_on(&broken, selecting()).is_running().unwrap_err();
        assert!(format!("{err:#}").contains("forbidden"), "{err:#}");
    }

    #[test]
    fn a_pod_and_a_selector_together_are_refused_and_a_selector_is_one_identity_for_the_daemon() {
        let both = KubeOptions {
            pod: Some("dev-0".into()),
            ..selecting()
        };
        let err = pod_on(&Arc::new(FakeHost::new()), both)
            .exec(&["true"], &ExecOpts::default())
            .unwrap_err();
        assert!(err.to_string().contains("not both"), "{err}");
        let host = Arc::new(FakeHost::new());
        let id = |options| pod_on(&host, options).id().to_string();
        assert_eq!(id(selecting()), id(selecting()));
        assert_ne!(id(selecting()), id(options()));
    }

    #[test]
    fn the_relay_goes_to_the_selected_pod() {
        let host = Arc::new(FakeHost::new().reply(&BY_LABEL, Reply::ok("api-7d9f")));
        let relay = pod_on(&host, selecting())
            .relay(9000, 80, Stream::Null)
            .unwrap();
        assert!(relay.is_ok());
        assert!(
            host.argvs()
                .last()
                .unwrap()
                .contains(&"pod/api-7d9f".to_string())
        );
    }

    // ---- what doctor checks ----

    #[test]
    fn doctor_checks_kubectl_and_then_the_pod() {
        let version = ["kubectl", "version", "--client"];
        let get = ["kubectl", "get", "pod", "dev-0"];
        let ok = Arc::new(
            FakeHost::new()
                .reply(
                    &version,
                    Reply::ok("Client Version: v1.30.1\nKustomize Version: v5\n"),
                )
                .reply(&get, Reply::ok("Running")),
        );
        let probes = KubeEndpoints::new(ok, options()).checks();
        let outcomes: Vec<_> = probes
            .iter()
            .map(|p| (p.name.as_str(), p.outcome.clone()))
            .collect();
        assert_eq!(
            outcomes,
            vec![
                ("kubectl", Ok("Client Version: v1.30.1".to_string())),
                ("pod", Ok("dev-0 is Running".to_string())),
            ]
        );

        let pending = Arc::new(
            FakeHost::new()
                .reply(&version, Reply::ok("Client Version: v1"))
                .reply(&get, Reply::ok("Pending")),
        );
        let probes = KubeEndpoints::new(pending, options()).checks();
        assert!(probes[1].outcome.as_ref().unwrap_err().contains("Pending"));

        // No kubectl: the pod is not asked about at all.
        let host = Arc::new(FakeHost::new().reply(&version, Reply::fail(127, "not found")));
        let probes = KubeEndpoints::new(host.clone(), options()).checks();
        assert_eq!(probes.len(), 1);
        assert!(probes[0].outcome.is_err());
        assert_eq!(host.argvs().len(), 1);

        // Nothing set: said as a failed check, not a crash.
        let host = Arc::new(FakeHost::new().reply(&version, Reply::ok("Client Version: v1")));
        let probes = KubeEndpoints::new(host, KubeOptions::default()).checks();
        assert!(
            probes[1]
                .outcome
                .as_ref()
                .unwrap_err()
                .contains("no pod to work in")
        );
    }
}
