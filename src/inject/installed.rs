//! What lodge has already installed in a container, by content hash.
//!
//! With a hash per installed piece, a run can tell *unchanged* (do nothing, which
//! is cheap) from *changed* (update it). `skip_if_exists` still lets a user
//! protect something from being replaced once it is there. Each record is a
//! file `<tool>.<kind>` holding the hash and where the piece was placed, in the
//! directory it is given (the layout's `installed/`); this module is the only one
//! that knows that format.

use anyhow::{Context, Result};

use crate::runtime::Env;
use crate::shell::shell_quote;

/// What kind of piece of a tool is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// The binary, behind a wrapper at its target.
    Binary,
    /// A config file or tree.
    Config,
}

impl Kind {
    const ALL: [Kind; 2] = [Kind::Binary, Kind::Config];

    fn suffix(self) -> &'static str {
        match self {
            Kind::Binary => "bin",
            Kind::Config => "config",
        }
    }
}

/// One installed piece: a tool's binary or its config.
#[derive(Debug, Clone, Copy)]
pub(super) struct Piece<'a> {
    pub(super) tool: &'a str,
    pub(super) kind: Kind,
}

impl Piece<'_> {
    fn path(&self, dir: &str) -> String {
        format!("{dir}/{}.{}", self.tool, self.kind.suffix())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    /// Already installed with exactly this content.
    UpToDate,
    /// Present but different, and the user asked not to replace it.
    KeepExisting,
    Install,
}

/// What to do about `piece`, whose bundle content has hash `hash`. `present` is
/// whether something is installed at its destination now.
pub(super) fn decide(
    env: &dyn Env,
    dir: &str,
    piece: Piece,
    hash: &str,
    present: bool,
    skip_if_exists: bool,
) -> Decision {
    if present && recorded(env, dir, piece).as_deref() == Some(hash) {
        Decision::UpToDate
    } else if present && skip_if_exists {
        Decision::KeepExisting
    } else {
        Decision::Install
    }
}

// A record is two lines: the hash, then where the piece was placed. Anything
// else (including a missing or unreadable record) counts as unrecorded: the
// piece is installed again, which is safe.
fn recorded(env: &dyn Env, dir: &str, piece: Piece) -> Option<String> {
    let text = env.read_file(&piece.path(dir)).ok().flatten()?;
    let mut lines = text.lines().map(str::trim);
    let hash = lines.next().filter(|s| !s.is_empty())?;
    lines.next().filter(|s| !s.is_empty())?;
    Some(hash.to_string())
}

/// Records that `piece` is installed at `placed_at` (its path in the container)
/// with content `hash`.
pub(super) fn record(
    env: &dyn Env,
    dir: &str,
    piece: Piece,
    hash: &str,
    placed_at: &str,
) -> Result<()> {
    env.write_file(
        &piece.path(dir),
        format!("{hash}\n{placed_at}\n").as_bytes(),
    )
}

/// The script behind [`placed`]: one `<record name>\t<path>` line per record in
/// `dir`. Exits 0 when there are none, so a failure means the records could not
/// be read.
pub(super) fn placed_script(dir: &str) -> String {
    format!(
        "for f in {}/*; do [ -f \"$f\" ] || continue; \
         printf '%s\\t%s\\n' \"${{f##*/}}\" \"$(sed -n 2p \"$f\")\"; done; exit 0",
        shell_quote(dir)
    )
}

/// The kind of piece a record named `name` is about, if it is one.
fn kind_of(name: &str) -> Option<Kind> {
    let (_, suffix) = name.rsplit_once('.')?;
    Kind::ALL.into_iter().find(|k| k.suffix() == suffix)
}

/// Every recorded piece: its kind and where it was placed. Records from before
/// paths were kept are left out. An error when the records can't be read: not
/// knowing what is installed is not "nothing".
pub(super) fn placed(env: &dyn Env, dir: &str) -> Result<Vec<(Kind, String)>> {
    let actor = env.actor();
    let out = env
        .exec_capture_as(actor.exec_as(), &["sh", "-c", &placed_script(dir)])
        .context("reading what lodge installed")?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(_, p)| !p.trim().is_empty())
        .filter_map(|(name, p)| Some((kind_of(name)?, p.trim().to_string())))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeEnv;

    const DIR: &str = "/p/installed";

    fn bin(tool: &str) -> Piece<'_> {
        Piece {
            tool,
            kind: Kind::Binary,
        }
    }

    #[test]
    fn nothing_installed_means_install() {
        let env = FakeEnv::new();
        assert_eq!(
            decide(&env, DIR, bin("nvim"), "aa", false, false),
            Decision::Install
        );
        assert_eq!(
            decide(&env, DIR, bin("nvim"), "aa", false, true),
            Decision::Install
        );
    }

    #[test]
    fn the_same_content_is_up_to_date_whatever_skip_says() {
        let env = FakeEnv::new();
        record(&env, DIR, bin("nvim"), "aa", "/usr/local/bin/nvim").unwrap();
        assert_eq!(
            decide(&env, DIR, bin("nvim"), "aa", true, false),
            Decision::UpToDate
        );
        assert_eq!(
            decide(&env, DIR, bin("nvim"), "aa", true, true),
            Decision::UpToDate
        );
    }

    #[test]
    fn changed_content_is_installed_unless_the_user_protected_it() {
        let env = FakeEnv::new();
        record(&env, DIR, bin("nvim"), "aa", "/usr/local/bin/nvim").unwrap();
        assert_eq!(
            decide(&env, DIR, bin("nvim"), "bb", true, false),
            Decision::Install
        );
        assert_eq!(
            decide(&env, DIR, bin("nvim"), "bb", true, true),
            Decision::KeepExisting
        );
    }

    #[test]
    fn a_record_without_a_destination_counts_as_unrecorded() {
        let env = FakeEnv::new().with_file(&bin("x").path(DIR), "aa\n");
        assert_eq!(
            decide(&env, DIR, bin("x"), "aa", true, false),
            Decision::Install
        );
    }

    #[test]
    fn something_present_but_never_recorded_counts_as_changed() {
        // Installed by hand.
        let env = FakeEnv::new();
        assert_eq!(
            decide(&env, DIR, bin("x"), "aa", true, false),
            Decision::Install
        );
        assert_eq!(
            decide(&env, DIR, bin("x"), "aa", true, true),
            Decision::KeepExisting
        );
    }

    #[test]
    fn the_placed_script_lists_records_and_their_paths() {
        let dir = crate::tmpfile::TempDir::new("placed").unwrap();
        std::fs::write(dir.path().join("nvim.bin"), "aa\n/usr/local/bin/nvim\n").unwrap();
        std::fs::write(dir.path().join("old.bin"), "bb\n").unwrap();
        let script = placed_script(&dir.path().to_string_lossy());
        let out = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "nvim.bin\t/usr/local/bin/nvim\nold.bin\t\n"
        );
        // An empty folder is no records, not a failure.
        std::fs::remove_file(dir.path().join("nvim.bin")).unwrap();
        std::fs::remove_file(dir.path().join("old.bin")).unwrap();
        assert!(
            std::process::Command::new("sh")
                .args(["-c", &script])
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn records_that_cannot_be_read_are_an_error() {
        let env = FakeEnv::new().with_failure(&["sh", "-c", &placed_script(DIR)]);
        assert!(placed(&env, DIR).is_err());
        let env = FakeEnv::new().with_output(
            &["sh", "-c", &placed_script(DIR)],
            "nvim.bin\t/usr/local/bin/nvim\nold.bin\t\n",
        );
        assert_eq!(
            placed(&env, DIR).unwrap(),
            [(Kind::Binary, "/usr/local/bin/nvim".to_string())]
        );
    }

    #[test]
    fn record_names_say_what_kind_of_piece_they_are_about() {
        assert_eq!(kind_of("nvim.bin"), Some(Kind::Binary));
        assert_eq!(kind_of("nvim-runtime.config"), Some(Kind::Config));
        assert_eq!(kind_of("g++.bin"), Some(Kind::Binary));
        assert_eq!(kind_of("stray"), None);
        let piece = Piece {
            tool: "nvim",
            kind: Kind::Config,
        };
        assert_eq!(piece.path(DIR), "/p/installed/nvim.config");
    }

    #[test]
    fn a_record_for_a_missing_install_does_not_count() {
        let env = FakeEnv::new();
        record(&env, DIR, bin("x"), "aa", "/usr/local/bin/x").unwrap();
        assert_eq!(
            decide(&env, DIR, bin("x"), "aa", false, false),
            Decision::Install
        );
    }
}
