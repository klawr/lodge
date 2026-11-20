//! Putting resolved secrets inside a container.
//!
//! Values live under `/dev/shm/lodge-secrets-<uid>/`: memory only, so never in an
//! image layer and gone when the container stops. The directory is private to the
//! session user (0700), each value is a 0600 file (or the mode a `file` secret
//! asks for), and everything is written over stdin ([`Env::write_secret`]) so
//! nothing secret reaches a command line.
//!
//! - an `env` secret becomes a variable that the session start-up script exports
//!   by reading its file ([`Placed::session_setup`]);
//! - a `file` secret is symlinked from the path the tool expects to its file in
//!   the directory, so the tool sees an ordinary credential file.
//!
//! A file in the directory whose secret is no longer declared is removed, so
//! dropping a `[[secret]]` takes effect without restarting the container.

use anyhow::{Context, Result};

use super::{As, Secret, SecretDecl};
use crate::runtime::{ContainerUser, Env, ExecOpts};
use crate::shell::shell_quote;

/// Mode of an `env` secret's file.
const ENV_MODE: u32 = 0o600;

/// `$1` is the directory, the rest are the names to keep. Creates it private to
/// the caller and refuses one somebody else made (`/dev/shm` is shared).
const PREPARE_SCRIPT: &str = r#"d=$1; shift
umask 077
mkdir -p "$d" && [ -d "$d" ] && [ ! -L "$d" ] && [ -O "$d" ] \
  || { echo "unsafe secret directory $d" >&2; exit 1; }
chmod 700 "$d"
for f in "$d"/*; do
  [ -e "$f" ] || continue
  keep=
  for n in "$@"; do [ "${f##*/}" = "$n" ] && keep=1; done
  [ -n "$keep" ] || rm -f "$f"
done"#;

/// `$1` is where the tool looks (`~/` allowed), `$2` the secret's file. Replaces
/// a stale file or link, but never a real directory.
const LINK_SCRIPT: &str = r#"case "$1" in "~/"*) p="$HOME/${1#"~/"}";; *) p=$1;; esac
if [ -d "$p" ] && [ ! -L "$p" ]; then echo "$p is a directory" >&2; exit 1; fi
mkdir -p "$(dirname "$p")" && ln -sfn "$2" "$p""#;

/// What was placed in one container.
#[derive(Debug)]
pub struct Placed {
    dir: String,
    env_names: Vec<String>,
    file_names: Vec<String>,
}

impl Placed {
    /// Names of every secret placed; never values.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.env_names
            .iter()
            .chain(&self.file_names)
            .map(String::as_str)
    }

    /// Shell lines for the start of a session that export the `env` secrets,
    /// reading each from its file inside the container, so the value is in no
    /// command line. Empty when there are none.
    pub fn session_setup(&self) -> String {
        self.env_names
            .iter()
            .map(|n| {
                let file = shell_quote(&format!("{}/{n}", self.dir));
                format!("if [ -r {file} ]; then export {n}=\"$(cat {file})\"; fi; ")
            })
            .collect()
    }
}

/// Places `items` in the container for `user`, and removes older ones that are no
/// longer wanted. The caller has already decided these may go to this container.
pub fn place(
    env: &dyn Env,
    user: &ContainerUser,
    items: &[(&SecretDecl, &Secret)],
) -> Result<Placed> {
    let opts = ExecOpts {
        user: user.exec_as(),
        ..ExecOpts::default()
    };
    let dir = secrets_dir(user);

    // Names are validated to [A-Za-z0-9_], so they are safe as arguments and as
    // shell variable names.
    let mut prepare = vec!["sh", "-c", PREPARE_SCRIPT, "sh", &dir];
    prepare.extend(items.iter().map(|(d, _)| d.name.as_str()));
    env.exec(&prepare, &opts)
        .context("preparing the secrets directory")?;

    let mut placed = Placed {
        dir,
        env_names: vec![],
        file_names: vec![],
    };
    for (decl, value) in items {
        let stored = format!("{}/{}", placed.dir, decl.name);
        match &decl.as_ {
            As::Env => {
                env.write_secret(&stored, value.expose(), ENV_MODE, user.exec_as())
                    .with_context(|| format!("placing secret {}", decl.name))?;
                placed.env_names.push(decl.name.clone());
            }
            As::File { path, mode } => {
                env.write_secret(&stored, value.expose(), *mode, user.exec_as())
                    .with_context(|| format!("placing secret {}", decl.name))?;
                env.exec(&["sh", "-c", LINK_SCRIPT, "sh", path, &stored], &opts)
                    .with_context(|| format!("linking secret {} at {path}", decl.name))?;
                placed.file_names.push(decl.name.clone());
            }
        }
    }
    Ok(placed)
}

fn secrets_dir(user: &ContainerUser) -> String {
    crate::layout::secrets_dir(&user.uid.to_string())
}

/// Removes every secret placed for `user`. Cheaper than [`place`] with nothing to
/// place, which would first create the directory.
pub fn clear(env: &dyn Env, user: &ContainerUser) -> Result<()> {
    let opts = ExecOpts {
        user: user.exec_as(),
        ..ExecOpts::default()
    };
    env.exec(&["rm", "-rf", &secrets_dir(user)], &opts)
        .context("removing placed secrets")
}

/// Each declared secret's name and whether it is placed for `user`: what
/// `ldg status` reports. Reads the directory listing only, never a value. An
/// error when the container can't be asked, which is not "none placed".
pub fn status(
    env: &dyn Env,
    user: &ContainerUser,
    decls: &[SecretDecl],
) -> Result<Vec<(String, bool)>> {
    if decls.is_empty() {
        return Ok(Vec::new());
    }
    let present = present_names(env, user)?;
    Ok(decls
        .iter()
        .map(|d| (d.name.clone(), present.contains(&d.name)))
        .collect())
}

/// The script behind [`present_names`]: lists the directory `$0`, and nothing
/// (successfully) when there is none.
const LIST_SCRIPT: &str = "[ -d \"$0\" ] || exit 0; ls -1 \"$0\"";

/// Names of the secrets currently placed for `user`.
fn present_names(env: &dyn Env, user: &ContainerUser) -> Result<Vec<String>> {
    let out = env
        .exec_capture_as(
            user.exec_as(),
            &["sh", "-c", LIST_SCRIPT, &secrets_dir(user)],
        )
        .context("listing placed secrets")?;
    Ok(out
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;
    use crate::secret::Source;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn dev() -> ContainerUser {
        ContainerUser {
            requested: Some("dev".into()),
            name: "dev".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/dev".into(),
        }
    }

    fn decl(name: &str, as_: As) -> SecretDecl {
        SecretDecl {
            name: name.into(),
            from: Source::Env("UNUSED".into()),
            as_,
            targets: vec![],
        }
    }

    #[test]
    fn env_and_file_secrets_are_placed_under_the_users_shm_directory() {
        let env = FakeEnv::new();
        let (a, b) = (
            decl("API_KEY", As::Env),
            decl(
                "GH",
                As::File {
                    path: "~/.config/gh/token".into(),
                    mode: 0o640,
                },
            ),
        );
        let (va, vb) = (
            Secret::new(b"k-123".to_vec()),
            Secret::new(b"g-456".to_vec()),
        );
        let placed = place(&env, &dev(), &[(&a, &va), (&b, &vb)]).unwrap();

        let dir = "/dev/shm/lodge-secrets-1000";
        assert_eq!(
            env.file(&format!("{dir}/API_KEY")).as_deref(),
            Some("k-123")
        );
        assert_eq!(env.mode(&format!("{dir}/API_KEY")), Some(0o600));
        assert_eq!(env.mode(&format!("{dir}/GH")), Some(0o640));
        // The tool's path is linked to the stored file.
        assert!(env.ran_matching(|c| c.len() == 6
            && c[4] == "~/.config/gh/token"
            && c[5] == format!("{dir}/GH")));
        assert_eq!(placed.names().collect::<Vec<_>>(), ["API_KEY", "GH"]);
        // Only env secrets are exported; the value is read in the container.
        let setup = placed.session_setup();
        assert!(setup.contains("export API_KEY=\"$(cat '/dev/shm/lodge-secrets-1000/API_KEY')\""));
        assert!(!setup.contains("GH") && !setup.contains("k-123"));
        // No command ran with a value in it.
        assert!(
            !env.ran_matching(|c| c.iter().any(|a| a.contains("k-123") || a.contains("g-456")))
        );
    }

    #[test]
    fn nothing_to_place_still_prunes_and_exports_nothing() {
        let env = FakeEnv::new();
        let placed = place(&env, &ContainerUser::root(), &[]).unwrap();
        assert!(placed.session_setup().is_empty());
        assert!(env.ran_matching(|c| c.get(4).is_some_and(|d| d == "/dev/shm/lodge-secrets-0")));
    }

    #[test]
    fn status_says_which_declared_secrets_are_placed() {
        let list = ["sh", "-c", LIST_SCRIPT, "/dev/shm/lodge-secrets-1000"];
        let env = FakeEnv::new().with_output(&list, "A\nOLD\n");
        let decls = [decl("A", As::Env), decl("B", As::Env)];
        assert_eq!(
            status(&env, &dev(), &decls).unwrap(),
            [("A".to_string(), true), ("B".to_string(), false)]
        );
        // Nothing declared: nothing to ask.
        assert!(status(&FakeEnv::new(), &dev(), &[]).unwrap().is_empty());
        // Not being able to ask is not "none placed".
        let env = FakeEnv::new().with_failure(&list);
        assert!(status(&env, &dev(), &decls).is_err());
    }

    #[test]
    fn the_list_script_lists_and_a_missing_directory_is_empty() {
        let tmp = crate::tmpfile::TempDir::new("list").unwrap();
        std::fs::write(tmp.path().join("A"), "").unwrap();
        let run = |dir: &std::path::Path| {
            let out = Command::new("sh")
                .args(["-c", LIST_SCRIPT, &dir.to_string_lossy()])
                .output()
                .unwrap();
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            )
        };
        assert_eq!(run(tmp.path()), (true, "A\n".to_string()));
        assert_eq!(run(&tmp.path().join("none")), (true, String::new()));
    }

    #[test]
    fn clearing_removes_the_users_secrets_directory() {
        let env = FakeEnv::new();
        clear(&env, &dev()).unwrap();
        assert!(env.ran(&["rm", "-rf", "/dev/shm/lodge-secrets-1000"]));
    }

    // The scripts, run for real against a temp directory.
    fn sh(script: &str, args: &[&str]) -> bool {
        Command::new("sh")
            .args(["-c", script, "sh"])
            .args(args)
            .env("HOME", "/nonexistent-home")
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn the_prepare_script_makes_a_private_directory_and_prunes_old_files() {
        let tmp = crate::tmpfile::TempDir::new("place").unwrap();
        let base = tmp.path();
        let d = base.join("secrets");
        let d = d.to_str().unwrap();
        std::fs::create_dir_all(base).unwrap();
        assert!(sh(PREPARE_SCRIPT, &[d, "KEEP"]));
        assert_eq!(
            std::fs::metadata(d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::write(format!("{d}/KEEP"), "1").unwrap();
        std::fs::write(format!("{d}/GONE"), "2").unwrap();
        assert!(sh(PREPARE_SCRIPT, &[d, "KEEP"]));
        assert!(std::path::Path::new(&format!("{d}/KEEP")).exists());
        assert!(!std::path::Path::new(&format!("{d}/GONE")).exists());
        // A symlinked directory (planted in a shared /dev/shm) is refused.
        let link = base.join("planted");
        std::os::unix::fs::symlink(base, &link).unwrap();
        assert!(!sh(PREPARE_SCRIPT, &[link.to_str().unwrap()]));
    }

    #[test]
    fn the_link_script_replaces_files_but_not_directories() {
        let tmp = crate::tmpfile::TempDir::new("link").unwrap();
        let base = tmp.path();
        let target = base.join("stored");
        std::fs::write(&target, "v").unwrap();
        let target = target.to_str().unwrap();
        let path = base.join("a/b/cred");
        let path = path.to_str().unwrap();
        assert!(sh(LINK_SCRIPT, &[path, target]));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "v");
        // A tool left a real file behind: it is replaced by the link.
        std::fs::remove_file(path).unwrap();
        std::fs::write(path, "stale").unwrap();
        assert!(sh(LINK_SCRIPT, &[path, target]));
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // A real directory in the way is refused.
        let dir = base.join("dir");
        std::fs::create_dir(&dir).unwrap();
        assert!(!sh(LINK_SCRIPT, &[dir.to_str().unwrap(), target]));
        // `~/` expands to the session user's home.
        let home = base.join("home");
        std::fs::create_dir(&home).unwrap();
        let ok = Command::new("sh")
            .args(["-c", LINK_SCRIPT, "sh", "~/.tok", target])
            .env("HOME", &home)
            .status()
            .unwrap()
            .success();
        assert!(ok && home.join(".tok").exists());
    }
}
