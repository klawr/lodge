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
    // The command list itself (the first code block of its section), not any
    // mention elsewhere in the document.
    let arch = read("docs/architecture.md");
    let section = &arch[arch
        .find("## Commands and control flow")
        .expect("a Commands and control flow section")..];
    let start = section.find("```\n").expect("a code block") + 4;
    let list = &section[start..start + section[start..].find("```").expect("its end")];
    let missing: Vec<String> = subcommands()
        .into_iter()
        .filter(|c| !list.contains(&format!("ldg {c}")))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/architecture.md's command list lacks {missing:?}"
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
fn the_docs_name_every_target() {
    // The targets are listed once in code (`cmd/targets.rs`). Where the docs list
    // them by hand, they must name each one there is, so a new target cannot be
    // left out of the prose.
    for doc in ["README.md", "docs/configuration.md"] {
        let text = read(doc);
        let missing: Vec<&str> = crate::cmd::targets::names()
            .into_iter()
            .filter(|name| !text.contains(&format!("`{name}`")))
            .collect();
        assert!(
            missing.is_empty(),
            "{doc} never names the target(s) {missing:?}"
        );
    }
}

#[test]
fn the_config_schema_is_current() {
    // The schema describes the file as written, so secrets in their raw form.
    let mut schema = serde_json::to_value(schemars::schema_for!(
        crate::config::Config<crate::config::secret::RawSecret>
    ))
    .unwrap();
    // What each target lets you say is its own to describe: the file's schema
    // lists the targets that exist, each with its settings, from the registry.
    let mut targets = serde_json::Map::new();
    for kind in crate::cmd::targets::all() {
        let mut settings = match kind.settings_schema() {
            Some(s) => serde_json::to_value(s).unwrap(),
            None => serde_json::json!({ "type": "object", "additionalProperties": false }),
        };
        settings.as_object_mut().unwrap().remove("$schema");
        targets.insert(kind.name().to_string(), settings);
    }
    schema["properties"]["targets"]["properties"] = targets.into();
    let mut names: Vec<serde_json::Value> = crate::cmd::targets::names()
        .into_iter()
        .map(Into::into)
        .collect();
    names.push(serde_json::Value::Null);
    schema["properties"]["target"]["enum"] = names.into();
    schema["properties"]["targets"]["additionalProperties"] = false.into();
    // The actions a key can be given to are known, so the editor can offer them.
    let any_keys = schema["properties"]["keys"]["additionalProperties"].clone();
    let actions: serde_json::Map<String, serde_json::Value> = crate::keys::Action::ALL
        .iter()
        .map(|a| (a.name().to_string(), any_keys.clone()))
        .collect();
    schema["properties"]["keys"]["properties"] = actions.into();
    schema["properties"]["keys"]["additionalProperties"] = false.into();
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
