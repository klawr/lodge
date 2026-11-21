//! Helpers for driving the real `ldg` binary against a real container engine.
//!
//! Each test gets a [`Sandbox`]: private config, data and temp directories (so
//! nothing touches the user's own `~/.config/lodge` or `/tmp`), and optionally a fake `ssh` that
//! runs the remote command locally, to exercise `--remote` without a second
//! machine. A [`Project`] removes its container when dropped, even if the test
//! failed.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

pub const IMAGE: &str = "debian:stable-slim";

/// Where lodge installs for root, whose home is `/root`: the user's prefix.
pub const ROOT_PREFIX: &str = "/root/.local/share/lodge";

/// A tool injected for root, by its path in the prefix.
pub fn root_tool(name: &str) -> String {
    format!("{ROOT_PREFIX}/bin/{name}")
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The result of one `ldg` (or docker) invocation.
pub struct Out {
    pub ok: bool,
    /// stdout followed by stderr.
    pub text: String,
    /// stdout alone (for `--json`).
    pub stdout: String,
}

impl Out {
    pub fn has(&self, needle: &str) -> bool {
        self.text.contains(needle)
    }

    /// The last `n` lines, for a short failure message.
    pub fn tail(&self, n: usize) -> String {
        let lines: Vec<&str> = self.text.lines().collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }
}

/// Runs a command and captures stdout and stderr together.
fn capture(mut cmd: Command) -> Out {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .expect("spawning a command");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut text = stdout.clone();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Out {
        ok: out.status.success(),
        text,
        stdout,
    }
}

/// `docker <args>`, returning trimmed stdout; panics with the output on failure.
pub fn docker(args: &[&str]) -> String {
    let mut cmd = Command::new("docker");
    cmd.args(args);
    let out = cmd.stdin(Stdio::null()).output().expect("running docker");
    assert!(
        out.status.success(),
        "docker {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A TCP port that is free right now.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("binding a port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// The uid of the user running the tests.
pub fn host_uid() -> u32 {
    let mut cmd = Command::new("id");
    cmd.arg("-u");
    let out = capture(cmd);
    out.text.trim().parse().expect("a numeric uid")
}

/// Fails with an install hint if a host tool ldg needs is missing.
pub fn require_tool(name: &str, hint: &str) {
    let found = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
        .unwrap_or(false);
    assert!(found, "these tests need `{name}` on PATH ({hint})");
}

/// The addresses (as `/proc/net/tcp` hex) that listen on `port`.
pub fn listeners(port: u16) -> Vec<String> {
    let text = std::fs::read_to_string("/proc/net/tcp").unwrap_or_default();
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            let (addr, p) = cols.get(1)?.split_once(':')?;
            let listening = *cols.get(3)? == "0A";
            (listening && u16::from_str_radix(p, 16).ok()? == port).then(|| addr.to_string())
        })
        .collect()
}

/// Polls `done` every 100 ms for up to `secs` seconds; whether it became true.
pub fn wait_until(secs: u64, mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..secs * 10 {
        if done() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    done()
}

/// The full id of `container` (a name or id).
pub fn container_id(container: &str) -> String {
    let out = Command::new("docker")
        .args(["inspect", "--format", "{{.Id}}", container])
        .output()
        .expect("docker inspect");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// How many `ldg _forward` daemons are running for `container` (by name or
/// by any prefix of its id, as the daemon may be given either). For a removed
/// container, pass its full id.
pub fn forward_daemons(container: &str) -> usize {
    forward_daemon_pids(container).len()
}

/// The pids of the processes whose command line (its arguments, split apart)
/// `matches`.
fn processes_with(matches: impl Fn(&[&[u8]]) -> bool) -> Vec<u32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return vec![];
    };
    dir.filter_map(|e| {
        let path = e.ok()?.path();
        let pid = path.file_name()?.to_str()?.parse().ok()?;
        let raw = std::fs::read(path.join("cmdline")).ok()?;
        let args: Vec<&[u8]> = raw.split(|&b| b == 0).collect();
        matches(&args).then_some(pid)
    })
    .collect()
}

fn forward_daemon_pids(container: &str) -> Vec<u32> {
    let id = match container_id(container) {
        id if id.is_empty() => container.to_string(),
        id => id,
    };
    processes_with(|args| {
        let names_it = |a: &&[u8]| {
            *a == container.as_bytes() || (a.len() >= 12 && id.as_bytes().starts_with(a))
        };
        args.contains(&&b"_forward"[..]) && args.iter().any(names_it)
    })
}

/// Stops the forward daemons of `container` and the tunnels they started (each
/// daemon leads its own process group).
pub fn stop_forward_daemons(container: &str) {
    for pid in forward_daemon_pids(container) {
        kill_group(pid);
    }
}

fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-TERM", "--", &format!("-{pid}")])
        .output();
}

/// A forward daemon the test started itself, so as to give it settings `up`
/// does not; stopped, with its tunnels, when dropped.
pub struct Daemon {
    child: std::process::Child,
}

impl Daemon {
    pub fn running(&mut self) -> bool {
        self.child.try_wait().expect("asking the daemon").is_none()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        kill_group(self.child.id());
        let _ = self.child.wait();
    }
}

/// A real OCI registry (`registry:2`) on a loopback port, removed when dropped.
/// Bundles are published with [`Registry::push`], in the shape the docs describe:
/// one layer holding the tar, which is what `oras push` makes of a file.
pub struct Registry {
    container: String,
    pub port: u16,
}

impl Registry {
    pub fn start() -> Self {
        let port = free_port();
        let container = docker(&[
            "run",
            "-d",
            "--rm",
            "-p",
            &format!("127.0.0.1:{port}:5000"),
            "registry:2",
        ]);
        let registry = Self { container, port };
        assert!(
            wait_until(20, || std::net::TcpStream::connect(("127.0.0.1", port))
                .is_ok()),
            "the registry did not come up"
        );
        registry
    }

    /// `oci://` reference to `name:tag` on this registry.
    pub fn reference(&self, name: &str, tag: &str) -> String {
        format!("oci://localhost:{}/{name}:{tag}", self.port)
    }

    /// What the registry holds for `name:tag`, as its own HTTP API says it: the
    /// manifest's text, read by a client that is not lodge.
    pub fn manifest(&self, name: &str, tag: &str) -> String {
        const SCRIPT: &str = r#"
import sys, urllib.request
req = urllib.request.Request(
    f"http://127.0.0.1:{sys.argv[1]}/v2/{sys.argv[2]}/manifests/{sys.argv[3]}",
    headers={"Accept": "application/vnd.oci.image.manifest.v1+json"})
sys.stdout.write(urllib.request.urlopen(req).read().decode())
"#;
        let mut cmd = Command::new("python3");
        cmd.args(["-c", SCRIPT, &self.port.to_string(), name, tag]);
        let out = capture(cmd);
        assert!(out.ok, "reading the manifest failed:\n{}", out.text);
        out.text
    }

    /// Publishes `file` as `name:tag`: an empty config and the file as the one
    /// layer. Plain HTTP, since this registry is on loopback.
    pub fn push(&self, name: &str, tag: &str, file: &Path) {
        const SCRIPT: &str = r#"
import hashlib, json, sys, urllib.request
base, repo, tag, path = sys.argv[1:5]
def blob(data):
    digest = "sha256:" + hashlib.sha256(data).hexdigest()
    started = urllib.request.urlopen(
        urllib.request.Request(f"{base}/v2/{repo}/blobs/uploads/", method="POST"))
    where = started.headers["Location"]
    if where.startswith("/"):
        where = base + where
    sep = "&" if "?" in where else "?"
    urllib.request.urlopen(urllib.request.Request(
        f"{where}{sep}digest={digest}", data=data, method="PUT",
        headers={"Content-Type": "application/octet-stream"}))
    return digest, len(data)
config = blob(b"{}")
layer = blob(open(path, "rb").read())
manifest = {
    "schemaVersion": 2,
    "mediaType": "application/vnd.oci.image.manifest.v1+json",
    "config": {"mediaType": "application/vnd.oci.empty.v1+json",
               "digest": config[0], "size": config[1]},
    "layers": [{"mediaType": "application/vnd.lodge.bundle.v1.tar",
                "digest": layer[0], "size": layer[1]}],
}
urllib.request.urlopen(urllib.request.Request(
    f"{base}/v2/{repo}/manifests/{tag}", data=json.dumps(manifest).encode(), method="PUT",
    headers={"Content-Type": manifest["mediaType"]}))
"#;
        let base = format!("http://127.0.0.1:{}", self.port);
        let mut cmd = Command::new("python3");
        cmd.args(["-c", SCRIPT, &base, name, tag]).arg(file);
        let out = capture(cmd);
        assert!(out.ok, "pushing to the registry failed:\n{}", out.text);
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// A real sshd container; removed when dropped.
pub struct SshHost {
    container: String,
    /// The server's `authorized_keys`, a file the test owns and can empty.
    authorized_keys: PathBuf,
    public_key: String,
}

impl SshHost {
    /// The server stops accepting the test's key, as a host that needs a password
    /// does for a connection that cannot ask for one.
    pub fn require_login(&self) {
        std::fs::write(&self.authorized_keys, "").expect("emptying authorized_keys");
    }

    /// Lets the key in again when the returned guard is dropped, so that what a
    /// test cleans up over this connection afterwards still can, even if the test
    /// failed while the key was refused. Declare it after the project.
    pub fn accept_login_when_done(&self) -> impl Drop + '_ {
        struct Guard<'a>(&'a SshHost);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.accept_login();
            }
        }
        Guard(self)
    }

    /// The server accepts the key again: the login was given.
    pub fn accept_login(&self) {
        std::fs::write(&self.authorized_keys, &self.public_key).expect("restoring authorized_keys");
    }

    /// The `--remote` destination.
    pub fn dest(&self) -> &'static str {
        "lodge-it-sshd"
    }
}

impl Drop for SshHost {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container])
            .output();
    }
}

/// Private config and data directories, and an optional fake `ssh`.
pub struct Sandbox {
    root: PathBuf,
    bin_on_path: bool,
}

impl Sandbox {
    pub fn new() -> Self {
        let status = Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        assert!(
            status.map(|s| s.success()).unwrap_or(false),
            "these tests need a running docker daemon"
        );
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("ldg-it-{}-{n}", std::process::id()));
        std::fs::create_dir_all(root.join("config/lodge")).expect("creating sandbox");
        std::fs::create_dir_all(root.join("data")).expect("creating sandbox");
        std::fs::create_dir_all(root.join("tmp")).expect("creating sandbox");
        std::fs::create_dir_all(root.join("cache")).expect("creating sandbox");
        let sb = Self {
            root,
            bin_on_path: false,
        };
        sb.config("");
        sb
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Writes the user's lodge config.
    pub fn config(&self, extra_toml: &str) {
        self.config_raw(extra_toml);
    }

    /// Writes the user's lodge config exactly as given.
    pub fn config_raw(&self, toml: &str) {
        std::fs::write(self.root.join("config/lodge/config.toml"), toml).expect("writing config");
    }

    /// Puts an `ssh` on `PATH` that skips its options and runs the remote command
    /// locally, the way a remote login shell would receive it.
    pub fn use_fake_ssh(&mut self) {
        let bin = self.root.join("bin");
        std::fs::create_dir_all(&bin).expect("creating bin");
        let script = "#!/bin/bash\n\
            while [ $# -gt 0 ]; do case \"$1\" in -o|-p|-L|-S|-i|-l|-E) shift 2 ;; -*) shift ;; *) break ;; esac; done\n\
            shift\n\
            exec sh -c \"$*\"\n";
        let path = bin.join("ssh");
        std::fs::write(&path, script).expect("writing fake ssh");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        self.bin_on_path = true;
    }

    /// Puts a `kubectl` on `PATH` for a cluster that is a docker engine: a pod is
    /// a running container of that name, `exec` is `docker exec`, and
    /// `port-forward` is a small relay to the container's address. Every call is
    /// appended to `kubectl.log` in the sandbox, so a test can see which flags
    /// lodge passed. Needs `python3` for the relay.
    pub fn use_fake_kubectl(&mut self) {
        let bin = self.root.join("bin");
        std::fs::create_dir_all(&bin).expect("creating bin");
        let script = include_str!("fake-kubectl.sh");
        let path = bin.join("kubectl");
        std::fs::write(&path, script).expect("writing fake kubectl");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        self.bin_on_path = true;
    }

    /// What the fake `kubectl` was called with, a line per call.
    pub fn kubectl_calls(&self) -> String {
        std::fs::read_to_string(self.root.join("bin/kubectl.log")).unwrap_or_default()
    }

    /// Starts a real OpenSSH server (a container with the docker CLI, talking to
    /// this engine through its socket) and puts an `ssh` on `PATH` that is the
    /// real client plus a private config. Returns the `--remote` destination.
    /// The sandbox is mounted at the same path in the server, so project paths
    /// mean the same on both sides, as on a real remote host.
    pub fn use_real_sshd(&mut self) -> SshHost {
        let real_ssh = which("ssh").expect("these tests need an ssh client");
        let bin = self.root.join("bin");
        std::fs::create_dir_all(&bin).expect("creating bin");
        let key = self.root.join("id_ed25519");
        let keygen = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .expect("running ssh-keygen");
        assert!(keygen.success(), "ssh-keygen failed");

        let image = "lodge-it-sshd";
        let dockerfile = concat!(
            "FROM docker:27-cli\n",
            "RUN apk add --no-cache --upgrade openssh-server openssh-client && ssh-keygen -A",
            " && sed -i 's/^#*PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config",
            // authorized_keys is bind-mounted from the sandbox, owned by the test's uid.
            " && echo 'StrictModes no' >> /etc/ssh/sshd_config",
            // The test refuses logins on purpose; the server must not answer by
            // refusing the machine, as a default sshd does.
            " && echo 'PerSourcePenalties no' >> /etc/ssh/sshd_config\n",
        );
        let mut build = Command::new("docker")
            .args(["build", "-q", "-t", image, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("docker build");
        {
            use std::io::Write as _;
            build
                .stdin
                .take()
                .expect("stdin")
                .write_all(dockerfile.as_bytes())
                .expect("writing the Dockerfile");
        }
        assert!(
            build.wait().expect("docker build").success(),
            "building the sshd image failed"
        );

        let port = free_port();
        let root = self.root.to_string_lossy().into_owned();
        let public_key =
            std::fs::read_to_string(format!("{}.pub", key.display())).expect("reading the key");
        // Bind-mounted as a file: the test rewrites it in place to let the key in
        // or out.
        let authorized_keys = self.root.join("authorized_keys");
        std::fs::write(&authorized_keys, &public_key).expect("writing authorized_keys");
        let name = format!(
            "lodge-it-sshd-{}",
            self.root.file_name().expect("a name").to_string_lossy()
        );
        docker(&[
            "run",
            "-d",
            "--name",
            &name,
            "-p",
            &format!("127.0.0.1:{port}:22"),
            "-v",
            "/var/run/docker.sock:/var/run/docker.sock",
            "-v",
            &format!("{root}:{root}"),
            "-v",
            &format!(
                "{}:/root/.ssh/authorized_keys:ro",
                authorized_keys.display()
            ),
            image,
            "/usr/sbin/sshd",
            "-D",
            "-e",
        ]);
        // From here on the container is removed even if the test panics.
        let host = SshHost {
            container: name,
            authorized_keys,
            public_key,
        };

        let config = self.root.join("ssh_config");
        std::fs::write(
            &config,
            format!(
                "Host lodge-it-sshd\n  HostName 127.0.0.1\n  Port {port}\n  User root\n  \
                 IdentityFile {}\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  \
                 UserKnownHostsFile /dev/null\n  LogLevel ERROR\n",
                key.display()
            ),
        )
        .expect("writing ssh_config");
        let wrapper = bin.join("ssh");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec {} -F {} \"$@\"\n",
                real_ssh.display(),
                config.display()
            ),
        )
        .expect("writing the ssh wrapper");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        self.bin_on_path = true;

        // Wait until sshd answers.
        for _ in 0..50 {
            let ok = Command::new(&wrapper)
                .args(["-o", "BatchMode=yes", "lodge-it-sshd", "true"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                return host;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        panic!("sshd in {} never answered", host.container);
    }

    /// Runs `ldg --remote <remote> _forward <container> --port <port>
    /// --unreachable-polls <polls>` in the background, its log in `log`.
    pub fn spawn_daemon(
        &self,
        remote: &str,
        container: &str,
        port: u16,
        polls: u32,
        log: &Path,
    ) -> Daemon {
        use std::os::unix::process::CommandExt as _;
        let log = std::fs::File::create(log).expect("creating the daemon log");
        let child = self
            .command()
            .args(["--remote", remote, "_forward", container])
            .args(["--port", &format!("{port}:{port}")])
            .args(["--unreachable-polls", &polls.to_string()])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("starting the daemon");
        Daemon { child }
    }

    /// Ends the shared ssh connection to the test's server, as a link that is lost
    /// for good does: its tunnels end with it, and a new one needs a login.
    pub fn drop_ssh_master(&self) {
        let sockets = self.root.join("tmp/lodge-ssh");
        let sockets = sockets.to_string_lossy().into_owned();
        for pid in processes_with(|args| {
            args.iter().any(|a| {
                let a = String::from_utf8_lossy(a);
                a.contains("[mux]") && a.contains(&sockets)
            })
        }) {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .output();
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ldg"));
        cmd.env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            // Scratch files and ssh control sockets land here, not in /tmp, and go
            // away with the sandbox.
            .env("TMPDIR", self.root.join("tmp"));
        if self.bin_on_path {
            let mut paths = vec![self.root.join("bin")];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            cmd.env("PATH", std::env::join_paths(paths).expect("joining PATH"));
        }
        cmd
    }

    /// Runs `ldg <args>` on a pseudo-terminal, as a person would, feeding it
    /// `input` (which should end with `exit` if a session opens); the whole run is
    /// cut off after 90 s. Output includes what the session printed, with `\r\n`
    /// line ends.
    pub fn session(&self, args: &[&str], host_env: &[(&str, &str)], input: &str) -> Out {
        use std::io::Write as _;
        fn quote(s: &str) -> String {
            format!("'{}'", s.replace('\'', "'\\''"))
        }
        let ldg = self.command();
        let mut line = format!(
            "exec timeout 90 {}",
            quote(&ldg.get_program().to_string_lossy())
        );
        for a in args {
            line.push(' ');
            line.push_str(&quote(a));
        }
        let mut cmd = Command::new("script");
        cmd.args(["-qec", &line, "/dev/null"]);
        for (k, v) in ldg.get_envs() {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        cmd.envs(host_env.iter().copied());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawning script");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("feeding the session");
        let out = child.wait_with_output().expect("waiting for script");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut text = stdout.clone();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Out {
            ok: out.status.success(),
            text,
            stdout,
        }
    }

    /// Runs `ldg <args>`.
    pub fn run(&self, args: &[&str]) -> Out {
        let mut cmd = self.command();
        cmd.args(args);
        capture(cmd)
    }

    /// Like [`run`](Self::run), but gives up after `secs` seconds (the output is
    /// then a failure), for a command that would otherwise hang the test.
    pub fn run_within(&self, secs: u64, args: &[&str]) -> Out {
        let ldg = self.command();
        let mut cmd = Command::new("timeout");
        cmd.arg(secs.to_string()).arg(ldg.get_program());
        for (k, v) in ldg.get_envs() {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        cmd.args(args);
        capture(cmd)
    }

    /// Runs `ldg <args>` as a user whose home is `home`: where a plain host, which
    /// is the machine itself, keeps lodge's prefix and the dotfiles it must not
    /// touch.
    pub fn run_with_home(&self, home: &Path, args: &[&str]) -> Out {
        let mut cmd = self.command();
        cmd.env("HOME", home).args(args);
        capture(cmd)
    }

    /// A directory containing only the named tools (symlinks to the real ones),
    /// for running `ldg` with a `PATH` that lacks everything else.
    pub fn restricted_path(&self, tools: &[&str]) -> PathBuf {
        let dir = self.root.join("minpath");
        std::fs::create_dir_all(&dir).expect("creating minpath");
        let real = std::env::var_os("PATH").unwrap_or_default();
        for tool in tools {
            let found = std::env::split_paths(&real)
                .map(|d| d.join(tool))
                .find(|p| p.is_file())
                .unwrap_or_else(|| panic!("`{tool}` is not on PATH"));
            let _ = std::os::unix::fs::symlink(found, dir.join(tool));
        }
        dir
    }

    /// Runs `ldg <args>` with `PATH` set to `path` alone.
    pub fn run_with_path(&self, path: &Path, args: &[&str]) -> Out {
        let mut cmd = self.command();
        cmd.env("PATH", path).args(args);
        capture(cmd)
    }

    /// A project directory with the given `.devcontainer/devcontainer.json`.
    pub fn project(&self, name: &str, devcontainer_json: &str) -> Project<'_> {
        let dir = self.root.join(format!("work/{name}"));
        std::fs::create_dir_all(dir.join(".devcontainer")).expect("creating project");
        let p = Project {
            sb: self,
            dir,
            base: name.to_string(),
            remote: None,
            last: RefCell::new(String::new()),
        };
        p.write(".devcontainer/devcontainer.json", devcontainer_json);
        p
    }
}

impl Sandbox {
    /// Whether any `ldg _forward` daemon started inside this sandbox is running.
    /// Whether a forward daemon that belongs to this sandbox is running.
    pub fn has_running_daemons(&self) -> bool {
        !self.daemon_pids().is_empty()
    }

    /// The forward daemons that belong to this sandbox (they have its data
    /// directory in their environment).
    fn daemon_pids(&self) -> Vec<u32> {
        let marker = format!("XDG_DATA_HOME={}", self.root.join("data").display());
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return vec![];
        };
        dir.filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                std::fs::read(p.join("cmdline"))
                    .map(|c| c.split(|&b| b == 0).any(|a| a == b"_forward"))
                    .unwrap_or(false)
            })
            .filter(|p| {
                std::fs::read(p.join("environ"))
                    .map(|e| e.split(|&b| b == 0).any(|v| v == marker.as_bytes()))
                    .unwrap_or(false)
            })
            .filter_map(|p| p.file_name()?.to_str()?.parse().ok())
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // The forward daemon outlives its container by up to a poll interval
        // (2 s), and keeps touching its log and ssh control directory until it
        // notices. Removing the sandbox under it would let it re-create parts of
        // it, so wait for it first.
        for _ in 0..80 {
            if !self.has_running_daemons() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // The connection's background master stays for ten minutes.
        self.drop_ssh_master();
        // One that is left has lost what it forwards for (its sshd is gone before
        // it noticed its container was) and would wait minutes to give up: no test
        // leaves a process behind.
        for pid in self.daemon_pids() {
            kill_group(pid);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A project directory, and the container ldg makes for it.
pub struct Project<'a> {
    sb: &'a Sandbox,
    dir: PathBuf,
    base: String,
    /// The `--remote` destination, if any.
    remote: Option<String>,
    last: RefCell<String>,
}

impl Project<'_> {
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Operate through the fake ssh (`--remote fakehost`) from now on.
    pub fn remote(self) -> Self {
        self.remote_via("fakehost")
    }

    /// Operate with `--remote <host>` from now on.
    pub fn remote_via(mut self, host: &str) -> Self {
        self.remote = Some(host.to_string());
        self
    }

    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.dir.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("creating dirs");
        std::fs::write(path, contents).expect("writing a project file");
    }

    /// `ldg [--remote fakehost] <subcommand> [extra…] <project>`.
    pub fn ldg(&self, subcommand: &str, extra: &[&str]) -> Out {
        let path = self.dir.to_string_lossy().into_owned();
        let mut args: Vec<&str> = Vec::new();
        if let Some(host) = &self.remote {
            args.extend(["--remote", host.as_str()]);
        }
        args.push(subcommand);
        args.extend(extra);
        args.push(&path);
        let out = self.sb.run(&args);
        *self.last.borrow_mut() = out.tail(15);
        out
    }

    /// `ldg up --no-attach`, which must succeed.
    pub fn up(&self) -> Out {
        let out = self.ldg("up", &["--no-attach"]);
        assert!(out.ok, "ldg up failed:\n{}", out.tail(15));
        out
    }

    /// `ldg [flags…] up <project>` *with* a session, on a pseudo-terminal made by
    /// `script`, feeding `input` to the shell it opens. `host_env` is added to
    /// ldg's environment (where a secret's source can live). The shell should
    /// end with `exit`; the whole run is cut off after 90 s. Output includes
    /// what the session printed, with `\r\n` line ends.
    pub fn attach(&self, flags: &[&str], host_env: &[(&str, &str)], input: &str) -> Out {
        let dir = self.dir.to_string_lossy().into_owned();
        let mut args = flags.to_vec();
        args.extend(["up", dir.as_str()]);
        let out = self.sb.session(&args, host_env, input);
        *self.last.borrow_mut() = out
            .text
            .lines()
            .rev()
            .take(15)
            .collect::<Vec<_>>()
            .join("\n");
        out
    }

    /// The name of this project's container (the first, for compose).
    pub fn container(&self) -> String {
        let filter = format!("name=^lodge-{}-", self.base);
        let names = docker(&["ps", "-a", "--filter", &filter, "--format", "{{.Names}}"]);
        names.lines().next().map(String::from).unwrap_or_else(|| {
            panic!(
                "no container for {}; last ldg output:\n{}",
                self.base,
                self.last.borrow()
            )
        })
    }

    /// Every container name belonging to this project.
    pub fn containers(&self) -> Vec<String> {
        let filter = format!("name=^lodge-{}-", self.base);
        docker(&["ps", "-a", "--filter", &filter, "--format", "{{.Names}}"])
            .lines()
            .map(String::from)
            .collect()
    }

    /// `docker exec <container> <argv…>`, returning trimmed stdout.
    pub fn exec(&self, argv: &[&str]) -> String {
        let container = self.container();
        let mut args = vec!["exec", container.as_str()];
        args.extend(argv);
        docker(&args)
    }

    /// `sh -c <script>` inside the container.
    pub fn sh(&self, script: &str) -> String {
        self.exec(&["sh", "-c", script])
    }
}

impl Drop for Project<'_> {
    fn drop(&mut self) {
        // Best effort, and it must run even when the test failed.
        let _ = self.ldg("rm", &[]);
        let images = Command::new("docker")
            .args(["images", "--format", "{{.Repository}}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let prefix = format!("lodge-{}-", self.base);
        for image in images
            .lines()
            .filter(|i| i.starts_with(&prefix) && i.ends_with("-img"))
        {
            let _ = Command::new("docker").args(["rmi", "-f", image]).output();
        }
    }
}
