//! The manifest: what a bundle contains, and the rules a bundle must obey.
//!
//! A bundle is a file that arrives from somewhere else, so a manifest is
//! validated before anything is trusted: paths inside the bundle are *derived
//! from names* (`bin/<tool>`, `lib/<file>`, `config/<tool>`) rather than stored,
//! so it cannot point outside itself, and names that end up in shell scripts
//! (alias names) are restricted to plain characters.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

use crate::elf::Arch;
use crate::fingerprint::content_hash;

/// The bundle format this ldg writes and reads.
pub const FORMAT: u32 = 1;

pub const MANIFEST_FILE: &str = "manifest.json";
pub const BIN_DIR: &str = "bin";
pub const LIB_DIR: &str = "lib";
pub const CONFIG_DIR: &str = "config";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub lodge_version: String,
    /// Seconds since the Unix epoch. Informational: not part of the identity.
    pub created: u64,
    /// The CPU architecture every binary was built for (`x86_64`, `aarch64`, …).
    pub arch: String,
    /// The dynamic linker's file name, when any binary is dynamic. It is also one
    /// of `libs`.
    pub linker: Option<String>,
    /// Shared libraries (and the linker), stored at `lib/<name>`.
    pub libs: Vec<Lib>,
    pub tools: Vec<Tool>,
    pub aliases: BTreeMap<String, String>,
    pub git_safe_directories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lib {
    pub name: String,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub binary: Option<Binary>,
    pub config: Option<ConfigTree>,
    pub skip_if_exists: bool,
}

/// A binary, stored at `bin/<tool name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binary {
    /// Where the wrapper goes in the container (`~/` is the user's home).
    pub target: String,
    /// Whether it was relinked to the bundled glibc (false: static, or copied as is).
    pub dynamic: bool,
    pub hash: String,
}

/// A config file or tree, stored at `config/<tool name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigTree {
    /// Where it goes in the container (`~/` is the user's home).
    pub target: String,
    pub is_dir: bool,
    pub hash: String,
}

impl Manifest {
    /// A short fingerprint of what the bundle contains, for noticing that it
    /// changed. The creation time is left out, so packing the same inputs twice
    /// gives the same id.
    pub fn id(&self) -> String {
        let mut canonical = self.clone();
        canonical.created = 0;
        content_hash(&serde_json::to_vec(&canonical).unwrap_or_default())
    }

    pub fn arch(&self) -> Result<Arch> {
        Arch::from_uname(&self.arch)
            .with_context(|| format!("the bundle names an unknown architecture '{}'", self.arch))
    }

    pub fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).context("writing the manifest")
    }

    /// Parses and validates a manifest. The format version is checked first, so a
    /// bundle from a newer ldg is refused with a clear message rather than a parse
    /// error.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).context("reading manifest.json")?;
        let format = value
            .get("format")
            .and_then(serde_json::Value::as_u64)
            .context("the manifest has no format version")?;
        if format != u64::from(FORMAT) {
            bail!("this bundle is format {format}; this ldg reads format {FORMAT} (update ldg?)");
        }
        let manifest: Manifest = serde_json::from_value(value).context("reading manifest.json")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        if self.format != FORMAT {
            bail!("unsupported bundle format {}", self.format);
        }
        self.arch()?;

        let mut lib_names = HashSet::new();
        for lib in &self.libs {
            check_name("library", &lib.name)?;
            check_hash(&lib.name, &lib.hash)?;
            if !lib_names.insert(lib.name.as_str()) {
                bail!("library '{}' is listed twice", lib.name);
            }
        }
        if let Some(linker) = &self.linker
            && !lib_names.contains(linker.as_str())
        {
            bail!("the dynamic linker '{linker}' is not among the bundled libraries");
        }

        let mut tool_names = HashSet::new();
        for tool in &self.tools {
            check_name("tool", &tool.name)?;
            if !tool_names.insert(tool.name.as_str()) {
                bail!("tool '{}' is listed twice", tool.name);
            }
            if tool.binary.is_none() && tool.config.is_none() {
                bail!("tool '{}' has neither a binary nor a config", tool.name);
            }
            if let Some(b) = &tool.binary {
                check_target(&tool.name, &b.target)?;
                check_hash(&tool.name, &b.hash)?;
                if b.dynamic && self.linker.is_none() {
                    bail!(
                        "tool '{}' is dynamic but the bundle has no linker",
                        tool.name
                    );
                }
            }
            if let Some(c) = &tool.config {
                check_target(&tool.name, &c.target)?;
                check_hash(&tool.name, &c.hash)?;
            }
        }

        for name in self.aliases.keys() {
            // Alias names are written unquoted into a shell script.
            if !is_alias_name(name) {
                bail!("'{name}' is not a plain alias name");
            }
        }
        Ok(())
    }
}

/// A file name that is safe as one path component and in a script: letters,
/// digits and `-_.+`, not `.` or `..`, not empty.
pub fn is_plain_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

fn is_alias_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn check_name(kind: &str, name: &str) -> Result<()> {
    if !is_plain_name(name) {
        bail!("{kind} name '{name}' is not a plain file name");
    }
    Ok(())
}

fn check_hash(what: &str, hash: &str) -> Result<()> {
    if hash.len() != 16 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("'{what}' has a malformed hash '{hash}'");
    }
    Ok(())
}

/// A container path: absolute or under `~/`, without `..` or NUL.
fn check_target(tool: &str, target: &str) -> Result<()> {
    let ok = (target.starts_with('/') || target.starts_with("~/"))
        && !target.contains('\0')
        && !target.split('/').any(|part| part == "..");
    if !ok {
        bail!("tool '{tool}' has an unusable target path '{target}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            format: FORMAT,
            lodge_version: "0.0.0".into(),
            created: 1_700_000_000,
            arch: "x86_64".into(),
            linker: Some("ld-linux-x86-64.so.2".into()),
            libs: vec![
                Lib {
                    name: "ld-linux-x86-64.so.2".into(),
                    hash: "0123456789abcdef".into(),
                },
                Lib {
                    name: "libc.so.6".into(),
                    hash: "fedcba9876543210".into(),
                },
            ],
            tools: vec![Tool {
                name: "nvim".into(),
                binary: Some(Binary {
                    target: "/usr/local/bin/nvim".into(),
                    dynamic: true,
                    hash: "00000000000000aa".into(),
                }),
                config: Some(ConfigTree {
                    target: "~/.config/nvim".into(),
                    is_dir: true,
                    hash: "00000000000000bb".into(),
                }),
                skip_if_exists: true,
            }],
            aliases: BTreeMap::from([("ll".into(), "ls -la".into())]),
            git_safe_directories: vec!["*".into()],
        }
    }

    #[test]
    fn a_well_formed_manifest_round_trips() {
        let m = sample();
        let again = Manifest::from_json(&m.to_json().unwrap()).unwrap();
        assert_eq!(m, again);
    }

    #[test]
    fn the_id_ignores_the_creation_time_but_not_the_content() {
        let a = sample();
        let mut later = sample();
        later.created += 1000;
        assert_eq!(a.id(), later.id());
        let mut other = sample();
        other.libs[1].hash = "1111111111111111".into();
        assert_ne!(a.id(), other.id());
    }

    #[test]
    fn a_newer_format_is_refused_with_a_clear_message() {
        let mut v: serde_json::Value =
            serde_json::from_slice(&sample().to_json().unwrap()).unwrap();
        v["format"] = 99.into();
        v["somethingNew"] = true.into();
        let err = Manifest::from_json(v.to_string().as_bytes())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("format 99") && err.contains("update ldg"),
            "{err}"
        );
    }

    fn invalid(edit: impl FnOnce(&mut Manifest)) -> String {
        let mut m = sample();
        edit(&mut m);
        m.validate().unwrap_err().to_string()
    }

    #[test]
    fn names_cannot_climb_out_of_the_bundle() {
        assert!(invalid(|m| m.tools[0].name = "../evil".into()).contains("plain file name"));
        assert!(invalid(|m| m.libs[1].name = "a/b".into()).contains("plain file name"));
        assert!(invalid(|m| m.tools[0].name = "..".into()).contains("plain file name"));
    }

    #[test]
    fn targets_must_be_absolute_or_home_relative_without_dotdot() {
        assert!(
            invalid(|m| m.tools[0].binary.as_mut().unwrap().target = "bin/nvim".into())
                .contains("unusable target")
        );
        assert!(
            invalid(|m| m.tools[0].config.as_mut().unwrap().target = "/etc/../x".into())
                .contains("unusable target")
        );
    }

    #[test]
    fn alias_names_cannot_smuggle_shell_syntax() {
        assert!(
            invalid(|m| {
                m.aliases.insert("x; rm -rf /".into(), "true".into());
            })
            .contains("alias")
        );
    }

    #[test]
    fn hashes_linker_and_duplicates_are_checked() {
        assert!(invalid(|m| m.libs[0].hash = "zz".into()).contains("malformed hash"));
        assert!(invalid(|m| m.linker = Some("missing.so".into())).contains("not among"));
        assert!(invalid(|m| m.libs.push(m.libs[0].clone())).contains("listed twice"));
        assert!(invalid(|m| m.linker = None).contains("no linker"));
        assert!(invalid(|m| m.arch = "pdp11".into()).contains("unknown architecture"));
    }

    #[test]
    fn a_tool_must_bring_something() {
        assert!(
            invalid(|m| {
                m.tools[0].binary = None;
                m.tools[0].config = None;
            })
            .contains("neither")
        );
    }
}
