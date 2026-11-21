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
struct Known {
    binary: &'static str,
    note: &'static str,
    configs: &'static [&'static str],
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
    let found: Vec<Detected> = KNOWN
        .iter()
        .filter_map(|k| {
            let binary = crate::shell::which(k.binary)?;
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
        .collect();
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
        note!("no known tools found on PATH; add [[inject]] entries by hand");
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

struct Detected {
    known: &'static Known,
    binary: PathBuf,
    configs: Vec<PathBuf>,
}

/// `~/…` for paths under `home`, so the config reads (and travels) well.
fn tilde(path: &Path, home: &Path) -> String {
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
    s.push_str("# ── Tools ────────────────────────────────────────────────────────────\n");
    s.push_str("# Each [[inject]] copies a binary (with its libraries) and/or a config.\n");
    s.push_str("# Uncomment what you want in your containers.\n");
    if found.is_empty() {
        s.push_str("#\n# [[inject]]\n# name   = \"nvim\"\n# binary = \"/usr/bin/nvim\"\n# config = \"~/.config/nvim\"\n");
    }
    for f in found {
        let k = f.known;
        s.push_str(&format!(
            "\n# {}\n# [[inject]]\n# name   = \"{}\"\n# binary = \"{}\"\n",
            k.note,
            k.binary,
            tilde(&f.binary, home)
        ));
        if let [config] = f.configs.as_slice() {
            s.push_str(&format!("# config = \"{}\"\n", tilde(config, home)));
        }
        if f.configs.len() > 1 {
            for (i, c) in f.configs.iter().enumerate() {
                s.push_str(&format!(
                    "#\n# [[inject]]\n# name   = \"{}-config-{}\"\n# config = \"{}\"\n",
                    k.binary,
                    i + 1,
                    tilde(c, home)
                ));
            }
        }
        if k.binary == "nvim"
            && let Some(runtime) = nvim_runtime(&f.binary)
        {
            let r = runtime.display();
            s.push_str(&format!(
                "#\n# Neovim looks for its runtime at the path it was built with.\n\
                 # [[inject]]\n# name          = \"nvim-runtime\"\n# config        = \"{r}\"\n# target_config = \"{r}\"\n"
            ));
        }
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
        assert!(text.contains("# binary = \"/usr/bin/nvim\""));
        assert!(text.contains("# config = \"~/.config/nvim\""));
        let cfg = crate::config::Config::parse(&text, Path::new("/x/config.toml")).unwrap();
        assert!(cfg.inject.is_empty());

        // Uncommenting a block gives a valid entry.
        let uncommented = text.replace("# [[inject]]\n# name   = \"nvim\"\n# binary = \"/usr/bin/nvim\"\n# config = \"~/.config/nvim\"", "[[inject]]\nname   = \"nvim\"\nbinary = \"/usr/bin/nvim\"\nconfig = \"~/.config/nvim\"");
        let cfg = crate::config::Config::parse(&uncommented, Path::new("/x/config.toml")).unwrap();
        assert_eq!(cfg.inject.len(), 1);
    }

    #[test]
    fn paths_under_home_use_a_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(tilde(Path::new("/home/me/.vimrc"), home), "~/.vimrc");
        assert_eq!(tilde(Path::new("/usr/bin/rg"), home), "/usr/bin/rg");
    }
}
