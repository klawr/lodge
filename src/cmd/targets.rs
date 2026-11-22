//! The targets lodge knows, in one list.
//!
//! Each is a [`TargetKind`]: a name, the settings it understands, and the concrete
//! types that make it up. This file and the command layer are the only places that
//! name those types, and the list in [`all`] is the only place that says how many
//! targets there are: a new one is a struct here and one line there. Everything
//! else (the flag's values, the config's `target`, the settings tables, the help)
//! is read from this list.

use anyhow::{Context, Result, bail};
use std::sync::Arc;

use super::prompt;
use crate::devcontainer::Devcontainer;
use crate::kube::{KubeEndpoints, KubeOptions};
use crate::plain_host::PlainHostEndpoints;
#[cfg(test)]
use crate::provision::Made;
use crate::provision::{Endpoints, TargetKind, Wiring};
use crate::runtime::Runtime;
use crate::runtime::cli::{Docker, DockerOptions, Podman, PodmanOptions};

/// Every target, the one used when nothing is said first.
pub fn all() -> Vec<Box<dyn TargetKind>> {
    vec![
        Box::new(DockerTarget),
        Box::new(PodmanTarget),
        Box::new(HostTarget),
        Box::new(KubeTarget),
    ]
}

/// The names of every target.
pub fn names() -> Vec<&'static str> {
    all().iter().map(|t| t.name()).collect()
}

/// The target that is used when none is named.
pub fn default_name() -> &'static str {
    all()[0].name()
}

/// The target called `name`.
pub fn find(name: &str) -> Option<Box<dyn TargetKind>> {
    all().into_iter().find(|t| t.name() == name)
}

/// `name` if it is a target, else an error that lists them.
pub fn check(name: &str) -> Result<&'static str> {
    match find(name) {
        Some(kind) => Ok(kind.name()),
        None => bail!("no target `{name}`; the targets are {}", names().join(", ")),
    }
}

/// A devcontainer on `rt`, which runs on the wiring's host.
fn devcontainer(rt: Box<dyn Runtime>, with: Wiring) -> Box<dyn Endpoints> {
    Box::new(Devcontainer::new(
        with.host,
        with.local,
        rt,
        with.oci,
        Arc::new(prompt::Terminal::default()),
    ))
}

/// The settings table as `T`, which refuses a key it does not know.
fn settings_as<T: serde::de::DeserializeOwned>(settings: &toml::Table, name: &str) -> Result<T> {
    settings
        .clone()
        .try_into()
        .with_context(|| format!("invalid [targets.{name}]"))
}

struct DockerTarget;

impl TargetKind for DockerTarget {
    fn name(&self) -> &'static str {
        "docker"
    }

    fn about(&self) -> &'static str {
        "a devcontainer on the docker CLI"
    }

    fn build(&self, with: Wiring, settings: &toml::Table) -> Result<Box<dyn Endpoints>> {
        let options: DockerOptions = settings_as(settings, self.name())?;
        Ok(devcontainer(
            Box::new(Docker::with_options(with.host.clone(), options)),
            with,
        ))
    }

    #[cfg(test)]
    fn made_of(&self) -> Made {
        Made {
            types: &[
                "CliEngine",
                "CliContainer",
                "DockerFlavor",
                "DockerOptions",
                "Devcontainer::new",
            ],
            home: &["runtime/cli/", "devcontainer/"],
        }
    }

    #[cfg(test)]
    fn settings_schema(&self) -> Option<schemars::Schema> {
        Some(schemars::schema_for!(DockerOptions))
    }
}

struct PodmanTarget;

impl TargetKind for PodmanTarget {
    fn name(&self) -> &'static str {
        "podman"
    }

    fn about(&self) -> &'static str {
        "a devcontainer on the podman CLI"
    }

    fn build(&self, with: Wiring, settings: &toml::Table) -> Result<Box<dyn Endpoints>> {
        let options: PodmanOptions = settings_as(settings, self.name())?;
        Ok(devcontainer(
            Box::new(Podman::with_options(with.host.clone(), options)),
            with,
        ))
    }

    #[cfg(test)]
    fn made_of(&self) -> Made {
        Made {
            types: &[
                "CliEngine",
                "CliContainer",
                "PodmanFlavor",
                "PodmanOptions",
                "Devcontainer::new",
            ],
            home: &["runtime/cli/", "devcontainer/"],
        }
    }

    #[cfg(test)]
    fn settings_schema(&self) -> Option<schemars::Schema> {
        Some(schemars::schema_for!(PodmanOptions))
    }
}

struct HostTarget;

impl TargetKind for HostTarget {
    fn name(&self) -> &'static str {
        "host"
    }

    fn about(&self) -> &'static str {
        "the machine itself, with no container"
    }

    fn build(&self, with: Wiring, settings: &toml::Table) -> Result<Box<dyn Endpoints>> {
        if let Some(key) = settings.keys().next() {
            bail!(
                "invalid [targets.{}]: it has no setting `{key}`",
                self.name()
            );
        }
        Ok(Box::new(PlainHostEndpoints::new(with.host)))
    }

    #[cfg(test)]
    fn made_of(&self) -> Made {
        Made {
            types: &["PlainHost"],
            home: &["plain_host.rs"],
        }
    }
}

struct KubeTarget;

impl TargetKind for KubeTarget {
    fn name(&self) -> &'static str {
        "kubernetes"
    }

    fn about(&self) -> &'static str {
        "a running Kubernetes pod, through kubectl"
    }

    fn build(&self, with: Wiring, settings: &toml::Table) -> Result<Box<dyn Endpoints>> {
        let options: KubeOptions = settings_as(settings, self.name())?;
        Ok(Box::new(KubeEndpoints::new(with.host, options)))
    }

    /// Which pod is the project's to say.
    fn per_project(&self) -> bool {
        true
    }

    /// A project that names a pod (or a selector) replaces how the user's config
    /// found one, which is the other of the two: the pair is one setting.
    fn merge_project(&self, user: &toml::Table, project: &toml::Table) -> toml::Table {
        let mut merged = user.clone();
        if project.contains_key("pod") || project.contains_key("selector") {
            merged.remove("pod");
            merged.remove("selector");
        }
        merged.extend(project.clone());
        merged
    }

    #[cfg(test)]
    fn made_of(&self) -> Made {
        Made {
            types: &["KubePod", "KubeEndpoints", "KubeOptions"],
            home: &["kube.rs"],
        }
    }

    #[cfg(test)]
    fn settings_schema(&self) -> Option<schemars::Schema> {
        Some(schemars::schema_for!(KubeOptions))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::fake::FakeHost;
    use crate::local::fake::FakeLocal;
    use crate::tools::NoRegistry;

    fn wiring() -> Wiring {
        Wiring {
            host: Arc::new(FakeHost::new()),
            local: Arc::new(FakeLocal::new()),
            oci: Arc::new(NoRegistry),
        }
    }

    fn table(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    #[test]
    fn the_targets_are_listed_once_and_docker_is_the_default() {
        assert_eq!(names(), ["docker", "podman", "host", "kubernetes"]);
        assert_eq!(default_name(), "docker");
        assert!(find("podman").is_some() && find("nonesuch").is_none());
        let err = check("dockr").unwrap_err().to_string();
        assert!(err.contains("docker, podman, host, kubernetes"), "{err}");
    }

    #[test]
    fn every_target_builds_from_no_settings_and_says_what_it_is() {
        for kind in all() {
            assert!(!kind.about().is_empty());
            kind.build(wiring(), &toml::Table::new())
                .unwrap_or_else(|e| panic!("{}: {e:#}", kind.name()));
        }
    }

    #[test]
    fn a_target_checks_its_own_settings_and_refuses_what_it_does_not_know() {
        let docker = find("docker").unwrap();
        docker
            .build(wiring(), &table("context = \"colima\""))
            .unwrap();
        let err = docker
            .build(wiring(), &table("contxt = \"x\""))
            .err()
            .unwrap();
        let text = format!("{err:#}");
        assert!(
            text.contains("[targets.docker]") && text.contains("contxt"),
            "{text}"
        );
        let host = find("host").unwrap();
        let err = host.build(wiring(), &table("anything = 1")).err().unwrap();
        assert!(err.to_string().contains("no setting `anything`"), "{err}");
    }

    #[test]
    fn a_project_names_its_pod_over_the_users_and_keeps_the_rest() {
        let kube = find("kubernetes").unwrap();
        assert!(kube.per_project() && !find("docker").unwrap().per_project());
        let user = table("selector = \"app=api\"\nnamespace = \"work\"\ncontext = \"staging\"");
        let project = table("pod = \"dev-0\"\nnamespace = \"mine\"");
        let merged = kube.merge_project(&user, &project);
        assert_eq!(
            merged,
            table("pod = \"dev-0\"\nnamespace = \"mine\"\ncontext = \"staging\"")
        );
        // Both are accepted by the target as one pod.
        kube.build(wiring(), &merged).unwrap();
        // Where the project says nothing, the user's table stands.
        assert_eq!(kube.merge_project(&user, &toml::Table::new()), user);
    }

    #[test]
    fn the_engines_a_target_talks_to_come_from_its_settings() {
        // A docker context ends up as `--context` on every command it runs.
        let host = Arc::new(FakeHost::new());
        let with = Wiring {
            host: host.clone(),
            ..wiring()
        };
        let endpoints = find("docker")
            .unwrap()
            .build(with, &table("context = \"colima\""))
            .unwrap();
        endpoints.engine().unwrap().stop("c").unwrap();
        assert_eq!(
            host.argvs()[0],
            ["docker", "--context", "colima", "stop", "c"]
        );
    }
}
