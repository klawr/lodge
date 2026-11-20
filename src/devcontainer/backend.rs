//! How a devcontainer's container is brought up.
//!
//! Two backends: compose (for `dockerComposeFile`) and a plain run of an `image`
//! or a locally built `Dockerfile`. Both are plain data; the engine they act on
//! is passed in. Everything after the container exists is shared and works off
//! its id.

use crate::output::info;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::vars::Vars;
use crate::runtime::{BuildSpec, Runtime};

// Brings a devcontainer's container up. Each backend only has to know how to
// find an existing container and create/start one; everything after that —
// hashing, env, features, lifecycle hooks — is shared and works off the id.
pub(super) trait Backend {
    /// Id of an existing container for this devcontainer (running or stopped),
    /// or None if it hasn't been created yet.
    fn existing(&self, rt: &dyn Runtime) -> Result<Option<String>>;
    /// Brings the container up, recreating it from scratch when `recreate` is
    /// set, applying any feature `flags` at creation, and returns its id.
    fn up(
        &self,
        rt: &dyn Runtime,
        recreate: bool,
        flags: &crate::features::CreateFlags,
    ) -> Result<String>;
    /// Removes the container (and, for compose, the project's network).
    fn remove(&self, rt: &dyn Runtime) -> Result<()>;
}

pub(super) struct ComposeBackend {
    pub(super) dir: PathBuf,
    pub(super) files: Vec<String>,
    pub(super) service: String,
    // Explicit compose project name (-p). Without it, compose derives the
    // project from the compose file's directory — which is ".devcontainer"
    // for most devcontainers, so *every* such project on a daemon would share
    // one project name and lodge could adopt a different project's container.
    pub(super) project: String,
    // `runServices`: bring up only these (always including `service`). Empty
    // means every service in the compose file.
    pub(super) run_services: Vec<String>,
}

impl Backend for ComposeBackend {
    fn remove(&self, rt: &dyn Runtime) -> Result<()> {
        rt.compose_down(&self.dir, &self.project, &self.files)
    }

    fn existing(&self, rt: &dyn Runtime) -> Result<Option<String>> {
        let id = rt.compose_ps(&self.dir, &self.project, &self.files, &self.service, true)?;
        Ok((!id.is_empty()).then_some(id))
    }

    fn up(
        &self,
        rt: &dyn Runtime,
        recreate: bool,
        flags: &crate::features::CreateFlags,
    ) -> Result<String> {
        // On recreate, rebuild and force a fresh container; otherwise keep the
        // existing one (and stop compose recreating it on its own config-hash).
        let extra: &[&str] = if recreate {
            &["--build", "--force-recreate"]
        } else {
            &["--no-recreate"]
        };

        // Feature create-flags go in a temporary compose override, appended as
        // the last `-f` so it layers over the project's files without changing
        // the project identity (the first file still sets the project dir).
        // Compose reads it client-side, so with a remote target it is written on
        // the remote host (where compose runs).
        let mut files = self.files.clone();
        let override_file = if flags.is_empty() {
            None
        } else {
            let content = flags.compose_override(&self.service);
            let path = rt.target().scratch_path("compose-override.yml");
            rt.target()
                .write_file(&path, &content)
                .context("writing compose override")?;
            files.push(path.clone());
            Some(path)
        };

        let mut up_args: Vec<&str> = extra.to_vec();
        up_args.extend(self.run_services.iter().map(String::as_str));
        let result = rt.compose_up(&self.dir, &self.project, &files, &up_args);
        if let Some(p) = &override_file {
            rt.target().remove_file(p);
        }
        result?;

        let id = rt.compose_ps(&self.dir, &self.project, &self.files, &self.service, false)?;
        if id.is_empty() {
            bail!(
                "could not find running container for service '{}'",
                self.service
            );
        }
        Ok(id)
    }
}

pub(super) enum ImageSource {
    Image(String),
    Build {
        context: PathBuf,
        dockerfile: Option<String>,
        target: Option<String>,
        args: BTreeMap<String, String>,
        tag: String,
    },
}

// A plain (non-compose) devcontainer: an `image` to run or a `build` to build,
// run detached as `sleep infinity` with the workspace mounted, under a stable
// per-project name so lodge can find/recreate it.
pub(super) struct ContainerBackend {
    pub(super) name: String,
    pub(super) source: ImageSource,
    pub(super) run_args: Vec<String>,
    // `overrideCommand` (default true): keep the container alive with
    // `sleep infinity` instead of running the image's own command.
    pub(super) override_command: bool,
}

impl Backend for ContainerBackend {
    fn remove(&self, rt: &dyn Runtime) -> Result<()> {
        rt.remove(&self.name)
    }

    fn existing(&self, rt: &dyn Runtime) -> Result<Option<String>> {
        rt.find_by_name(&self.name)
    }

    fn up(
        &self,
        rt: &dyn Runtime,
        recreate: bool,
        flags: &crate::features::CreateFlags,
    ) -> Result<String> {
        let exists = self.existing(rt)?.is_some();
        if exists && !recreate {
            info!("starting {}", self.name);
            rt.start(&self.name)
                .context("starting the existing container")?;
            return Ok(self.name.clone());
        }
        if exists {
            rt.remove(&self.name)
                .context("removing the old container before recreating it")?;
        }

        let image = self.resolve_image(rt)?;
        info!("creating container {}", self.name);
        let mut run_args = self.run_args.clone();
        run_args.extend(flags.run_args());
        let command: &[&str] = if self.override_command {
            &["sleep", "infinity"]
        } else {
            &[]
        };
        rt.run_detached(&self.name, &run_args, &image, command)
            .context("creating container")?;
        Ok(self.name.clone())
    }
}

impl ContainerBackend {
    fn resolve_image(&self, rt: &dyn Runtime) -> Result<String> {
        match &self.source {
            ImageSource::Image(img) => Ok(img.clone()),
            ImageSource::Build {
                context,
                dockerfile,
                target,
                args,
                tag,
            } => {
                info!("building image {tag}");
                rt.build_image(&BuildSpec {
                    context: context.clone(),
                    dockerfile: dockerfile.clone(),
                    target: target.clone(),
                    args: args.clone(),
                    tag: tag.clone(),
                })?;
                Ok(tag.clone())
            }
        }
    }
}

/// Container-creation settings that devcontainer.json states directly. They
/// apply to the image/Dockerfile backend only; with compose the compose file is
/// where these are configured.
#[derive(Debug, Default)]
pub(super) struct CreateSettings {
    pub(super) privileged: bool,
    pub(super) init: bool,
    pub(super) cap_add: Vec<String>,
    pub(super) security_opt: Vec<String>,
    pub(super) container_user: Option<String>,
    /// `-p` values, from `appPort`.
    pub(super) published_ports: Vec<String>,
}

impl CreateSettings {
    /// The `docker run` flags these settings stand for.
    pub(super) fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.privileged {
            args.push("--privileged".into());
        }
        if self.init {
            args.push("--init".into());
        }
        for cap in &self.cap_add {
            args.extend(["--cap-add".into(), cap.clone()]);
        }
        for opt in &self.security_opt {
            args.extend(["--security-opt".into(), opt.clone()]);
        }
        if let Some(user) = &self.container_user {
            args.extend(["-u".into(), user.clone()]);
        }
        for port in &self.published_ports {
            args.extend(["-p".into(), port.clone()]);
        }
        args
    }
}

// Parses `appPort`: a port, a "host:container" string, or an array of either.
// Published on the loopback interface only, so a devcontainer's ports are not
// exposed to the whole network.
pub(super) fn parse_app_ports(value: Option<serde_json::Value>) -> Vec<String> {
    let items = match value {
        Some(serde_json::Value::Array(a)) => a,
        Some(v) => vec![v],
        None => vec![],
    };
    items
        .into_iter()
        .filter_map(|v| match v {
            serde_json::Value::Number(n) => n.as_u64().map(|n| format!("127.0.0.1:{n}:{n}")),
            serde_json::Value::String(s) => Some(publish_spec(&s)),
            _ => None,
        })
        .collect()
}

fn publish_spec(s: &str) -> String {
    match s.split(':').count() {
        // "3000": the same port on both sides.
        1 => format!("127.0.0.1:{s}:{s}"),
        // "3000:8080": host:container. Anything longer already names an address.
        2 => format!("127.0.0.1:{s}"),
        _ => s.to_string(),
    }
}

// Builds the `docker run` arguments for a ContainerBackend: the workspace mount
// (or a default bind), the working dir, any extra `mounts`, the settings that
// devcontainer.json states directly, and finally the user's `runArgs` (last, so
// they can override).
pub(super) fn build_run_args(
    user_args: Option<Vec<String>>,
    workspace_mount: Option<String>,
    mounts: Option<Vec<serde_json::Value>>,
    settings: &CreateSettings,
    vars: &Vars,
    project_path: &Path,
    workspace_folder: &str,
) -> Vec<String> {
    let mut args = Vec::new();
    match workspace_mount {
        Some(m) => {
            args.push("--mount".into());
            args.push(vars.apply(&m));
        }
        None => {
            args.push("-v".into());
            args.push(format!("{}:{}", project_path.display(), workspace_folder));
        }
    }
    args.push("-w".into());
    args.push(workspace_folder.to_string());

    for m in mounts.unwrap_or_default() {
        if let Some(spec) = mount_to_arg(&m, vars) {
            args.push("--mount".into());
            args.push(spec);
        }
    }
    args.extend(settings.to_args());
    args.extend(
        user_args
            .unwrap_or_default()
            .into_iter()
            .map(|a| vars.apply(&a)),
    );
    args
}

// A devcontainer `mounts` entry → a `--mount` value (string or {source,target,type}).
fn mount_to_arg(m: &serde_json::Value, vars: &Vars) -> Option<String> {
    match m {
        serde_json::Value::String(s) => Some(vars.apply(s)),
        serde_json::Value::Object(o) => {
            let mut parts = Vec::new();
            if let Some(t) = o.get("type").and_then(|v| v.as_str()) {
                parts.push(format!("type={t}"));
            }
            if let Some(s) = o.get("source").and_then(|v| v.as_str()) {
                parts.push(format!("source={}", vars.apply(s)));
            }
            if let Some(t) = o.get("target").and_then(|v| v.as_str()) {
                parts.push(format!("target={}", vars.apply(t)));
            }
            (!parts.is_empty()).then(|| parts.join(","))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_to_arg_handles_string_and_object() {
        let p = Path::new("/home/me/proj");
        let vars = Vars::new(p);
        assert_eq!(
            mount_to_arg(&serde_json::json!("type=bind,source=/a,target=/b"), &vars).unwrap(),
            "type=bind,source=/a,target=/b"
        );
        let obj = serde_json::json!({"type": "volume", "source": "vol", "target": "/data"});
        assert_eq!(
            mount_to_arg(&obj, &vars).unwrap(),
            "type=volume,source=vol,target=/data"
        );
    }

    #[test]
    fn mounts_resolve_variables_including_the_container_workspace() {
        let p = Path::new("/home/me/proj");
        let vars = Vars::new(p).with_container_workspace("/workspaces/proj");
        let obj = serde_json::json!({
            "type": "bind",
            "source": "${localWorkspaceFolder}/cache",
            "target": "${containerWorkspaceFolder}/.cache"
        });
        assert_eq!(
            mount_to_arg(&obj, &vars).unwrap(),
            "type=bind,source=/home/me/proj/cache,target=/workspaces/proj/.cache"
        );
    }

    #[test]
    fn settings_become_run_flags() {
        let s = CreateSettings {
            privileged: true,
            init: true,
            cap_add: vec!["SYS_PTRACE".into()],
            security_opt: vec!["seccomp=unconfined".into()],
            container_user: Some("dev".into()),
            published_ports: vec!["127.0.0.1:3000:3000".into()],
        };
        assert_eq!(
            s.to_args(),
            [
                "--privileged",
                "--init",
                "--cap-add",
                "SYS_PTRACE",
                "--security-opt",
                "seccomp=unconfined",
                "-u",
                "dev",
                "-p",
                "127.0.0.1:3000:3000"
            ]
        );
        assert!(CreateSettings::default().to_args().is_empty());
    }

    #[test]
    fn app_ports_are_published_on_loopback_only() {
        let ports = parse_app_ports(Some(serde_json::json!([
            3000,
            "4000",
            "8080:80",
            "0.0.0.0:9:9"
        ])));
        assert_eq!(
            ports,
            [
                "127.0.0.1:3000:3000",
                "127.0.0.1:4000:4000",
                "127.0.0.1:8080:80",
                "0.0.0.0:9:9" // an explicit address is the user's decision
            ]
        );
        assert_eq!(
            parse_app_ports(Some(serde_json::json!(5000))),
            ["127.0.0.1:5000:5000"]
        );
        assert!(parse_app_ports(None).is_empty());
    }

    #[test]
    fn user_run_args_come_last_so_they_can_override() {
        let p = Path::new("/home/me/proj");
        let args = build_run_args(
            Some(vec!["--cap-drop=ALL".into()]),
            None,
            None,
            &CreateSettings {
                privileged: true,
                ..Default::default()
            },
            &Vars::new(p),
            p,
            "/workspaces/proj",
        );
        assert_eq!(
            args,
            [
                "-v",
                "/home/me/proj:/workspaces/proj",
                "-w",
                "/workspaces/proj",
                "--privileged",
                "--cap-drop=ALL"
            ]
        );
    }
}
