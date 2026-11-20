use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::secret::{self, SecretDecl};

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    /// A bundle file to inject from instead of packing the `[[inject]]` entries.
    #[serde(default)]
    pub bundle: Option<PathBuf>,
    #[serde(default)]
    pub inject: Vec<Injectable>,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    /// Secrets to deliver at attach time. Local config only, never in a bundle.
    #[serde(default)]
    pub secret: Vec<SecretDecl>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Injectable {
    pub name: String,
    /// Local path to the binary to copy in
    pub binary: Option<PathBuf>,
    /// Local config directory/file to copy in
    pub config: Option<PathBuf>,
    /// Where to place the binary inside the container
    pub target_binary: Option<String>,
    /// Where to place the config inside the container
    pub target_config: Option<String>,
    /// If true, never replace the binary/config once it exists in the container,
    /// even when the bundle has a newer version (for a tree you update inside the
    /// container yourself, such as installed plugins). The default, false, keeps
    /// it current: replaced when the bundle changed, left alone when it did not.
    #[serde(default)]
    pub skip_if_exists: bool,
    /// Copy shared library dependencies discovered via ldd (skips libs already present)
    #[serde(default = "default_true")]
    pub copy_deps: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SessionConfig {
    /// Shell to launch inside the container
    #[serde(default = "default_shell")]
    pub shell: String,
    /// Terminal multiplexer to wrap the session in: "tmux" or "none"
    /// ("none" execs straight into the container with no multiplexer).
    #[serde(default = "default_multiplexer")]
    pub multiplexer: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            shell: default_shell(),
            multiplexer: default_multiplexer(),
        }
    }
}

fn default_shell() -> String {
    "/bin/bash".into()
}

fn default_multiplexer() -> String {
    "tmux".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GitConfig {
    /// Directories to register as git `safe.directory` inside the container.
    /// Injected plugins and the mounted workspace are owned by the host user but
    /// git runs as root in the container, so without this git refuses them with
    /// "detected dubious ownership" and plugins/repos fail to load. Defaults to
    /// `["*"]` (trust all); set to `[]` to disable.
    #[serde(default = "default_safe_directories")]
    pub safe_directories: Vec<String>,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            safe_directories: default_safe_directories(),
        }
    }
}

fn default_safe_directories() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_true() -> bool {
    true
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if (s.starts_with("~/") || s == "~")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(&s[2..]);
    }
    path
}

/// The config file chosen with `-c`, for the whole run.
static CONFIG_FILE: OnceLock<PathBuf> = OnceLock::new();

/// Makes `path` the config file for this run. Unlike the default location, a file
/// asked for by name must exist.
pub fn use_config_file(path: PathBuf) -> Result<()> {
    let path = expand_tilde(path);
    if !path.is_file() {
        anyhow::bail!("config file {} does not exist", path.display());
    }
    // Absolute, so a relative `bundle =` resolves the same wherever we run from.
    let path = std::path::absolute(&path).unwrap_or(path);
    CONFIG_FILE.set(path).ok();
    Ok(())
}

/// A relative path in the config is relative to the config file's folder.
fn resolve_against(config_file: &Path, path: PathBuf) -> PathBuf {
    let path = expand_tilde(path);
    if path.is_absolute() {
        return path;
    }
    config_file
        .parent()
        .map_or(path.clone(), |dir| dir.join(&path))
}

impl Config {
    /// An empty config, for tests elsewhere in the crate.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self::default()
    }

    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw).context("parsing lodge config")?;
        cfg.bundle = cfg.bundle.take().map(|b| resolve_against(&path, b));
        for item in &mut cfg.inject {
            item.binary = item.binary.take().map(expand_tilde);
            item.config = item.config.take().map(expand_tilde);
        }
        secret::validate_all(&cfg.secret)?;
        Ok(cfg)
    }

    fn default() -> Self {
        Self {
            bundle: None,
            inject: vec![],
            session: SessionConfig::default(),
            git: GitConfig::default(),
            aliases: HashMap::new(),
            secret: vec![],
        }
    }
}

pub fn config_path() -> PathBuf {
    if let Some(path) = CONFIG_FILE.get() {
        return path.clone();
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("lodge")
        .join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.inject.is_empty());
        assert_eq!(c.session.shell, "/bin/bash");
        assert_eq!(c.session.multiplexer, "tmux");
        assert_eq!(c.git.safe_directories, vec!["*".to_string()]);
        assert!(c.aliases.is_empty());
    }

    #[test]
    fn parses_full_config() {
        let toml = r#"
            [[inject]]
            name = "nvim"
            binary = "/usr/bin/nvim"
            skip_if_exists = false

            [session]
            shell = "/bin/zsh"
            multiplexer = "none"

            [git]
            safe_directories = ["/work"]

            [aliases]
            ll = "ls -la"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.inject.len(), 1);
        assert_eq!(c.inject[0].name, "nvim");
        assert!(!c.inject[0].skip_if_exists);
        assert!(c.inject[0].copy_deps); // defaults to true
        assert_eq!(c.session.shell, "/bin/zsh");
        assert_eq!(c.session.multiplexer, "none");
        assert_eq!(c.git.safe_directories, vec!["/work".to_string()]);
        assert_eq!(c.aliases.get("ll").map(String::as_str), Some("ls -la"));
    }

    #[test]
    fn a_relative_path_is_relative_to_the_config_file() {
        let cfg = Path::new("/srv/deploy/config.toml");
        assert_eq!(
            resolve_against(cfg, "bundle.tar".into()),
            Path::new("/srv/deploy/bundle.tar")
        );
        assert_eq!(
            resolve_against(cfg, "../b/x.tar".into()),
            Path::new("/srv/deploy/../b/x.tar")
        );
        assert_eq!(
            resolve_against(cfg, "/abs/x.tar".into()),
            Path::new("/abs/x.tar")
        );
    }

    #[test]
    fn parses_secret_declarations() {
        let c: Config = toml::from_str(
            "[[secret]]\nname = \"GH_TOKEN\"\nfrom = { command = \"gh auth token\" }\nas = \"env\"\n",
        )
        .unwrap();
        assert_eq!(c.secret.len(), 1);
        assert!(Config::default().secret.is_empty());
    }
}
