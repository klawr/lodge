//! Devcontainer *features*: fetch them, order them, apply their creation
//! settings and run their `install.sh` in the container.
//!
//! - [`fetch`]: get a feature's files onto this machine (OCI or a local path)
//!
//! The container-creation settings features ask for (mounts, `privileged`,
//! capabilities, `init`) are gathered into the same
//! [`CreateSettings`] devcontainer.json's own
//! settings use.

mod fetch;

use super::create::{CreateSettings, Mount};
use super::markers;
use crate::host::Host;
use crate::output::{info, warning};
use crate::runtime::{ContainerUser, Env, ExecOpts, User};
use crate::tmpfile::TempDir;
use crate::tools::OciFetcher;
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use fetch::is_local_ref;

use crate::layout::FEATURES as FEATURES_DIR;

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
    /// Features that must be installed too, and before this one: reference to
    /// options. Unlike `installsAfter`, they are pulled in when not requested.
    #[serde(default, rename = "dependsOn")]
    depends_on: BTreeMap<String, Value>,
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
    /// Pulled in by another feature's `dependsOn`, not asked for in the config.
    implicit: bool,
}

/// Where features find the project.
pub struct Context<'a> {
    /// The devcontainer folder, on the project's host; local feature paths are
    /// relative to it.
    pub base_dir: &'a Path,
    /// The project's host, where local features are read from.
    pub host: &'a dyn Host,
    /// Where registry features are pulled from.
    pub oci: &'a dyn OciFetcher,
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
    /// success marker, and writes the feature's `containerEnv` for login
    /// shells. Each feature has its own profile, so installing some features
    /// again (the ones that failed before) leaves the others' env alone.
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
        for r in &self.resolved {
            // A dependency may be there already (it was installed for another
            // feature that is being retried).
            if r.implicit {
                match markers::done(env, &marker_key(&r.reference)) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => {
                        warning!(
                            "skipping feature {}: could not tell whether it is installed: {e:#}",
                            r.reference
                        );
                        continue;
                    }
                }
            }
            warn_unsupported(r);
            validate_options(r);
            if let Err(e) = install_one(env, r, &users) {
                warning!("feature {} install failed (continuing): {e:#}", r.reference);
                continue;
            }
            // Persisted so login shells (lodge enters with `-l`) see it.
            if !r.meta.container_env.is_empty() {
                super::profile::write_profile(
                    env,
                    &crate::layout::profile_feature(&r.meta.id),
                    &r.meta.container_env,
                )?;
            }
            if let Err(e) = markers::mark_done(env, &marker_key(&r.reference)) {
                // Not fatal, but the feature will be installed again next run.
                warning!(
                    "could not record that feature {} is installed: {e:#}",
                    r.reference
                );
            }
        }
        Ok(())
    }
}

/// Fetches and orders the given features (pulling OCI artifacts through
/// `ctx.oci`, reading local-path features from the project's host). Returns an
/// empty plan if there's nothing to do.
pub fn resolve(
    ctx: &Context,
    features: &[FeatureRequest],
    override_order: &[String],
) -> Result<Plan> {
    if features.is_empty() {
        return Ok(Plan::default());
    }

    let work = TempDir::new("features")?;

    // The requested features, then whatever their `dependsOn` adds (and what
    // those add, once each: the config's own entry, with its options, wins).
    let mut queue: std::collections::VecDeque<(FeatureRequest, bool)> =
        features.iter().cloned().map(|f| (f, false)).collect();
    let mut seen: Vec<String> = Vec::new();
    let mut resolved: Vec<Resolved> = Vec::with_capacity(features.len());
    let mut n = 0;
    while let Some((req, implicit)) = queue.pop_front() {
        if seen.iter().any(|s| same_ref(s, &req.reference)) {
            continue;
        }
        seen.push(req.reference.clone());
        info!("fetching feature {}", req.reference);
        n += 1;
        match fetch::fetch(ctx, &req.reference, &work.path().join(n.to_string())) {
            Ok((meta, dir)) => {
                for (dep, options) in &meta.depends_on {
                    queue.push_back((
                        FeatureRequest {
                            reference: dep.clone(),
                            options: options.as_object().cloned().unwrap_or_default(),
                        },
                        true,
                    ));
                }
                resolved.push(Resolved {
                    reference: req.reference,
                    options: req.options,
                    meta,
                    dir,
                    implicit,
                });
            }
            Err(e) => warning!(
                "feature {} could not be fetched (skipping): {e:#}",
                req.reference
            ),
        }
    }
    drop_without_dependencies(&mut resolved);

    Ok(Plan {
        resolved: order(resolved, override_order),
        _work: Some(work),
    })
}

// Removes each feature whose `dependsOn` names one that is not there (it could
// not be fetched), and then whatever depended on that, with a warning each.
fn drop_without_dependencies(resolved: &mut Vec<Resolved>) {
    while let Some(i) = resolved.iter().position(|r| {
        r.meta
            .depends_on
            .keys()
            .any(|d| !resolved.iter().any(|o| same_ref(d, &o.reference)))
    }) {
        let r = resolved.remove(i);
        warning!(
            "skipping feature {}: a feature it depends on is not available",
            r.reference
        );
    }
}

/// The features that haven't been installed successfully in `env` yet: all of
/// them in a new container, the ones that failed before in a reused one. One
/// whose marker can't be checked is left out (with a warning) rather than
/// installed again.
pub fn pending(env: &dyn Env, features: &[FeatureRequest]) -> Vec<FeatureRequest> {
    features
        .iter()
        .filter(|f| match markers::done(env, &marker_key(&f.reference)) {
            Ok(done) => !done,
            Err(e) => {
                warning!(
                    "skipping feature {}: could not tell whether it is installed: {e:#}",
                    f.reference
                );
                false
            }
        })
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
    env.exec(&["rm", "-rf", &dest], &ExecOpts::root().repeatable())?;
    env.cp_in(&r.dir.to_string_lossy(), &dest)?;

    info!("installing feature {}", r.reference);
    let vars = build_env(r, users);
    env.exec(
        &["sh", "-c", "chmod +x ./install.sh && ./install.sh"],
        &ExecOpts {
            user: User::Root,
            workdir: Some(&dest),
            env: &vars,
            // An installer may do anything, so it is not run a second time.
            repeat: crate::host::Repeat::Never,
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

    // deps[i] = indices that must precede i: its installsAfter and dependsOn
    // present in the set.
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ri) in resolved.iter().enumerate() {
        for (j, rj) in resolved.iter().enumerate() {
            if j != i
                && (ri.meta.installs_after.iter().any(|d| matches_ref(d, rj))
                    || ri
                        .meta
                        .depends_on
                        .keys()
                        .any(|d| same_ref(d, &rj.reference)))
            {
                deps[i].push(j);
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
            .or_else(|| {
                let first = idx.iter().copied().find(|&i| !done[i]);
                if let Some(i) = first {
                    warning!(
                        "features that install after each other in a circle; {} goes next",
                        resolved[i].reference
                    );
                }
                first
            });
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

// Whether two references name the same feature, ignoring the tag.
fn same_ref(a: &str, b: &str) -> bool {
    strip_tag(a) == strip_tag(b)
}

// `ghcr.io/x/node:1` without `:1`. A colon before the last `/` is a registry's
// port (`localhost:5000/x`), not a tag.
fn strip_tag(reference: &str) -> &str {
    let name_start = reference.rfind('/').map_or(0, |i| i + 1);
    match reference[name_start..].find(':') {
        Some(i) => &reference[..name_start + i],
        None => reference,
    }
}

// Matches an installsAfter / override entry against a resolved feature, by full
// reference, tag-stripped reference, or the feature's id (last path segment).
fn matches_ref(target: &str, r: &Resolved) -> bool {
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
            implicit: false,
        }
    }

    /// A fetched feature `id` whose metadata sets `containerEnv` to `vars`.
    fn fetched(work: &Path, id: &str, vars: &[(&str, &str)]) -> Resolved {
        let dir = work.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("install.sh"), "true\n").unwrap();
        let mut r = resolved(&format!("ghcr.io/x/{id}:1"), id, &[]);
        r.meta.container_env = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        r.dir = dir;
        r
    }

    fn container() -> crate::runtime::fake::FakeEnv {
        crate::runtime::fake::FakeEnv::new().with_output(
            &["sh", "-c", crate::runtime::PROBE_SCRIPT],
            "0\n0\nroot\n/root\n",
        )
    }

    fn plan(resolved: Vec<Resolved>) -> Plan {
        Plan {
            resolved,
            _work: None,
        }
    }

    #[test]
    fn installing_a_feature_again_keeps_the_others_env() {
        let work = TempDir::new("features-env").unwrap();
        let env = container();
        let a = fetched(work.path(), "a", &[("A_HOME", "/opt/a")]);
        let b = fetched(work.path(), "b", &[("B_HOME", "/opt/b")]);

        // The first run installs `a`; `b` could not be fetched, say.
        plan(vec![a]).install(&env, None).unwrap();
        assert!(env.file("/etc/profile.d/lodge-feature-a.sh").is_some());
        assert!(env.file("/etc/profile.d/lodge-feature-b.sh").is_none());
        let features = [
            FeatureRequest {
                reference: "ghcr.io/x/a:1".into(),
                options: Map::new(),
            },
            FeatureRequest {
                reference: "ghcr.io/x/b:1".into(),
                options: Map::new(),
            },
        ];
        let retried: Vec<_> = pending(&env, &features)
            .into_iter()
            .map(|f| f.reference)
            .collect();
        assert_eq!(retried, ["ghcr.io/x/b:1"], "only the failed one is retried");

        // The retry installs only `b`; `a`'s env stays.
        plan(vec![b]).install(&env, None).unwrap();
        assert_eq!(
            env.file("/etc/profile.d/lodge-feature-a.sh").as_deref(),
            Some("export A_HOME='/opt/a'\n")
        );
        assert_eq!(
            env.file("/etc/profile.d/lodge-feature-b.sh").as_deref(),
            Some("export B_HOME='/opt/b'\n")
        );
        assert!(pending(&env, &features).is_empty());
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

    #[test]
    fn a_registry_port_is_not_a_tag() {
        assert!(same_ref("localhost:5000/x/node:1", "localhost:5000/x/node"));
        assert!(!same_ref("localhost:5000/x/node", "localhost:5001/x/node"));
        assert_eq!(strip_tag("localhost:5000/x/node"), "localhost:5000/x/node");
        assert_eq!(strip_tag("./feat"), "./feat");
    }

    /// A host with local features in `/dc`: name to its `dependsOn` JSON.
    fn local_features(features: &[(&str, &str)]) -> crate::host::fake::FakeHost {
        features.iter().fold(
            crate::host::fake::FakeHost::new(),
            |host, (name, depends_on)| {
                host.with_file(&format!("/dc/{name}/install.sh"), "true\n")
                    .with_file(
                        &format!("/dc/{name}/devcontainer-feature.json"),
                        &format!(r#"{{"id": {name:?}, "dependsOn": {depends_on}}}"#),
                    )
            },
        )
    }

    fn request(reference: &str, options: Value) -> FeatureRequest {
        FeatureRequest {
            reference: reference.to_string(),
            options: options.as_object().cloned().unwrap_or_default(),
        }
    }

    fn resolve_local(host: &crate::host::fake::FakeHost, requested: &[FeatureRequest]) -> Plan {
        let ctx = Context {
            base_dir: Path::new("/dc"),
            host,
            oci: &crate::tools::NoRegistry,
        };
        resolve(&ctx, requested, &[]).unwrap()
    }

    #[test]
    fn a_dependency_nobody_asked_for_is_installed_first_with_its_options() {
        let dir = local_features(&[("app", r#"{"./lib": {"mode": "x"}}"#), ("lib", "{}")]);
        let plan = resolve_local(&dir, &[request("./app", json!({}))]);
        let got: Vec<_> = plan
            .resolved
            .iter()
            .map(|r| (r.meta.id.as_str(), r.implicit))
            .collect();
        assert_eq!(got, [("lib", true), ("app", false)]);
        assert_eq!(plan.resolved[0].options["mode"], "x");
    }

    #[test]
    fn a_dependency_that_is_also_requested_is_installed_once_with_the_users_options() {
        let dir = local_features(&[("app", r#"{"./lib": {"mode": "x"}}"#), ("lib", "{}")]);
        let plan = resolve_local(
            &dir,
            &[
                request("./app", json!({})),
                request("./lib", json!({"mode": "mine"})),
            ],
        );
        assert_eq!(plan.resolved.len(), 2);
        let lib = plan.resolved.iter().find(|r| r.meta.id == "lib").unwrap();
        assert!(!lib.implicit);
        assert_eq!(lib.options["mode"], "mine");
        assert_eq!(plan.resolved[0].meta.id, "lib", "and before its dependent");
    }

    #[test]
    fn dependencies_of_dependencies_come_along_and_a_cycle_ends() {
        let dir = local_features(&[
            ("a", r#"{"./b": {}}"#),
            ("b", r#"{"./c": {}}"#),
            ("c", r#"{"./a": {}}"#),
        ]);
        let plan = resolve_local(&dir, &[request("./a", json!({}))]);
        assert_eq!(plan.resolved.len(), 3);
    }

    #[test]
    fn a_feature_whose_dependency_cannot_be_fetched_is_skipped_with_what_needs_it() {
        let dir = local_features(&[
            ("app", r#"{"./lib": {}}"#),
            ("lib", r#"{"./missing": {}}"#),
            ("plain", "{}"),
        ]);
        let plan = resolve_local(
            &dir,
            &[request("./app", json!({})), request("./plain", json!({}))],
        );
        let ids: Vec<_> = plan.resolved.iter().map(|r| r.meta.id.as_str()).collect();
        assert_eq!(ids, ["plain"]);
    }

    #[test]
    fn an_installed_dependency_is_not_installed_again_but_a_requested_feature_is() {
        let env = container();
        markers::mark_done(&env, &marker_key("ghcr.io/x/lib:1")).unwrap();
        let work = TempDir::new("dep-installed").unwrap();
        let mut lib = fetched(work.path(), "lib", &[]);
        lib.implicit = true;
        let app = fetched(work.path(), "app", &[]);
        plan(vec![lib, app]).install(&env, None).unwrap();
        assert!(!env.ran_matching(|c| c.iter().any(|a| a.contains("/lib"))));
        assert!(env.ran_matching(|c| c.iter().any(|a| a.contains("/app"))));
    }
}
