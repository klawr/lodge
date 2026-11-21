//! The machine lodge runs on.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use super::{FirstFile, Host, Job, Line, Ran, ScratchFile, run_interactive, run_process};
use crate::tmpfile::TempFile;

/// This machine: commands run as child processes and files are read directly.
pub struct LocalHost;

/// The local process that runs `line`, in `dir` if given.
fn command(line: &Line, dir: Option<&Path>) -> Command {
    let mut c = match line {
        Line::Shell(script) => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(script);
            c
        }
        Line::Argv(argv) => match argv.split_first() {
            Some((program, args)) => {
                let mut c = Command::new(program);
                c.args(args);
                c
            }
            // Nothing to run: `false` is the closest honest answer, and the
            // caller sees a failed step like any other.
            None => Command::new("false"),
        },
    };
    if let Some(dir) = dir {
        c.current_dir(dir);
    }
    c
}

impl Host for LocalHost {
    fn label(&self) -> Option<String> {
        None
    }

    fn names(&self) -> Vec<String> {
        vec!["local".into()]
    }

    fn shares_cwd(&self) -> bool {
        true
    }

    fn run(&self, job: &mut Job) -> std::io::Result<Ran> {
        run_process(command(&job.line, job.dir), job)
    }

    fn interactive(&self, argv: &[&str]) -> std::io::Result<ExitStatus> {
        let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        run_interactive(command(&Line::Argv(argv), None))
    }

    fn write_scratch(&self, name: &str, content: &str) -> Result<ScratchFile<'_>> {
        let file =
            TempFile::write(name, content.as_bytes()).with_context(|| format!("writing {name}"))?;
        let path = file.path().to_string_lossy().into_owned();
        // The file removes itself when dropped.
        Ok(ScratchFile::new(path, move || drop(file)))
    }

    fn remove_file(&self, path: &str) {
        crate::tmpfile::discard_file(Path::new(path));
    }

    fn read_file(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn canonical_dir(&self, dir: &Path) -> Result<PathBuf> {
        std::fs::canonicalize(dir)
            .with_context(|| format!("project folder {} does not exist", dir.display()))
    }

    fn read_first(&self, patterns: &[PathBuf]) -> Result<FirstFile> {
        let paths: Vec<PathBuf> = patterns.iter().flat_map(|p| expand_local(p)).collect();
        let contents = match paths.first() {
            Some(first) => {
                Some(std::fs::read(first).with_context(|| format!("reading {}", first.display()))?)
            }
            None => None,
        };
        Ok(FirstFile { paths, contents })
    }

    fn fetch_dir(&self, dir: &Path, _dest: &Path) -> Result<PathBuf> {
        Ok(dir.to_path_buf())
    }
}

/// The local files matching `pattern` (see [`Host::read_first`]), sorted.
pub(crate) fn expand_local(pattern: &Path) -> Vec<PathBuf> {
    let mut found = vec![PathBuf::new()];
    for component in pattern.components() {
        let name = component.as_os_str();
        found = if name == "*" {
            let mut next: Vec<PathBuf> = found
                .iter()
                .filter_map(|dir| std::fs::read_dir(dir).ok())
                .flatten()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            next.sort();
            next
        } else {
            found.into_iter().map(|p| p.join(name)).collect()
        };
    }
    found.retain(|p| p.is_file());
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_follow_the_pattern_order_and_expand_stars() {
        let tmp = crate::tmpfile::TempDir::new("existing").unwrap();
        let p = tmp.path();
        for f in ["a.json", "sub/b/x.json", "sub/a/x.json", "sub/c/other"] {
            std::fs::create_dir_all(p.join(f).parent().unwrap()).unwrap();
            std::fs::write(p.join(f), "").unwrap();
        }
        let found = LocalHost
            .read_first(&[p.join("missing"), p.join("sub/*/x.json"), p.join("a.json")])
            .unwrap();
        assert_eq!(
            found.paths,
            [
                p.join("sub/a/x.json"),
                p.join("sub/b/x.json"),
                p.join("a.json")
            ]
        );
    }

    #[test]
    fn a_local_scratch_file_holds_the_content_until_dropped() {
        let f = LocalHost.write_scratch("x.yml", "a: 1\n").unwrap();
        let path = PathBuf::from(f.path());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a: 1\n");
        drop(f);
        assert!(!path.exists());
    }

    #[test]
    fn it_is_this_machine() {
        assert_eq!(LocalHost.label(), None);
        assert_eq!(LocalHost.names(), ["local"]);
        assert_eq!(LocalHost.on_host(), "");
        assert!(LocalHost.shares_cwd());
        assert!(
            LocalHost
                .tunnel(1, "x:2", crate::host::Stream::Null)
                .is_none()
        );
    }
}
