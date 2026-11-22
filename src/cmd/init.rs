//! `ldg init`: a commented starting config, with the tools found on this
//! machine filled in (commented out, so nothing is injected until you choose).

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use super::Ctx;
use crate::output::{info, line, note};

/// Where editors and TOML tools fetch the schema of `config.toml` from.
pub const SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/klawr/lodge/master/docs/config.schema.json";

/// A tool `init` looks for: its binary on `PATH`, and the config paths (under
/// the home directory) worth bringing along when they exist.
pub(super) struct Known {
    pub binary: &'static str,
    pub note: &'static str,
    pub configs: &'static [&'static str],
}

const KNOWN: &[Known] = &[
    Known {
        binary: "nvim",
        note: "Neovim; its runtime is added below when found",
        configs: &[".config/nvim"],
    },
    Known {
        binary: "hx",
        note: "Helix; set HELIX_RUNTIME or inject its runtime folder too",
        configs: &[".config/helix"],
    },
    Known {
        binary: "vim",
        note: "Vim",
        configs: &[".vimrc", ".vim"],
    },
    Known {
        binary: "rg",
        note: "ripgrep",
        configs: &[],
    },
    Known {
        binary: "fd",
        note: "fd",
        configs: &[],
    },
    Known {
        binary: "fzf",
        note: "fzf",
        configs: &[],
    },
    Known {
        binary: "bat",
        note: "bat",
        configs: &[".config/bat"],
    },
    Known {
        binary: "eza",
        note: "eza",
        configs: &[],
    },
    Known {
        binary: "jq",
        note: "jq",
        configs: &[],
    },
    Known {
        binary: "lazygit",
        note: "lazygit",
        configs: &[".config/lazygit"],
    },
    Known {
        binary: "yazi",
        note: "yazi",
        configs: &[".config/yazi"],
    },
    Known {
        binary: "starship",
        note: "starship prompt",
        configs: &[".config/starship.toml"],
    },
    Known {
        binary: "zoxide",
        note: "zoxide",
        configs: &[],
    },
    Known {
        binary: "claude",
        note: "Claude Code; deliver its login with a [[secret]] below, not as a config",
        configs: &[],
    },
];

pub fn run(ctx: &Ctx, force: bool, print: bool) -> Result<()> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let found = detect(&home, &ctx.local.var("PATH").unwrap_or_default());
    let text = render(&found, &home);

    if print {
        crate::output::raw_stdout(&text);
        return Ok(());
    }
    let path = &ctx.config_path;
    if path.exists() && !force {
        bail!(
            "{} already exists (--force overwrites it, --print shows what would be written)",
            path.display()
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
    info!("wrote {}", path.display());
    if found.is_empty() {
        note!("no known tools found on PATH; add [[binary]] and [[config]] entries by hand");
    } else {
        let names: Vec<&str> = found.iter().map(|f| f.known.binary).collect();
        note!(
            "found {}: uncomment the ones you want, then run `ldg doctor`",
            names.join(", ")
        );
    }
    line!("");
    Ok(())
}

pub(super) struct Detected {
    pub known: &'static Known,
    pub binary: PathBuf,
    pub configs: Vec<PathBuf>,
}

/// The known tools that are on `PATH`, each with the configs of it that exist
/// under `home`.
pub(super) fn detect(home: &Path, path_var: &std::ffi::OsStr) -> Vec<Detected> {
    KNOWN
        .iter()
        .filter_map(|k| {
            let binary = crate::shell::which_in(k.binary, path_var)?;
            let binary = std::fs::canonicalize(&binary).unwrap_or(binary);
            let configs = k
                .configs
                .iter()
                .map(|c| home.join(c))
                .filter(|p| p.exists())
                .collect();
            Some(Detected {
                known: k,
                binary,
                configs,
            })
        })
        .collect()
}

/// `~/…` for paths under `home`, so the config reads (and travels) well.
pub(super) fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if home != Path::new("/") => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

fn render(found: &[Detected], home: &Path) -> String {
    let mut s = format!(
        "#:schema {SCHEMA_URL}\n\
         #\n\
         # lodge config: what `ldg up` brings into a container.\n\
         # Reference: https://github.com/klawr/lodge/blob/master/docs/configuration.md\n\
         # Check it with `ldg doctor`.\n\n"
    );
    s.push_str(&format!(
        "# ── Target ───────────────────────────────────────────────────────────\n\
             # Where to work (--target overrides it): {}. The first is the default.\n\
             # target = \"{}\"\n\n",
        super::targets::all()
            .iter()
            .map(|t| format!("\"{}\" ({})", t.name(), t.about()))
            .collect::<Vec<_>>()
            .join(", "),
        super::targets::default_name()
    ));
    s.push_str("# ── Programs ─────────────────────────────────────────────────────────\n");
    s.push_str("# Each [[binary]] copies a program, with the libraries it needs.\n");
    s.push_str("# Uncomment what you want in your containers.\n");
    if found.is_empty() {
        s.push_str("#\n# [[binary]]\n# name = \"nvim\"\n# path = \"/usr/bin/nvim\"\n");
    }
    for f in found {
        let k = f.known;
        s.push_str(&format!(
            "\n# {}\n# [[binary]]\n# name = \"{}\"\n# path = \"{}\"\n",
            k.note,
            k.binary,
            tilde(&f.binary, home)
        ));
    }
    s.push_str("\n# ── Configs ──────────────────────────────────────────────────────────\n");
    s.push_str("# Each [[config]] copies a config file or directory; `target` is where it\n");
    s.push_str("# goes in the container (default: ~/.config/<name>).\n");
    let mut any = false;
    for f in found {
        let k = f.known;
        for (i, c) in f.configs.iter().enumerate() {
            any = true;
            let name = if f.configs.len() == 1 {
                k.binary.to_string()
            } else {
                format!("{}-{}", k.binary, i + 1)
            };
            let at = tilde(c, home);
            s.push_str(&format!(
                "#\n# [[config]]\n# name   = \"{name}\"\n# path   = \"{at}\"\n# target = \"{at}\"\n"
            ));
        }
        if k.binary == "nvim"
            && let Some(runtime) = nvim_runtime(&f.binary)
        {
            any = true;
            let r = runtime.display();
            s.push_str(&format!(
                "#\n# Neovim looks for its runtime at the path it was built with.\n\
                 # [[config]]\n# name   = \"nvim-runtime\"\n# path   = \"{r}\"\n# target = \"{r}\"\n"
            ));
        }
    }
    if !any {
        s.push_str("#\n# [[config]]\n# name   = \"nvim\"\n# path   = \"~/.config/nvim\"\n# target = \"~/.config/nvim\"\n");
    }
    s.push_str(
        "\n# ── Session ──────────────────────────────────────────────────────────\n\
         # [session]\n\
         # shell = \"/bin/bash\"   # must exist in the container\n\
         \n\
         # ── Secrets (delivered at attach, never stored in a bundle) ──────────\n\
         # [[secret]]\n\
         # name = \"GH_TOKEN\"\n\
         # from = { command = \"gh auth token\" }\n\
         # as   = \"env\"\n",
    );
    s
}

/// `<prefix>/share/nvim/runtime` for an nvim at `<prefix>/bin/nvim`, if it exists.
fn nvim_runtime(binary: &Path) -> Option<PathBuf> {
    let prefix = binary.parent()?.parent()?;
    let runtime = prefix.join("share/nvim/runtime");
    runtime.is_dir().then_some(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;

    static NVIM: Known = Known {
        binary: "nvim",
        note: "Neovim",
        configs: &[".config/nvim"],
    };

    #[test]
    fn the_rendered_config_parses_and_injects_nothing_until_uncommented() {
        let home = Path::new("/home/me");
        let found = [Detected {
            known: &NVIM,
            binary: PathBuf::from("/usr/bin/nvim"),
            configs: vec![home.join(".config/nvim")],
        }];
        let text = render(&found, home);
        assert!(text.starts_with("#:schema "));
        assert!(text.contains("# path = \"/usr/bin/nvim\""));
        assert!(text.contains("# path   = \"~/.config/nvim\""));
        let cfg = crate::config::Config::parse(&text, Path::new("/x/config.toml")).unwrap();
        assert!(cfg.binaries.is_empty() && cfg.configs.is_empty());

        // Uncommenting a block gives a valid entry, of each kind.
        let uncommented = text
            .replace(
                "# [[binary]]\n# name = \"nvim\"\n# path = \"/usr/bin/nvim\"",
                "[[binary]]\nname = \"nvim\"\npath = \"/usr/bin/nvim\"",
            )
            .replace(
                "# [[config]]\n# name   = \"nvim\"\n# path   = \"~/.config/nvim\"\n# target = \"~/.config/nvim\"",
                "[[config]]\nname = \"nvim\"\npath = \"~/.config/nvim\"\ntarget = \"~/.config/nvim\"",
            );
        let cfg = crate::config::Config::parse(&uncommented, Path::new("/x/config.toml")).unwrap();
        assert_eq!((cfg.binaries.len(), cfg.configs.len()), (1, 1));
    }

    #[test]
    fn paths_under_home_use_a_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(tilde(Path::new("/home/me/.vimrc"), home), "~/.vimrc");
        assert_eq!(tilde(Path::new("/usr/bin/rg"), home), "/usr/bin/rg");
    }
}
