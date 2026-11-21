//! The manifest: what a bundle contains, and the rules a bundle must obey.
//!
//! A bundle is a file that arrives from somewhere else, so a manifest is
//! validated before anything is trusted: paths inside the bundle are *derived
//! from names* (`bin/<tool>`, `lib/<file>`, `config/<tool>`) rather than stored,
//! so it cannot point outside itself.
//!
//! A bundle does not know where it will be installed. Its dynamic binaries carry
//! a placeholder interpreter and find their libraries relative to themselves
//! ([`ORIGIN_RPATH`]), so injection only has to write the real interpreter path
//! ([`crate::bundle::elf::set_interpreter`]).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::bundle::arch::Arch;
use crate::fingerprint::content_hash;
use crate::shell::{container_path_problem, is_plain_name};

pub const MANIFEST_FILE: &str = "manifest.json";
pub const BIN_DIR: &str = "bin";
pub const LIB_DIR: &str = "lib";
pub const CONFIG_DIR: &str = "config";

/// The library search path of a packed binary, relative to where the binary is:
/// `bin/` and `lib/` are siblings in a bundle and stay siblings where it is
/// installed. `DT_RPATH` (not `RUNPATH`) so it also resolves the libraries' own
/// dependencies. Part of the format.
pub const ORIGIN_RPATH: &str = "$ORIGIN/../lib";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
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
    pub git_safe_directories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lib {
    pub name: String,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub name: String,
    pub binary: Option<Binary>,
    pub config: Option<ConfigTree>,
}

/// A binary, stored at `bin/<tool name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binary {
    /// Where an extra wrapper script goes (`~/` is the user's home), when the
    /// config asked for one with `target_binary`. Without it the binary is
    /// reached through the `PATH` of lodge's sessions. Called `target` in the
    /// manifest, after the config's `target_binary`.
    #[serde(rename = "target", default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    /// Whether it was relinked to the bundled glibc (false: static, or copied as is).
    pub dynamic: bool,
    pub hash: String,
}

/// A config file or tree, stored at `config/<tool name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigTree {
    /// Where it goes in the container (`~/` is the user's home). Called
    /// `target` in the manifest, after the config's `target_config`.
    #[serde(rename = "target")]
    pub dest: String,
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

    /// Parses and validates a manifest.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let manifest: Manifest = serde_json::from_slice(bytes).context("reading manifest.json")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
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
                if let Some(dest) = &b.dest {
                    check_dest(&tool.name, dest)?;
                }
                check_hash(&tool.name, &b.hash)?;
                if b.dynamic && self.linker.is_none() {
                    bail!(
                        "tool '{}' is dynamic but the bundle has no linker",
                        tool.name
                    );
                }
            }
            if let Some(c) = &tool.config {
                check_dest(&tool.name, &c.dest)?;
                check_hash(&tool.name, &c.hash)?;
            }
        }
        Ok(())
    }
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

/// A container path: absolute or under `~/`, without `..` or control characters.
fn check_dest(tool: &str, dest: &str) -> Result<()> {
    if let Some(why) = container_path_problem(dest) {
        bail!("tool '{tool}' has an unusable target path {dest:?}: it {why}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
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
                    dest: Some("/usr/local/bin/nvim".into()),
                    dynamic: true,
                    hash: "00000000000000aa".into(),
                }),
                config: Some(ConfigTree {
                    dest: "~/.config/nvim".into(),
                    is_dir: true,
                    hash: "00000000000000bb".into(),
                }),
            }],
            git_safe_directories: vec!["*".into()],
        }
    }

    #[test]
    fn the_rpath_matches_the_bundle_layout() {
        assert_eq!(ORIGIN_RPATH, format!("$ORIGIN/../{LIB_DIR}"));
        assert_eq!(
            BIN_DIR, "bin",
            "the rpath assumes bin/ and lib/ are siblings"
        );
    }

    #[test]
    fn a_binary_without_a_wrapper_target_is_valid_and_leaves_it_out() {
        let mut m = sample();
        m.tools[0].binary.as_mut().unwrap().dest = None;
        m.validate().unwrap();
        assert!(
            !String::from_utf8(m.to_json().unwrap())
                .unwrap()
                .contains("\"target\": \"/usr")
        );
        assert_eq!(Manifest::from_json(&m.to_json().unwrap()).unwrap(), m);
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
            invalid(|m| m.tools[0].binary.as_mut().unwrap().dest = Some("bin/nvim".into()))
                .contains("unusable target")
        );
        assert!(
            invalid(|m| m.tools[0].config.as_mut().unwrap().dest = "/etc/../x".into())
                .contains("unusable target")
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
