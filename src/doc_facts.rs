//! Facts the docs state that the code can check, so the README and docs/architecture.md
//! can't quietly drift from the code (as they did when bundles landed).
//!
//! Each test reads the docs from the checkout and compares them with the
//! definition in the code. When one fails, fix the doc (or the code), not the
//! test. The config schema is generated here too: `make schema` rewrites it.

#![cfg(test)]

use clap::CommandFactory;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap()
}

/// The subcommands a user can type (hidden ones and `help` left out).
fn subcommands() -> Vec<String> {
    crate::cli::Cli::command()
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .map(|c| c.get_name().to_string())
        .collect()
}

#[test]
fn the_readme_shows_every_command() {
    let readme = read("README.md");
    let missing: Vec<String> = subcommands()
        .into_iter()
        .filter(|c| !readme.contains(&format!("ldg {c}")))
        .collect();
    assert!(
        missing.is_empty(),
        "README.md never shows `ldg {missing:?}`"
    );
}

#[test]
fn the_architecture_lists_every_command() {
    let arch = read("docs/architecture.md");
    let missing: Vec<String> = subcommands()
        .into_iter()
        .filter(|c| !arch.contains(&format!("ldg {c}")))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/architecture.md's command list lacks {missing:?}"
    );
}

#[test]
fn the_documented_skip_if_exists_default_is_the_real_one() {
    let cfg = crate::config::Config::parse(
        "[[inject]]\nname = \"x\"\nbinary = \"/x\"\n",
        std::path::Path::new("/c.toml"),
    )
    .unwrap();
    let default = format!("`{}`", cfg.inject[0].skip_if_exists);
    let reference = read("docs/configuration.md");
    let row = reference
        .lines()
        .find(|l| l.starts_with("| `skip_if_exists`"))
        .expect("docs/configuration.md has a skip_if_exists row");
    assert!(
        row.contains(&default),
        "docs/configuration.md says {row:?}, default is {default}"
    );
    assert!(
        read("docs/architecture.md").contains(&format!("`skip_if_exists` defaults to {default}")),
        "architecture.md must say `skip_if_exists` defaults to {default}"
    );
}

#[test]
fn the_readme_requirements_name_every_host_tool() {
    let readme = read("README.md");
    let start = readme
        .find("## Requirements")
        .expect("a Requirements section");
    let section = &readme[start..];
    let section = &section[..section[3..].find("\n## ").map_or(section.len(), |i| i + 3)];
    for tool in crate::host_tools::ALL {
        assert!(
            section.contains(&format!("`{}`", tool.name)),
            "README Requirements doesn't mention `{}`",
            tool.name
        );
    }
}

#[test]
fn every_doc_the_readme_links_exists() {
    let readme = read("README.md");
    for link in readme.split("](").skip(1) {
        let target = link.split([')', '#']).next().unwrap_or("");
        if target.starts_with("http") || target.is_empty() {
            continue;
        }
        assert!(
            root().join(target).exists(),
            "README links to missing {target}"
        );
    }
}

#[test]
fn the_config_schema_is_current() {
    let schema = schemars::schema_for!(crate::config::Config);
    let text = serde_json::to_string_pretty(&schema).unwrap() + "\n";
    let path = root().join("docs/config.schema.json");
    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == text,
        "docs/config.schema.json is out of date: run `make schema`"
    );
}
