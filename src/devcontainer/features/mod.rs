//! Devcontainer *features*: fetch them, order them, apply their creation
//! settings and run their `install.sh` in the container.
//!
//! - [`fetch`]: get a feature's files onto this machine (OCI or a local path)
//!
//! The container-creation settings features ask for (mounts, `privileged`,
//! capabilities, `init`) are gathered into the same
//! [`CreateSettings`](super::backend::CreateSettings) devcontainer.json's own
//! settings use.

mod fetch;

use super::backend::{CreateSettings, Mount};
use super::markers;
use crate::output::{info, plural, warning};
use crate::runtime::{ContainerUser, Env, ExecOpts, User};
use crate::target::Target;
use crate::tmpfile::TempDir;
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use fetch::is_local_ref;

use crate::layout::{FEATURES as FEATURES_DIR, PROFILE_FEATURES as PROFILE_PATH};

/// A feature as requested in devcontainer.json: an OCI reference plus the
/// user-supplied options object.
#[derive(Clone)]
pub struct FeatureRequest {
    pub reference: String,
    pub options: Map<String, Value>,
}

/// Relevant subset of a feature's `devcontainer-feature.json` metadata.
#[derive(Debug, Default, Deserialize)]
struct FeatureMeta {
    id: String,
    #[serde(default)]
    options: BTreeMap<String, OptionSpec>,
    #[serde(default, rename = "installsAfter")]
    installs_after: Vec<String>,
    #[serde(default, rename = "containerEnv")]
    container_env: BTreeMap<String, String>,
    // Container *creation* settings — applied to the backend at up time
    // (see `Plan::create_settings`).
    #[serde(default)]
    mounts: Vec<Value>,
    #[serde(default)]
    privileged: bool,
    #[serde(default, rename = "capAdd")]
    cap_add: Vec<String>,
    #[serde(default, rename = "securityOpt")]
    security_opt: Vec<String>,
    #[serde(default)]
    init: bool,
    // entrypoint would replace the container's command; lodge runs `sleep
    // infinity`, so this is warned-and-skipped rather than applied.
    #[serde(default)]
    entrypoint: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OptionSpec {
    #[serde(default)]
    default: Value,
    #[serde(default, rename = "enum")]
    enum_values: Option<Vec<Value>>,
}

struct Resolved {
    reference: String,
    options: Map<String, Value>,
    meta: FeatureMeta,
    dir: PathBuf, // local dir with install.sh + metadata
}

/// Where features find the project.
pub struct Context<'a> {
    /// The devcontainer folder, on the project's host; local feature paths are
    /// relative to it.
    pub base_dir: &'a Path,
    /// The project's host, where local features are read from.
    pub target: &'a Target,
}

/// A user an install script is told about. Falls back to root when the
/// container can't say, so the script still runs.
fn script_user(env: &dyn Env, user: Option<&str>) -> ContainerUser {
    ContainerUser::resolve(env, user).unwrap_or_else(|e| {
        warning!("{e:#}; telling feature scripts the user is root");
        ContainerUser::root()
    })
}

/// A fetched, ordered set of features ready to install. Produced by `resolve`
/// (which does the network/disk work up front, so creation settings are known
/// before the container is brought up) and consumed by `install`.
#[derive(Default)]
pub struct Plan {
    resolved: Vec<Resolved>,
    /// Where the features' files were fetched to; removed with the plan.
    _work: Option<TempDir>,
}

impl Plan {
    /// The container-creation settings these features require.
    pub fn create_settings(&self) -> CreateSettings {
        let mut settings = CreateSettings::default();
        for r in &self.resolved {
            let m = &r.meta;
            settings.merge(CreateSettings {
                privileged: m.privileged,
                init: m.init,
                cap_add: m.cap_add.clone(),
                security_opt: m.security_opt.clone(),
                mounts: m
                    .mounts
                    .iter()
                    .filter_map(|v| Mount::parse(v, &str::to_string))
                    .collect(),
                ..CreateSettings::default()
            });
        }
        settings
    }

    /// Runs each feature's `install.sh` (as root, options as env), records a
    /// success marker, and persists merged `containerEnv` for login shells.
    pub fn install(&self, env: &dyn Env, remote_user: Option<&str>) -> Result<()> {
        if self.resolved.is_empty() {
            return Ok(());
        }
        if let Err(e) = env.mkdir_p(&[FEATURES_DIR]) {
            // Like a failing install script: reported, not fatal.
            warning!("cannot install devcontainer features (continuing without them): {e:#}");
            return Ok(());
        }
        let users = [script_user(env, remote_user), script_user(env, None)];
        let mut container_env: BTreeMap<String, String> = BTreeMap::new();
        for r in &self.resolved {
            warn_unsupported(r);
            validate_options(r);
            match install_one(env, r, &users) {
                Ok(()) => {
                    for (k, v) in &r.meta.container_env {
                        container_env.insert(k.clone(), v.clone());
                    }
                    if let Err(e) = markers::mark_done(env, &marker_key(&r.reference)) {
                        // Not fatal, but the feature will be installed again next run.
                        warning!(
                            "could not record that feature {} is installed: {e:#}",
                            r.reference
                        );
                    }
                }
                Err(e) => warning!("feature {} install failed (continuing): {e}", r.reference),
            }
        }
        // Persisted so login shells (lodge enters with `-l`) see it.
        if !container_env.is_empty() {
            super::profile::write_profile(env, PROFILE_PATH, &container_env)?;
        }
        Ok(())
    }
}

/// Fetches and orders the given features (pulling OCI artifacts with `oras`,
/// reading local-path features from the project's host). Returns an empty plan
/// if there's nothing to do or `oras` is needed but missing.
pub fn resolve(
    ctx: &Context,
    features: &[FeatureRequest],
    override_order: &[String],
) -> Result<Plan> {
    if features.is_empty() {
        return Ok(Plan::default());
    }
    if features.iter().any(|f| !is_local_ref(&f.reference))
        && crate::host_tools::ORAS.find().is_none()
    {
        warning!(
            "devcontainer features need `oras` (https://oras.land); skipping {}",
            plural(features.len(), "feature", "features")
        );
        return Ok(Plan::default());
    }

    let work = TempDir::new("features")?;

    let mut resolved = Vec::with_capacity(features.len());
    for (i, req) in features.iter().enumerate() {
        info!("fetching feature {}", req.reference);
        match fetch::fetch(ctx, &req.reference, &work.path().join(i.to_string())) {
            Ok((meta, dir)) => resolved.push(Resolved {
                reference: req.reference.clone(),
                options: req.options.clone(),
                meta,
                dir,
            }),
            Err(e) => warning!(
                "feature {} could not be fetched (skipping): {e:#}",
                req.reference
            ),
        }
    }

    Ok(Plan {
        resolved: order(resolved, override_order),
        _work: Some(work),
    })
}

/// The features that haven't been installed successfully in `env` yet: all of
/// them in a new container, the ones that failed before in a reused one.
pub fn pending(env: &dyn Env, features: &[FeatureRequest]) -> Vec<FeatureRequest> {
    features
        .iter()
        .filter(|f| !markers::done(env, &marker_key(&f.reference)))
        .cloned()
        .collect()
}

fn marker_key(reference: &str) -> String {
    format!("feature.{reference}")
}

// Warns (but doesn't fail) when a provided option value isn't one of the
// feature's declared enum values.
fn validate_options(r: &Resolved) {
    for (id, value) in &r.options {
        if let Some(spec) = r.meta.options.get(id)
            && let Some(allowed) = &spec.enum_values
            && !allowed.contains(value)
        {
            warning!(
                "feature {} option {id}={} is not one of its declared values",
                r.reference,
                value
            );
        }
    }
}

// Copies a feature into the container and runs its install.sh as root.
fn install_one(env: &dyn Env, r: &Resolved, users: &[ContainerUser; 2]) -> Result<()> {
    let dest = format!("{FEATURES_DIR}/{}", r.meta.id);
    env.exec_root(&["rm", "-rf", &dest])?;
    env.cp_in(&r.dir.to_string_lossy(), &dest)?;

    info!("installing feature {}", r.reference);
    let vars = build_env(r, users);
    env.exec(
        &["sh", "-c", "chmod +x ./install.sh && ./install.sh"],
        &ExecOpts {
            user: User::Root,
            workdir: Some(&dest),
            env: &vars,
        },
    )
}

fn warn_unsupported(r: &Resolved) {
    if r.meta.entrypoint.is_some() {
        warning!(
            "feature {} declares an entrypoint, which lodge can't apply \
             (the container runs `sleep infinity`); ignoring it",
            r.reference
        );
    }
}

// Builds the env for install.sh: each declared option as an uppercased var
// (user value or its default), plus the `_REMOTE_USER` / `_CONTAINER_USER`
// variables (and their homes) that scripts use to set up the right user.
fn build_env(r: &Resolved, [remote, container]: &[ContainerUser; 2]) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for (id, spec) in &r.meta.options {
        let value = r.options.get(id).unwrap_or(&spec.default);
        env.push((env_name(id), stringify(value)));
    }
    for (k, v) in [
        ("_REMOTE_USER", &remote.name),
        ("_REMOTE_USER_HOME", &remote.home),
        ("_CONTAINER_USER", &container.name),
        ("_CONTAINER_USER_HOME", &container.home),
    ] {
        env.push((k.to_string(), v.clone()));
    }
    env
}

fn env_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// Orders features so that any `installsAfter` dependency that is *also* in this
// set comes first. Ties break by overrideFeatureInstallOrder, then reference —
// a deterministic topological sort (n is tiny, so an O(n²) scan is fine).
fn order(resolved: Vec<Resolved>, override_order: &[String]) -> Vec<Resolved> {
    let n = resolved.len();
    let rank = |r: &Resolved| {
        override_order
            .iter()
            .position(|o| matches_ref(o, r))
            .unwrap_or(usize::MAX)
    };

    // Stable base order: override rank, then reference.
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| {
        rank(&resolved[a])
            .cmp(&rank(&resolved[b]))
            .then(resolved[a].reference.cmp(&resolved[b].reference))
    });

    // deps[i] = indices that must precede i (its installsAfter present in the set).
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ri) in resolved.iter().enumerate() {
        for dep in &ri.meta.installs_after {
            for (j, rj) in resolved.iter().enumerate() {
                if j != i && matches_ref(dep, rj) {
                    deps[i].push(j);
                }
            }
        }
    }

    let mut done = vec![false; n];
    let mut out: Vec<usize> = Vec::with_capacity(n);
    for _ in 0..n {
        // pick the first (by base order) node whose deps are all satisfied;
        // on a cycle, fall back to the first remaining node.
        let pick = idx
            .iter()
            .copied()
            .find(|&i| !done[i] && deps[i].iter().all(|&d| done[d]))
            .or_else(|| idx.iter().copied().find(|&i| !done[i]));
        if let Some(i) = pick {
            done[i] = true;
            out.push(i);
        }
    }

    let mut slots: Vec<Option<Resolved>> = resolved.into_iter().map(Some).collect();
    // `out` holds each index exactly once, so every slot is taken exactly once.
    out.into_iter()
        .filter_map(|i| slots.get_mut(i).and_then(Option::take))
        .collect()
}

// Matches an installsAfter / override entry against a resolved feature, by full
// reference, tag-stripped reference, or the feature's id (last path segment).
fn matches_ref(target: &str, r: &Resolved) -> bool {
    let strip_tag = |s: &str| s.rsplit_once(':').map(|(a, _)| a).unwrap_or(s).to_string();
    let t = strip_tag(target);
    if t == strip_tag(&r.reference) {
        return true;
    }
    t.rsplit('/').next() == Some(r.meta.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resolved(reference: &str, id: &str, installs_after: &[&str]) -> Resolved {
        Resolved {
            reference: reference.to_string(),
            options: Map::new(),
            meta: FeatureMeta {
                id: id.to_string(),
                installs_after: installs_after.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            dir: PathBuf::new(),
        }
    }

    #[test]
    fn env_name_uppercases_and_sanitizes() {
        assert_eq!(env_name("nodeGypDependencies"), "NODEGYPDEPENDENCIES");
        assert_eq!(env_name("install-yarn"), "INSTALL_YARN");
        assert_eq!(env_name("a.b"), "A_B");
    }

    #[test]
    fn stringify_each_kind() {
        assert_eq!(stringify(&json!("lts")), "lts"); // no surrounding quotes
        assert_eq!(stringify(&json!(true)), "true");
        assert_eq!(stringify(&json!(20)), "20");
        assert_eq!(stringify(&json!(null)), "");
    }

    #[test]
    fn matches_ref_by_ref_tag_and_id() {
        let r = resolved("ghcr.io/devcontainers/features/node:1", "node", &[]);
        assert!(matches_ref("ghcr.io/devcontainers/features/node:1", &r));
        assert!(matches_ref("ghcr.io/devcontainers/features/node", &r)); // tag stripped
        assert!(matches_ref("node", &r)); // by id
        assert!(!matches_ref("ghcr.io/devcontainers/features/go", &r));
    }

    #[test]
    fn order_puts_installs_after_dependency_first() {
        // node installsAfter common-utils; input deliberately in the wrong order.
        let v = vec![
            resolved("ghcr.io/x/node:1", "node", &["ghcr.io/x/common-utils"]),
            resolved("ghcr.io/x/common-utils:1", "common-utils", &[]),
        ];
        let ids: Vec<_> = order(v, &[]).into_iter().map(|r| r.meta.id).collect();
        assert_eq!(ids, vec!["common-utils", "node"]);
    }

    #[test]
    fn order_respects_override_then_reference() {
        let v = vec![
            resolved("ghcr.io/x/a:1", "a", &[]),
            resolved("ghcr.io/x/b:1", "b", &[]),
        ];
        let ids: Vec<_> = order(v, &["ghcr.io/x/b".to_string()])
            .into_iter()
            .map(|r| r.meta.id)
            .collect();
        assert_eq!(ids, vec!["b", "a"]); // b promoted by override order
    }

    #[test]
    fn order_does_not_hang_on_cycle() {
        let v = vec![
            resolved("ghcr.io/x/a:1", "a", &["ghcr.io/x/b"]),
            resolved("ghcr.io/x/b:1", "b", &["ghcr.io/x/a"]),
        ];
        assert_eq!(order(v, &[]).len(), 2); // both emitted, no panic/hang
    }
}
