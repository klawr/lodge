//! A [`Host`] for tests: it runs nothing. Every job is recorded, and answered from
//! a script (a success with no output when nothing matches); files live in
//! memory. What the real hosts do with processes and the file system is tested
//! with them; everything that only *uses* a host is tested with this.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Mutex;

use super::{Child, FirstFile, Host, Input, Job, Line, Ran, RetryPolicy, ScratchFile, Stream};

/// What a matching job answers.
#[derive(Clone, Debug)]
pub struct Reply {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Reply {
    pub fn ok(stdout: &str) -> Self {
        Reply {
            code: 0,
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
        }
    }

    pub fn fail(code: i32, stderr: &str) -> Self {
        Reply {
            code,
            stdout: vec![],
            stderr: stderr.as_bytes().to_vec(),
        }
    }
}

/// One job the host was asked to run.
#[derive(Clone, Debug)]
pub struct Call {
    /// The words of the command; a script is `["sh", "-c", script]`.
    pub argv: Vec<String>,
    pub dir: Option<PathBuf>,
    /// What the job was fed on stdin.
    pub fed: Vec<u8>,
}

#[derive(Default)]
struct State {
    calls: Vec<Call>,
    /// Prefix, answer, and whether the answer is used up by one job.
    replies: Vec<(Vec<String>, Reply, bool)>,
    files: BTreeMap<PathBuf, Vec<u8>>,
    scratch: u32,
}

/// A host that records and scripts instead of running.
pub struct FakeHost {
    label: Option<String>,
    tunnels: bool,
    state: Mutex<State>,
}

impl FakeHost {
    /// This machine's stand-in.
    pub fn new() -> Self {
        FakeHost {
            label: None,
            tunnels: false,
            state: Mutex::default(),
        }
    }

    /// Another machine, called `label`.
    pub fn remote(label: &str) -> Self {
        FakeHost {
            label: Some(label.to_string()),
            ..Self::new()
        }
    }

    /// Provides tunnels, as a host whose network the caller cannot reach does.
    pub fn with_tunnels(mut self) -> Self {
        self.tunnels = true;
        self
    }

    /// A job whose words start with `prefix` prints `stdout`.
    pub fn on(self, prefix: &[&str], stdout: &str) -> Self {
        self.reply(prefix, Reply::ok(stdout))
    }

    /// A job whose words start with `prefix` answers `reply`. The first match
    /// wins.
    pub fn reply(self, prefix: &[&str], reply: Reply) -> Self {
        self.push_reply(prefix, reply, false)
    }

    /// Like [`reply`](Self::reply), for the first matching job only; the ones
    /// after it fall through to what comes next. A connection that drops once is
    /// `reply_once(…, Reply::fail(255, …))`.
    pub fn reply_once(self, prefix: &[&str], reply: Reply) -> Self {
        self.push_reply(prefix, reply, true)
    }

    fn push_reply(self, prefix: &[&str], reply: Reply, once: bool) -> Self {
        self.state.lock().unwrap().replies.push((
            prefix.iter().map(|w| w.to_string()).collect(),
            reply,
            once,
        ));
        self
    }

    /// A file on the host.
    pub fn with_file(self, path: &str, contents: &str) -> Self {
        self.state
            .lock()
            .unwrap()
            .files
            .insert(PathBuf::from(path), contents.as_bytes().to_vec());
        self
    }

    /// Puts (or replaces) a file on the host, for a test that edits one.
    pub fn put_file(&self, path: &str, contents: &str) {
        self.state
            .lock()
            .unwrap()
            .files
            .insert(PathBuf::from(path), contents.as_bytes().to_vec());
    }

    /// Every job run so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    /// The words of every job run so far.
    pub fn argvs(&self) -> Vec<Vec<String>> {
        self.calls().into_iter().map(|c| c.argv).collect()
    }

    fn answer(&self, argv: &[String]) -> Reply {
        let mut state = self.state.lock().unwrap();
        let Some(i) = state
            .replies
            .iter()
            .position(|(prefix, ..)| argv.starts_with(prefix))
        else {
            return Reply::ok("");
        };
        let (_, reply, once) = state.replies[i].clone();
        if once {
            state.replies.remove(i);
        }
        reply
    }
}

impl Default for FakeHost {
    fn default() -> Self {
        Self::new()
    }
}

fn argv_of(line: &Line) -> Vec<String> {
    match line {
        Line::Shell(script) => vec!["sh".into(), "-c".into(), script.clone()],
        Line::Argv(argv) => argv.clone(),
    }
}

fn status(code: i32) -> ExitStatus {
    ExitStatus::from_raw(code << 8)
}

struct FakeChild {
    stopped: bool,
}

impl Child for FakeChild {
    fn kill(&mut self) -> std::io::Result<()> {
        self.stopped = true;
        Ok(())
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        Ok(self.stopped.then(|| status(0)))
    }
}

impl Host for FakeHost {
    fn label(&self) -> Option<String> {
        self.label.clone()
    }

    fn names(&self) -> Vec<String> {
        vec![self.label.clone().unwrap_or_else(|| "local".into())]
    }

    fn shares_cwd(&self) -> bool {
        self.label.is_none()
    }

    fn run(&self, job: &mut Job) -> std::io::Result<Ran> {
        let argv = argv_of(&job.line);
        let mut fed = Vec::new();
        let wrote = match &mut job.stdin {
            Input::Feed(feed) => feed(&mut fed),
            _ => Ok(()),
        };
        self.state.lock().unwrap().calls.push(Call {
            argv: argv.clone(),
            dir: job.dir.map(Path::to_path_buf),
            fed,
        });
        let reply = self.answer(&argv);
        let captured = |stream: Stream, bytes: Vec<u8>| {
            if stream == Stream::Capture {
                bytes
            } else {
                vec![]
            }
        };
        Ok(Ran {
            status: status(reply.code),
            stdout: captured(job.stdout, reply.stdout),
            stderr: captured(job.stderr, reply.stderr),
            fed: wrote,
        })
    }

    fn interactive(&self, argv: &[&str]) -> std::io::Result<ExitStatus> {
        let argv: Vec<String> = std::iter::once("interactive".to_string())
            .chain(argv.iter().map(|a| a.to_string()))
            .collect();
        self.state.lock().unwrap().calls.push(Call {
            argv: argv.clone(),
            dir: None,
            fed: vec![],
        });
        Ok(status(self.answer(&argv).code))
    }

    fn tunnel(
        &self,
        local: u16,
        to: &str,
        _chatter: Stream,
    ) -> Option<std::io::Result<Box<dyn Child>>> {
        if !self.tunnels {
            return None;
        }
        self.state.lock().unwrap().calls.push(Call {
            argv: vec!["tunnel".into(), local.to_string(), to.to_string()],
            dir: None,
            fed: vec![],
        });
        Some(Ok(Box::new(FakeChild { stopped: false })))
    }

    fn write_scratch(&self, name: &str, content: &str) -> Result<ScratchFile<'_>> {
        let path = {
            let mut state = self.state.lock().unwrap();
            state.scratch += 1;
            let path = format!("/fake-scratch/{}/{name}", state.scratch);
            state
                .files
                .insert(PathBuf::from(&path), content.as_bytes().to_vec());
            path
        };
        let gone = path.clone();
        Ok(ScratchFile::new(path, move || {
            self.state.lock().unwrap().files.remove(Path::new(&gone));
        }))
    }

    fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        Ok(self.state.lock().unwrap().files.get(path).cloned())
    }

    fn canonical_dir(&self, dir: &Path) -> Result<PathBuf> {
        let state = self.state.lock().unwrap();
        if state.files.keys().any(|f| f.starts_with(dir)) {
            Ok(dir.to_path_buf())
        } else {
            anyhow::bail!("project folder {} does not exist", dir.display())
        }
    }

    fn read_first(&self, patterns: &[PathBuf]) -> Result<FirstFile> {
        let state = self.state.lock().unwrap();
        let mut paths = Vec::new();
        for pattern in patterns {
            let mut found: Vec<&PathBuf> = state
                .files
                .keys()
                .filter(|f| matches_pattern(pattern, f))
                .collect();
            found.sort();
            paths.extend(found.into_iter().cloned());
        }
        let contents = paths.first().and_then(|p| state.files.get(p).cloned());
        Ok(FirstFile { paths, contents })
    }

    fn fetch_dir(&self, dir: &Path, dest: &Path) -> Result<PathBuf> {
        let files: Vec<(PathBuf, Vec<u8>)> = {
            let state = self.state.lock().unwrap();
            state
                .files
                .iter()
                .filter_map(|(path, bytes)| {
                    Some((path.strip_prefix(dir).ok()?.to_path_buf(), bytes.clone()))
                })
                .collect()
        };
        for (relative, bytes) in files {
            let to = dest.join(relative);
            std::fs::create_dir_all(to.parent().context("a file has a folder")?)?;
            std::fs::write(to, bytes)?;
        }
        std::fs::create_dir_all(dest)?;
        Ok(dest.to_path_buf())
    }

    fn uid_gid(&self) -> Result<(u32, u32)> {
        Ok((1000, 1000))
    }

    /// A remote fake loses its connection like ssh does: exit 255, and the step
    /// is tried again at once, three times in all.
    fn link_lost(&self, status: ExitStatus, _stderr: &[u8]) -> bool {
        self.label.is_some() && status.code() == Some(255)
    }

    fn retry_policy(&self) -> RetryPolicy {
        if self.label.is_some() {
            RetryPolicy {
                tries: 3,
                first_delay: std::time::Duration::ZERO,
            }
        } else {
            RetryPolicy::NEVER
        }
    }
}

/// Whether `file` matches `pattern`, a path whose components that are exactly `*`
/// match any one name (see [`Host::read_first`]).
fn matches_pattern(pattern: &Path, file: &Path) -> bool {
    let (mut p, mut f) = (pattern.components(), file.components());
    loop {
        match (p.next(), f.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if a.as_os_str() == "*" || a == b => {}
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Repeat;

    #[test]
    fn jobs_are_recorded_and_answered_from_the_script() {
        let host = FakeHost::new()
            .on(&["docker", "ps"], "abc\n")
            .reply(&["docker", "rm"], Reply::fail(1, "no such"));
        let ps = host
            .exec(Job::argv(&["docker", "ps", "-a"]).capture())
            .unwrap();
        assert_eq!(ps.stdout_text(), "abc\n");
        let rm = host
            .exec(Job::argv(&["docker", "rm", "x"]).capture())
            .unwrap();
        assert_eq!(
            (rm.status.code(), rm.stderr_text().as_str()),
            (Some(1), "no such")
        );
        // Nothing matches: a quiet success.
        let other = host.exec(Job::shell("true").repeat(Repeat::Safe)).unwrap();
        assert!(other.status.success());
        assert_eq!(host.argvs()[2], ["sh", "-c", "true"]);
    }

    #[test]
    fn what_a_job_is_fed_is_recorded_and_an_uncaptured_stream_stays_empty() {
        let host = FakeHost::new().on(&["cat"], "echoed");
        let mut feed = |w: &mut dyn std::io::Write| w.write_all(b"in");
        let ran = host
            .exec(Job::argv(&["cat"]).stdin(Input::Feed(&mut feed)))
            .unwrap();
        assert!(ran.stdout.is_empty());
        assert_eq!(host.calls()[0].fed, b"in");
    }

    #[test]
    fn files_are_in_memory() {
        let host = FakeHost::new()
            .with_file("/p/.devcontainer/devcontainer.json", "{}")
            .with_file("/p/.devcontainer/Dockerfile", "FROM a");
        assert_eq!(
            host.read_file(Path::new("/p/.devcontainer/Dockerfile"))
                .unwrap(),
            Some(b"FROM a".to_vec())
        );
        assert_eq!(host.read_file(Path::new("/nope")).unwrap(), None);
        let first = host
            .read_first(&[PathBuf::from("/p/.devcontainer/*.json")])
            .unwrap();
        assert!(first.paths.is_empty(), "only a whole component is a star");
        let first = host
            .read_first(&[PathBuf::from("/p/*/devcontainer.json")])
            .unwrap();
        assert_eq!(first.contents, Some(b"{}".to_vec()));
        let tmp = crate::tmpfile::TempDir::new("fake-fetch").unwrap();
        let dir = host
            .fetch_dir(Path::new("/p/.devcontainer"), &tmp.path().join("d"))
            .unwrap();
        assert_eq!(std::fs::read(dir.join("Dockerfile")).unwrap(), b"FROM a");
    }
}
