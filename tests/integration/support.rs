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

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The result of one `ldg` (or docker) invocation.
pub struct Out {
    pub ok: bool,
    pub text: String,
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
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Out {
        ok: out.status.success(),
        text,
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

/// How many `ldg _forward` daemons are running for `container`.
pub fn forward_daemons(container: &str) -> usize {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return 0;
    };
    dir.filter_map(|e| std::fs::read(e.ok()?.path().join("cmdline")).ok())
        .filter(|raw| {
            let args: Vec<&[u8]> = raw.split(|&b| b == 0).collect();
            args.contains(&&b"_forward"[..]) && args.contains(&container.as_bytes())
        })
        .count()
}

/// Private config and data directories, and an optional fake `ssh`.
pub struct Sandbox {
    root: PathBuf,
    fake_ssh: bool,
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
        let sb = Self {
            root,
            fake_ssh: false,
        };
        sb.config("");
        sb
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Writes the user's lodge config. A session section that avoids tmux is
    /// always included.
    pub fn config(&self, extra_toml: &str) {
        let toml = format!("{extra_toml}\n[session]\nmultiplexer = \"none\"\n");
        std::fs::write(self.root.join("config/lodge/config.toml"), toml).expect("writing config");
    }

    /// Puts an `ssh` on `PATH` that skips its options and runs the remote command
    /// locally, the way a remote login shell would receive it.
    pub fn use_fake_ssh(&mut self) {
        let bin = self.root.join("bin");
        std::fs::create_dir_all(&bin).expect("creating bin");
        let script = "#!/bin/bash\n\
            while [ $# -gt 0 ]; do case \"$1\" in -o|-p|-L|-S|-i|-l) shift 2 ;; -*) shift ;; *) break ;; esac; done\n\
            shift\n\
            exec sh -c \"$*\"\n";
        let path = bin.join("ssh");
        std::fs::write(&path, script).expect("writing fake ssh");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        self.fake_ssh = true;
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ldg"));
        cmd.env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            // Scratch files and ssh control sockets land here, not in /tmp, and go
            // away with the sandbox.
            .env("TMPDIR", self.root.join("tmp"));
        if self.fake_ssh {
            let mut paths = vec![self.root.join("bin")];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            cmd.env("PATH", std::env::join_paths(paths).expect("joining PATH"));
        }
        cmd
    }

    /// Runs `ldg <args>`.
    pub fn run(&self, args: &[&str]) -> Out {
        let mut cmd = self.command();
        cmd.args(args);
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
            remote: false,
            last: RefCell::new(String::new()),
        };
        p.write(".devcontainer/devcontainer.json", devcontainer_json);
        p
    }
}

impl Sandbox {
    /// Whether any `ldg _forward` daemon started inside this sandbox is running.
    fn has_running_daemons(&self) -> bool {
        let marker = format!("XDG_DATA_HOME={}", self.root.join("data").display());
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return false;
        };
        dir.filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                std::fs::read(p.join("cmdline"))
                    .map(|c| c.split(|&b| b == 0).any(|a| a == b"_forward"))
                    .unwrap_or(false)
            })
            .any(|p| {
                std::fs::read(p.join("environ"))
                    .map(|e| e.split(|&b| b == 0).any(|v| v == marker.as_bytes()))
                    .unwrap_or(false)
            })
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
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A project directory, and the container ldg makes for it.
pub struct Project<'a> {
    sb: &'a Sandbox,
    dir: PathBuf,
    base: String,
    remote: bool,
    last: RefCell<String>,
}

impl Project<'_> {
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Operate through the fake ssh (`--remote fakehost`) from now on.
    pub fn remote(mut self) -> Self {
        self.remote = true;
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
        if self.remote {
            args.extend(["--remote", "fakehost"]);
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
