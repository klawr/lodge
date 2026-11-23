//! `ldg init`: a commented starting config, with the tools found on this
//! machine filled in (commented out, so nothing is injected until you choose),
//! and a commented client file beside it for what is this machine's own.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

use super::Ctx;
use crate::config::Document;
use crate::layout::tilde;
use crate::output::{info, line, note};

/// Where editors and TOML tools fetch the schema of `config.toml` from.
pub const SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/klawr/lodge/master/docs/config.schema.json";

/// The same for `client.toml`.
pub const CLIENT_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/klawr/lodge/master/docs/client.schema.json";

/// A tool `init` looks for: its binary on `PATH`, and the paths under the home
/// directory (its config) worth bringing along when they exist. What it keeps
/// beside its binary is found the same way for any program, not listed here.
pub(super) struct Known {
    pub binary: &'static str,
    pub note: &'static str,
    pub config: &'static [&'static str],
}

const KNOWN: &[Known] = &[
    Known {
        binary: "nvim",
        note: "Neovim",
        config: &[".config/nvim"],
    },
    Known {
        binary: "hx",
        note: "Helix",
        config: &[".config/helix"],
    },
    Known {
        binary: "vim",
        note: "Vim",
        config: &[".vimrc", ".vim"],
    },
    Known {
        binary: "rg",
        note: "ripgrep",
        config: &[],
    },
    Known {
        binary: "fd",
        note: "fd",
        config: &[],
    },
    Known {
        binary: "fzf",
        note: "fzf",
        config: &[],
    },
    Known {
        binary: "bat",
        note: "bat",
        config: &[".config/bat"],
    },
    Known {
        binary: "eza",
        note: "eza",
        config: &[],
    },
    Known {
        binary: "jq",
        note: "jq",
        config: &[],
    },
    Known {
        binary: "lazygit",
        note: "lazygit",
        config: &[".config/lazygit"],
    },
    Known {
        binary: "yazi",
        note: "yazi",
        config: &[".config/yazi"],
    },
    Known {
        binary: "starship",
        note: "starship prompt",
        config: &[".config/starship.toml"],
    },
    Known {
        binary: "zoxide",
        note: "zoxide",
        config: &[],
    },
    Known {
        binary: "claude",
        note: "Claude Code; deliver a token (`claude setup-token`) with a [[secret]] in client.toml, not its files as data",
        config: &[],
    },
];

pub fn run(ctx: &Ctx, force: bool, print: bool, print_client: bool) -> Result<()> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let found = detect(&home, &ctx.local.var("PATH").unwrap_or_default());
    let text = render(&found, &home);

    if print {
        // One file's text, so what is printed can be redirected into it.
        crate::output::raw_stdout(&if print_client { render_client() } else { text });
        return Ok(());
    }
    let path = &ctx.paths.config;
    if path.exists() && !force {
        bail!(
            "{} already exists (--force overwrites it, --print shows what would be written)",
            path.display()
        );
    }
    path.write_text(&text)?;
    info!("wrote {}", path.display());
    // The client's file may hold secrets' sources a person wrote: never overwritten.
    let client = &ctx.paths.client;
    if client.exists() {
        note!(
            "{} is there already and was left as it is",
            client.display()
        );
    } else {
        client.write_text(&render_client())?;
        info!("wrote {}", client.display());
    }
    if found.is_empty() {
        note!("no known tools found on PATH; add [[binary]] and [[data]] entries by hand");
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
    /// The paths of its config that exist.
    pub config: Vec<PathBuf>,
}

/// The known tools that are on `PATH`, each with the config of it that exists
/// under `home`.
pub(super) fn detect(home: &Path, path_var: &std::ffi::OsStr) -> Vec<Detected> {
    KNOWN
        .iter()
        .filter_map(|k| {
            let binary = crate::shell::which_in(k.binary, path_var)?;
            let binary = std::fs::canonicalize(&binary).unwrap_or(binary);
            let config = k
                .config
                .iter()
                .map(|c| home.join(c))
                .filter(|p| p.exists())
                .collect();
            Some(Detected {
                known: k,
                binary,
                config,
            })
        })
        .collect()
}

/// The client's file: nothing is set until a line is uncommented.
fn render_client() -> String {
    let mut s = format!(
        "#:schema {CLIENT_SCHEMA_URL}\n\
         #\n\
         # lodge client settings: what depends on this machine and the person using it.\n\
         # Never part of a bundle; the config (config.toml) is.\n\
         # Reference: https://github.com/klawr/lodge/blob/master/docs/configuration.md\n\n"
    );
    s.push_str(&format!(
        "# ── Target ───────────────────────────────────────────────────────────\n\
             # Where to work (--target overrides it): {}. The first is the default.\n\
             # default_target = \"{}\"\n\n",
        super::targets::all()
            .iter()
            .map(|t| format!("\"{}\" ({})", t.name(), t.about()))
            .collect::<Vec<_>>()
            .join(", "),
        super::targets::default_name()
    ));
    s.push_str(
        "# ── Bundle ───────────────────────────────────────────────────────────\n\
          # The bundles on offer: files, or registries (`ldg pack -o` adds what it writes).\n\
          # bundles = [\"oci://ghcr.io/me/lodge:1\"]\n\
          # The one to inject from instead of packing config.toml's entries: one of\n\
          # `bundles` (`ldg bundle use` changes it).\n\
          # default_bundle = \"oci://ghcr.io/me/lodge:1\"\n\
          \n\
          # ── Secrets (delivered at attach, never stored in a bundle) ──────────\n\
          # [[secret]]\n\
          # name = \"GH_TOKEN\"\n\
          # from = { command = \"gh auth token\" }\n\
          # as   = \"env\"\n\
          \n\
          # A value kept in this machine's credential store (`ldg secret set claude_token`):\n\
          # [[secret]]\n\
          # name = \"CLAUDE_CODE_OAUTH_TOKEN\"   # from `claude setup-token`\n\
          # from = { store = \"claude_token\" }\n\
          # as   = \"env\"\n",
    );
    s
}

fn render(found: &[Detected], home: &Path) -> String {
    let mut s = format!(
        "#:schema {SCHEMA_URL}\n\
         #\n\
         # lodge config: what `ldg up` brings into a container.\n\
         # Reference: https://github.com/klawr/lodge/blob/master/docs/configuration.md\n\
         # Check it with `ldg doctor`.\n\n"
    );
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
    s.push_str("\n# ── Data ──────────────────────────────────────────────────────────────\n");
    s.push_str("# Each [[data]] copies a file or directory as it is, to the same place in the\n");
    s.push_str("# container (`target` sets another). `for` names the program it belongs to.\n");
    let mut any = false;
    for f in found {
        let name = f.known.binary;
        let beside = super::candidates::companions(&f.binary);
        for place in beside.iter().chain(&f.config) {
            any = true;
            s.push_str(&format!(
                "#\n# [[data]]\n# path = \"{}\"\n# for  = \"{name}\"\n",
                tilde(place, home)
            ));
        }
    }
    if !any {
        s.push_str("#\n# [[data]]\n# path = \"~/.config/nvim\"\n# for  = \"nvim\"\n");
    }
    s.push_str(
        "\n# ── Session ──────────────────────────────────────────────────────────\n\
         # [session]\n\
         # shell = \"/bin/bash\"   # must exist in the container\n\
         \n\
         # What is this machine's own (the target, where a bundle is read from, secrets,\n\
         # keys) is in client.toml, beside this file.\n",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    static NVIM: Known = Known {
        binary: "nvim",
        note: "Neovim",
        config: &[".config/nvim"],
    };

    #[test]
    fn the_rendered_client_file_parses_and_sets_nothing_until_uncommented() {
        let text = render_client();
        assert!(text.starts_with("#:schema "));
        let c = crate::config::client::Client::parse(&text, Path::new("/x/client.toml")).unwrap();
        assert!(c.default_target.is_none() && c.default_bundle.is_none() && c.secret.is_empty());
        // Uncommenting a secret gives a valid one.
        let on = text.replace("# [[secret]]\n# name = \"GH_TOKEN\"\n# from = { command = \"gh auth token\" }\n# as   = \"env\"", "[[secret]]\nname = \"GH_TOKEN\"\nfrom = { command = \"gh auth token\" }\nas = \"env\"");
        let c = crate::config::client::Client::parse(&on, Path::new("/x/client.toml")).unwrap();
        assert_eq!(c.secret.len(), 1);
    }

    #[test]
    fn the_rendered_config_parses_and_injects_nothing_until_uncommented() {
        let dir = crate::tmpfile::TempDir::new("init-render").unwrap();
        let home = dir.path().join("home");
        let prefix = home.join("opt");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::create_dir_all(prefix.join("share/nvim")).unwrap();
        let binary = prefix.join("bin/nvim");
        let found = [Detected {
            known: &NVIM,
            binary: binary.clone(),
            config: vec![home.join(".config/nvim")],
        }];
        let text = render(&found, &home);
        assert!(text.starts_with("#:schema "));
        assert!(text.contains(&format!("# path = \"{}\"", tilde(&binary, &home))));
        // What sits beside the program and its config are data for it, found for any
        // program the same way.
        assert!(text.contains("# path = \"~/opt/share/nvim\"\n# for  = \"nvim\""));
        assert!(text.contains("# path = \"~/.config/nvim\"\n# for  = \"nvim\""));
        let cfg = crate::config::Config::parse(&text, Path::new("/x/config.toml")).unwrap();
        assert!(cfg.binaries.is_empty() && cfg.data.is_empty());

        // Uncommenting a block gives a valid entry, of each kind.
        let uncommented = text
            .replace(
                &format!(
                    "# [[binary]]\n# name = \"nvim\"\n# path = \"{}\"",
                    tilde(&binary, &home)
                ),
                &format!(
                    "[[binary]]\nname = \"nvim\"\npath = \"{}\"",
                    tilde(&binary, &home)
                ),
            )
            .replace(
                "# [[data]]\n# path = \"~/.config/nvim\"\n# for  = \"nvim\"",
                "[[data]]\npath = \"~/.config/nvim\"\nfor = \"nvim\"",
            );
        let cfg = crate::config::Config::parse(&uncommented, Path::new("/x/config.toml")).unwrap();
        assert_eq!((cfg.binaries.len(), cfg.data.len()), (1, 1));
    }
}
