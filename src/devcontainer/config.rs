//! Parsing `devcontainer.json` into a backend-agnostic [`Spec`].
//!
//! Handles config discovery, JSONC parsing, `${localEnv:…}`-style variable
//! substitution, port-forward entries, and the stable container/session names
//! derived from the project path.

use crate::output::warning;
use anyhow::{Context, Result, bail};
use json_comments::StripComments;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::backend::{
    Backend, ComposeBackend, ContainerBackend, CreateSettings, ImageSource, Mount, build_run_args,
    parse_app_ports,
};
use super::lifecycle::{Cmd, Lifecycle};
use super::vars::{Vars, path_tag};
use crate::port::PortForward;
use crate::shell::slug;
use crate::target::Target;

/// A parsed devcontainer.json, grouped by what the fields are for.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevcontainerConfig {
    // Human-readable label, used (when present) for the tmux session name.
    name: Option<String>,
    workspace_folder: Option<String>,
    forward_ports: Option<Vec<serde_json::Value>>,
    features: Option<serde_json::Map<String, serde_json::Value>>,
    override_feature_install_order: Option<Vec<String>>,
    container_env: Option<BTreeMap<String, String>>,
    remote_env: Option<BTreeMap<String, String>>,
    // The user VSCode (and lodge) connects as inside the container. Overrides
    // the image's default CMD user, which is often root even when a non-root
    // user exists in the image (e.g. mcr.microsoft.com/devcontainers/* images).
    remote_user: Option<String>,
    // The user the container's processes run as (`docker run -u`). Also the
    // default for `remoteUser`.
    container_user: Option<String>,
    // Linux: line the container user's uid/gid up with the host user's so
    // bind-mounted files keep sensible ownership. Default true. The spec spells
    // it with an upper-case UID, which `camelCase` would not produce.
    #[serde(rename = "updateRemoteUserUID")]
    update_remote_user_uid: Option<bool>,
    #[serde(flatten)]
    source: SourceConfig,
    #[serde(flatten)]
    create: CreateConfig,
    #[serde(flatten)]
    hooks: HookConfig,
}

/// Where the container comes from: a compose service, an image, or a build.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceConfig {
    // dockerComposeFile backend.
    docker_compose_file: Option<serde_json::Value>,
    service: Option<String>,
    // Compose: bring up only these services (plus `service`).
    run_services: Option<Vec<String>>,
    // image / dockerFile backend.
    image: Option<String>,
    build: Option<BuildConfig>,
    docker_file: Option<String>,
    context: Option<String>,
}

/// How a plain (non-compose) container is created. With compose these belong in
/// the compose file.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateConfig {
    run_args: Option<Vec<String>>,
    workspace_mount: Option<String>,
    mounts: Option<Vec<serde_json::Value>>,
    privileged: Option<bool>,
    init: Option<bool>,
    cap_add: Option<Vec<String>>,
    security_opt: Option<Vec<String>>,
    // Default true: keep the container alive with `sleep infinity` rather than
    // running the image's own command.
    override_command: Option<bool>,
    // Ports to publish: a port, a "host:container" string, or an array of them.
    app_port: Option<serde_json::Value>,
}

/// The lifecycle hooks. Each may be a string ("sh -c"), an array (argv), or an
/// object of named commands. Absent fields deserialize to None.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookConfig {
    initialize_command: Option<Cmd>,
    on_create_command: Option<Cmd>,
    update_content_command: Option<Cmd>,
    post_create_command: Option<Cmd>,
    post_start_command: Option<Cmd>,
    post_attach_command: Option<Cmd>,
}

impl HookConfig {
    fn into_lifecycle(self) -> Lifecycle {
        Lifecycle {
            initialize: self.initialize_command,
            on_create: self.on_create_command,
            update_content: self.update_content_command,
            post_create: self.post_create_command,
            post_start: self.post_start_command,
            post_attach: self.post_attach_command,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildConfig {
    dockerfile: Option<String>,
    context: Option<String>,
    target: Option<String>,
    #[serde(default)]
    args: BTreeMap<String, String>,
}

// The backend-agnostic parts of a parsed devcontainer.
pub(super) struct Spec {
    pub(super) config_path: PathBuf,
    // Directory the devcontainer.json lives in: hashed for change detection,
    // used as cwd for initializeCommand, and as the base for local feature refs.
    pub(super) base_dir: PathBuf,
    // Extra files (besides the .devcontainer dir) to fold into the hash.
    pub(super) hash_extra: Vec<PathBuf>,
    pub(super) workspace_folder: String,
    // Human-readable tmux session name (e.g. "lodge-myproject-1a2b3c").
    pub(super) session_name: String,
    pub(super) forward_ports: Vec<PortForward>,
    pub(super) lifecycle: Lifecycle,
    pub(super) features: Vec<super::features::FeatureRequest>,
    pub(super) override_feature_order: Vec<String>,
    // Merged containerEnv + remoteEnv (remoteEnv wins on conflict).
    pub(super) env: BTreeMap<String, String>,
    pub(super) remote_user: Option<String>,
    // Whether to line the container user's uid up with the host user's.
    pub(super) sync_uid: bool,
    pub(super) is_compose: bool,
    pub(super) backend: Box<dyn Backend>,
}

pub(super) fn load_spec(project_path: &Path, target: &Target) -> Result<Spec> {
    let (config_path, raw) = find_config(project_path, target)?;
    build_spec(parse_config(&raw)?, config_path, project_path)
}

fn parse_config(raw: &str) -> Result<DevcontainerConfig> {
    serde_json::from_reader(StripComments::new(raw.as_bytes())).context("parsing devcontainer.json")
}

/// Turns a parsed config into the backend-agnostic [`Spec`]. Pure: it touches no
/// file and no container, so it can be tested on its own.
fn build_spec(
    config: DevcontainerConfig,
    config_path: PathBuf,
    project_path: &Path,
) -> Result<Spec> {
    let DevcontainerConfig {
        name,
        workspace_folder,
        forward_ports,
        features,
        override_feature_install_order,
        container_env,
        remote_env,
        remote_user,
        container_user,
        update_remote_user_uid,
        source,
        create,
        hooks,
    } = config;

    let base_dir = config_path.parent().unwrap_or(project_path).to_path_buf();
    let basename = project_basename(project_path);
    let workspace_folder = workspace_folder
        .map(|wf| Vars::new(project_path).apply(&wf))
        .unwrap_or_else(|| format!("/workspaces/{basename}"));
    // From here on `${containerWorkspaceFolder}` can be resolved too.
    let vars = Vars::new(project_path).with_container_workspace(&workspace_folder);

    let built = build_backend(
        source,
        create,
        container_user.clone(),
        &BackendContext {
            project_path,
            base_dir: &base_dir,
            workspace_folder: &workspace_folder,
            vars: &vars,
        },
    )?;
    let env = build_env(container_env, remote_env, &vars);
    let session_name = session_name(name.as_deref(), &basename, project_path);

    Ok(Spec {
        config_path,
        base_dir,
        hash_extra: built.hash_extra,
        workspace_folder,
        session_name,
        forward_ports: parse_forward_ports(forward_ports),
        lifecycle: hooks.into_lifecycle(),
        features: feature_requests(features),
        override_feature_order: override_feature_install_order.unwrap_or_default(),
        env,
        // remoteUser defaults to containerUser.
        remote_user: remote_user.or(container_user),
        sync_uid: update_remote_user_uid.unwrap_or(true) && !built.is_compose,
        is_compose: built.is_compose,
        backend: built.backend,
    })
}

fn project_basename(project_path: &Path) -> String {
    project_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string())
}

fn feature_requests(
    features: Option<serde_json::Map<String, serde_json::Value>>,
) -> Vec<super::features::FeatureRequest> {
    features
        .unwrap_or_default()
        .into_iter()
        .map(|(reference, opts)| super::features::FeatureRequest {
            reference,
            options: match opts {
                serde_json::Value::Object(m) => m,
                _ => Default::default(),
            },
        })
        .collect()
}

/// containerEnv + remoteEnv, with remoteEnv overriding on conflict. Values are
/// normalized: host-side vars resolved now, ${containerEnv:X} rewritten to ${X}
/// so the container's shell expands it when the profile is sourced.
fn build_env(
    container_env: Option<BTreeMap<String, String>>,
    remote_env: Option<BTreeMap<String, String>>,
    vars: &Vars,
) -> BTreeMap<String, String> {
    container_env
        .into_iter()
        .chain(remote_env)
        .flatten()
        .map(|(k, v)| (k, normalize_env_value(&v, vars)))
        .collect()
}

/// What backend construction needs to know about the project.
struct BackendContext<'a> {
    project_path: &'a Path,
    base_dir: &'a Path,
    workspace_folder: &'a str,
    vars: &'a Vars<'a>,
}

struct BuiltBackend {
    backend: Box<dyn Backend>,
    /// Extra files (besides the `.devcontainer` dir) to fold into the drift hash.
    hash_extra: Vec<PathBuf>,
    is_compose: bool,
}

/// Picks the backend for whichever form the devcontainer uses.
fn build_backend(
    source: SourceConfig,
    create: CreateConfig,
    container_user: Option<String>,
    ctx: &BackendContext,
) -> Result<BuiltBackend> {
    if source.docker_compose_file.is_some() {
        compose_backend(source, ctx)
    } else if source.image.is_some() || source.build.is_some() || source.docker_file.is_some() {
        Ok(container_backend(source, create, container_user, ctx))
    } else {
        bail!("devcontainer.json must set one of: dockerComposeFile, image, build, or dockerFile")
    }
}

fn compose_backend(source: SourceConfig, ctx: &BackendContext) -> Result<BuiltBackend> {
    let files = match source.docker_compose_file {
        Some(serde_json::Value::String(s)) => vec![s],
        Some(serde_json::Value::Array(a)) => a
            .into_iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => bail!("unexpected dockerComposeFile format"),
    };
    let service = source
        .service
        .context("devcontainer.json with dockerComposeFile is missing 'service'")?;
    let mut run_services = source.run_services.unwrap_or_default();
    if !run_services.is_empty() && !run_services.contains(&service) {
        run_services.push(service.clone());
    }
    let hash_extra = files.iter().map(|f| ctx.base_dir.join(f)).collect();
    Ok(BuiltBackend {
        backend: Box::new(ComposeBackend {
            dir: ctx.base_dir.to_path_buf(),
            files,
            service,
            run_services,
            // Compose project names must be lowercase.
            project: container_name(ctx.project_path).to_lowercase(),
            project_path: ctx.project_path.to_path_buf(),
        }),
        hash_extra,
        is_compose: true,
    })
}

fn container_backend(
    source: SourceConfig,
    create: CreateConfig,
    container_user: Option<String>,
    ctx: &BackendContext,
) -> BuiltBackend {
    let name = container_name(ctx.project_path);
    let image_source = image_source(source, &name, ctx.base_dir);
    let settings = CreateSettings {
        privileged: create.privileged.unwrap_or(false),
        init: create.init.unwrap_or(false),
        cap_add: create.cap_add.unwrap_or_default(),
        security_opt: create.security_opt.unwrap_or_default(),
        mounts: create
            .mounts
            .unwrap_or_default()
            .iter()
            .filter_map(|m| Mount::parse(m, &|s| ctx.vars.apply(s)))
            .collect(),
        container_user,
        published_ports: parse_app_ports(create.app_port),
    };
    let run_args = build_run_args(
        create.run_args,
        create.workspace_mount,
        &settings,
        ctx.vars,
        ctx.project_path,
        ctx.workspace_folder,
    );
    BuiltBackend {
        backend: Box::new(ContainerBackend {
            name,
            source: image_source,
            run_args,
            override_command: create.override_command.unwrap_or(true),
        }),
        hash_extra: vec![],
        is_compose: false,
    }
}

/// An `image` to run, or a `build` to build (with `dockerFile`/`context` as the
/// older spellings of its fields).
fn image_source(source: SourceConfig, container_name: &str, base_dir: &Path) -> ImageSource {
    if let Some(image) = source.image {
        return ImageSource::Image(image);
    }
    let build = source.build.unwrap_or_default();
    let context = build
        .context
        .or(source.context)
        .unwrap_or_else(|| ".".to_string());
    ImageSource::Build {
        context: base_dir.join(context),
        dockerfile: build.dockerfile.or(source.docker_file),
        target: build.target,
        args: build.args,
        tag: format!("{container_name}-img"),
    }
}

// A stable container name for a non-compose devcontainer: project basename plus
// a short hash of its path, so distinct projects with the same basename don't
// collide.
pub(super) fn container_name(project_path: &Path) -> String {
    let base = slug(&project_basename(project_path));
    format!("lodge-{base}-{}", path_tag(project_path))
}

// A readable tmux session name: the devcontainer's `name` (or the project
// basename when it has none), sanitized for tmux, plus a short path hash so two
// projects that share a label don't land on the same session. We derive this
// from the human label rather than the container id, whose compose form is an
// opaque hash that makes "lodge-<hash>" sessions impossible to tell apart.
fn session_name(name: Option<&str>, basename: &str, project_path: &Path) -> String {
    let label = slug(name.unwrap_or(basename));
    let label = label.trim_matches('-');
    let label = if label.is_empty() { "workspace" } else { label };
    format!("lodge-{label}-{}", path_tag(project_path))
}

// Parses forwardPorts entries. The spec allows an integer (a port of the
// primary container) or a "host:port" string, where host is another host on
// the container network — typically a compose service ("db:5432"). On top of
// that, a numeric prefix ("3000:8080") is read docker -p style as a
// local-port:container-port mapping, since a purely numeric hostname is never
// meaningful. A "/proto" suffix is tolerated and ignored (TCP only).
fn parse_forward_ports(values: Option<Vec<serde_json::Value>>) -> Vec<PortForward> {
    values
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| match v {
            serde_json::Value::Number(n) => n
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .map(PortForward::same),
            serde_json::Value::String(s) => parse_forward_entry(&s),
            _ => None,
        })
        .collect()
}

fn parse_forward_entry(s: &str) -> Option<PortForward> {
    let s = s.split('/').next().unwrap_or(s);
    match s.split_once(':') {
        None => s.parse().ok().map(PortForward::same),
        Some((prefix, port)) => {
            let port: u16 = port.parse().ok()?;
            match prefix.parse::<u16>() {
                Ok(local) => Some(PortForward {
                    local,
                    host: None,
                    port,
                }),
                Err(_) => Some(PortForward {
                    local: port,
                    host: Some(prefix.to_string()),
                    port,
                }),
            }
        }
    }
}

fn normalize_env_value(v: &str, vars: &Vars) -> String {
    vars.apply(v).replace("${containerEnv:", "${")
}

// Devcontainer config lookup, per spec precedence: .devcontainer/devcontainer.json,
// .devcontainer.json, then .devcontainer/<folder>/devcontainer.json (one level
// deep). With several subfolder configs the first (alphabetically) wins, with a
// warning — lodge has no UI to pick one. The files are looked for on the
// project's host, local or remote alike.
fn find_config(project_path: &Path, target: &Target) -> Result<(PathBuf, String)> {
    let fixed = [
        project_path.join(".devcontainer").join("devcontainer.json"),
        project_path.join(".devcontainer.json"),
    ];
    let subfolders = project_path
        .join(".devcontainer")
        .join("*")
        .join("devcontainer.json");
    let patterns = [fixed[0].clone(), fixed[1].clone(), subfolders];
    let found = target.read_first(&patterns)?;
    let (Some(first), Some(bytes)) = (found.paths.first(), found.contents) else {
        let place = target
            .ssh_host()
            .map(|h| format!(" on {h}"))
            .unwrap_or_default();
        bail!(
            "no devcontainer config found in {}{place} (looked for \
             .devcontainer/devcontainer.json, .devcontainer.json, and \
             .devcontainer/*/devcontainer.json)",
            project_path.display()
        );
    };
    if !fixed.contains(first) && found.paths.len() > 1 {
        warning!(
            "multiple devcontainer configs found, using {}",
            first.display()
        );
    }
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8", first.display()))?;
    Ok((first.clone(), text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_is_found_in_spec_order() {
        let tmp = crate::tmpfile::TempDir::new("find-config").unwrap();
        let p = tmp.path();
        assert!(find_config(p, &Target::Local).is_err());
        for f in [
            ".devcontainer/b/devcontainer.json",
            ".devcontainer/a/devcontainer.json",
        ] {
            std::fs::create_dir_all(p.join(f).parent().unwrap()).unwrap();
            std::fs::write(p.join(f), "{}").unwrap();
        }
        // Subfolders: the first alphabetically.
        assert_eq!(
            find_config(p, &Target::Local).unwrap().0,
            p.join(".devcontainer/a/devcontainer.json")
        );
        std::fs::write(p.join(".devcontainer.json"), "{}").unwrap();
        assert_eq!(
            find_config(p, &Target::Local).unwrap().0,
            p.join(".devcontainer.json")
        );
        std::fs::write(p.join(".devcontainer/devcontainer.json"), "{}").unwrap();
        assert_eq!(
            find_config(p, &Target::Local).unwrap().0,
            p.join(".devcontainer/devcontainer.json")
        );
    }

    #[test]
    fn container_name_is_stable_and_path_scoped() {
        let a = container_name(Path::new("/home/me/proj"));
        assert_eq!(a, container_name(Path::new("/home/me/proj")));
        assert!(a.starts_with("lodge-proj-"), "got {a}");
        // same basename, different path → different name (no collision)
        assert_ne!(a, container_name(Path::new("/elsewhere/proj")));
    }

    #[test]
    fn session_name_prefers_devcontainer_label() {
        let p = Path::new("/home/me/proj");
        // The devcontainer's `name` wins and is sanitized for tmux.
        let s = session_name(Some("My App (dev)"), "proj", p);
        assert!(s.starts_with("lodge-My-App--dev-"), "got {s}");
        // Falls back to the basename when there's no name.
        assert!(session_name(None, "proj", p).starts_with("lodge-proj-"));
        // Same label, different path → different session (no collision).
        assert_ne!(
            session_name(Some("app"), "proj", Path::new("/a/proj")),
            session_name(Some("app"), "proj", Path::new("/b/proj")),
        );
        // An all-punctuation label degrades to a usable placeholder.
        assert!(session_name(Some("***"), "proj", p).starts_with("lodge-workspace-"));
    }

    #[test]
    fn forward_ports_parse_all_spec_forms() {
        let values = serde_json::json!([3000, "4000", "3000:8080", "db:5432", "9000/tcp", false]);
        let parsed = parse_forward_ports(Some(values.as_array().unwrap().clone()));
        assert_eq!(
            parsed,
            vec![
                PortForward::same(3000),
                PortForward::same(4000),
                PortForward {
                    local: 3000,
                    host: None,
                    port: 8080
                },
                PortForward {
                    local: 5432,
                    host: Some("db".into()),
                    port: 5432
                },
                PortForward::same(9000),
            ]
        );
    }

    // ── parsing and building a spec ───────────────────────────────────────

    fn spec_from(json: &str) -> Result<Spec> {
        build_spec(
            parse_config(json)?,
            PathBuf::from("/home/me/proj/.devcontainer/devcontainer.json"),
            Path::new("/home/me/proj"),
        )
    }

    #[test]
    fn fields_are_grouped_by_concern_when_parsed() {
        let c = parse_config(
            r#"{
                // comments are allowed
                "name": "demo",
                "image": "debian",
                "capAdd": ["SYS_PTRACE"],
                "privileged": true,
                "postCreateCommand": "make setup",
                "initializeCommand": ["echo", "hi"],
                "remoteUser": "dev"
            }"#,
        )
        .unwrap();
        assert_eq!(c.name.as_deref(), Some("demo"));
        assert_eq!(c.remote_user.as_deref(), Some("dev"));
        assert_eq!(c.source.image.as_deref(), Some("debian"));
        assert_eq!(
            c.create.cap_add.as_deref(),
            Some(&["SYS_PTRACE".to_string()][..])
        );
        assert_eq!(c.create.privileged, Some(true));
        assert!(matches!(c.hooks.post_create_command, Some(Cmd::Shell(_))));
        assert!(matches!(c.hooks.initialize_command, Some(Cmd::Exec(_))));
        assert!(c.hooks.on_create_command.is_none());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        assert!(
            parse_config(r#"{"image":"x","customizations":{"vscode":{}},"somethingNew":1}"#)
                .is_ok()
        );
    }

    #[test]
    fn a_config_with_no_source_is_rejected_with_the_options() {
        let err = spec_from(r#"{"name":"x"}"#).err().unwrap().to_string();
        assert!(
            err.contains("dockerComposeFile") && err.contains("image"),
            "{err}"
        );
    }

    #[test]
    fn compose_without_a_service_is_rejected() {
        let err = spec_from(r#"{"dockerComposeFile":"c.yml"}"#)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("service"), "{err}");
    }

    #[test]
    fn the_workspace_folder_defaults_to_workspaces_slash_name() {
        let spec = spec_from(r#"{"image":"debian"}"#).unwrap();
        assert_eq!(spec.workspace_folder, "/workspaces/proj");
    }

    #[test]
    fn remote_env_overrides_container_env_and_variables_resolve() {
        let spec = spec_from(
            r#"{
                "image": "debian",
                "workspaceFolder": "/work",
                "containerEnv": {"A": "from-container", "WS": "${containerWorkspaceFolder}/x"},
                "remoteEnv": {"A": "from-remote", "N": "${localEnv:LODGE_UNSET_FOR_TEST:dflt}"}
            }"#,
        )
        .unwrap();
        assert_eq!(spec.env["A"], "from-remote");
        assert_eq!(spec.env["WS"], "/work/x");
        assert_eq!(spec.env["N"], "dflt");
    }

    #[test]
    fn remote_user_defaults_to_container_user() {
        let spec = spec_from(r#"{"image":"debian","containerUser":"dev"}"#).unwrap();
        assert_eq!(spec.remote_user.as_deref(), Some("dev"));
        let spec =
            spec_from(r#"{"image":"debian","containerUser":"dev","remoteUser":"other"}"#).unwrap();
        assert_eq!(spec.remote_user.as_deref(), Some("other"));
    }

    #[test]
    fn uid_sync_applies_to_plain_containers_only_and_can_be_switched_off() {
        assert!(spec_from(r#"{"image":"debian"}"#).unwrap().sync_uid);
        assert!(
            !spec_from(r#"{"image":"debian","updateRemoteUserUID":false}"#)
                .unwrap()
                .sync_uid
        );
        assert!(
            !spec_from(r#"{"dockerComposeFile":"c.yml","service":"app"}"#)
                .unwrap()
                .sync_uid
        );
    }

    #[test]
    fn compose_files_may_be_a_string_or_a_list_and_feed_the_drift_hash() {
        let one = spec_from(r#"{"dockerComposeFile":"c.yml","service":"app"}"#).unwrap();
        assert_eq!(
            one.hash_extra,
            [PathBuf::from("/home/me/proj/.devcontainer/c.yml")]
        );
        let two = spec_from(r#"{"dockerComposeFile":["a.yml","b.yml"],"service":"app"}"#).unwrap();
        assert_eq!(two.hash_extra.len(), 2);
        assert!(
            spec_from(r#"{"image":"debian"}"#)
                .unwrap()
                .hash_extra
                .is_empty()
        );
    }

    #[test]
    fn hooks_features_and_ports_are_carried_into_the_spec() {
        let spec = spec_from(
            r#"{
                "image": "debian",
                "postStartCommand": "echo up",
                "forwardPorts": [3000],
                "features": {"./feat": {"x": 1}, "ghcr.io/a/b:1": {}},
                "overrideFeatureInstallOrder": ["./feat"]
            }"#,
        )
        .unwrap();
        assert!(spec.lifecycle.post_start.is_some() && spec.lifecycle.on_create.is_none());
        assert_eq!(spec.forward_ports, [PortForward::same(3000)]);
        assert_eq!(spec.features.len(), 2);
        assert_eq!(spec.override_feature_order, ["./feat"]);
    }

    #[test]
    fn a_dockerfile_build_resolves_relative_to_the_config_directory() {
        let source = SourceConfig {
            docker_file: Some("Dockerfile".into()),
            context: Some("..".into()),
            ..Default::default()
        };
        match image_source(source, "lodge-proj-1", Path::new("/p/.devcontainer")) {
            ImageSource::Build {
                context,
                dockerfile,
                tag,
                ..
            } => {
                assert_eq!(context, PathBuf::from("/p/.devcontainer/.."));
                assert_eq!(dockerfile.as_deref(), Some("Dockerfile"));
                assert_eq!(tag, "lodge-proj-1-img");
            }
            ImageSource::Image(_) => panic!("expected a build"),
        }
    }
}
