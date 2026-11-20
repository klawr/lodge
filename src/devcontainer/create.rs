//! How a devcontainer's container is created: what devcontainer.json states
//! directly (privileged, capabilities, mounts, published ports, …) and what
//! features ask for, rendered as `docker run` flags or as a compose override.

/// How a container is created: what devcontainer.json states directly (for the
/// image/Dockerfile backend; with compose these belong in the compose file),
/// and what features ask for (either backend).
#[derive(Debug, Default)]
pub(super) struct CreateSettings {
    pub(super) privileged: bool,
    pub(super) init: bool,
    pub(super) cap_add: Vec<String>,
    pub(super) security_opt: Vec<String>,
    pub(super) mounts: Vec<Mount>,
    pub(super) container_user: Option<String>,
    /// `-p` values, from `appPort`.
    pub(super) published_ports: Vec<String>,
}

impl CreateSettings {
    /// Adds `other`'s settings to these.
    pub(super) fn merge(&mut self, other: CreateSettings) {
        self.privileged |= other.privileged;
        self.init |= other.init;
        self.cap_add.extend(other.cap_add);
        self.security_opt.extend(other.security_opt);
        self.mounts.extend(other.mounts);
        self.container_user = other.container_user.or(self.container_user.take());
        self.published_ports.extend(other.published_ports);
    }

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
        for m in &self.mounts {
            args.extend(["--mount".into(), m.spec.clone()]);
        }
        if let Some(user) = &self.container_user {
            args.extend(["-u".into(), user.clone()]);
        }
        for port in &self.published_ports {
            args.extend(["-p".into(), port.clone()]);
        }
        args
    }

    /// A compose override adding these settings and `labels` to `service`.
    /// Every scalar is quoted, so no value can change the document's shape.
    /// `container_user` and `published_ports` are left out: they come from
    /// devcontainer.json, whose compose form configures them in the compose file.
    pub(super) fn compose_override(&self, service: &str, labels: &[(&str, &str)]) -> String {
        let q = yaml_string;
        let mut s = format!("services:\n  {}:\n", q(service));
        if self.privileged {
            s.push_str("    privileged: true\n");
        }
        if self.init {
            s.push_str("    init: true\n");
        }
        let mut list = |key: &str, items: Vec<String>| {
            if !items.is_empty() {
                s.push_str(&format!("    {key}:\n"));
                for item in items {
                    s.push_str(&format!("      - {}\n", q(&item)));
                }
            }
        };
        list("cap_add", self.cap_add.clone());
        list("security_opt", self.security_opt.clone());
        list(
            "volumes",
            self.mounts
                .iter()
                .filter_map(Mount::compose_volume)
                .collect(),
        );
        if !labels.is_empty() {
            s.push_str("    labels:\n");
            for (k, v) in labels {
                s.push_str(&format!("      {}: {}\n", q(k), q(v)));
            }
        }
        s
    }
}

/// A mount, from devcontainer.json's `mounts` or a feature's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Mount {
    /// The `--mount` value.
    pub(super) spec: String,
    source: Option<String>,
    target: Option<String>,
}

impl Mount {
    /// Parses a mount in either spec form: a `--mount` string
    /// (`type=bind,source=/a,target=/b`) or a `{type, source, target}` object.
    /// `apply` substitutes variables in the values.
    pub(super) fn parse(value: &serde_json::Value, apply: &dyn Fn(&str) -> String) -> Option<Self> {
        match value {
            serde_json::Value::String(s) => {
                let spec = apply(s);
                let (mut source, mut target) = (None, None);
                for kv in spec.split(',') {
                    if let Some((k, v)) = kv.split_once('=') {
                        match k.trim() {
                            "source" | "src" => source = Some(v.to_string()),
                            "target" | "dst" | "destination" => target = Some(v.to_string()),
                            _ => {}
                        }
                    }
                }
                Some(Self {
                    spec,
                    source,
                    target,
                })
            }
            serde_json::Value::Object(o) => {
                let get = |k: &str| o.get(k).and_then(|v| v.as_str());
                let source = get("source").map(apply);
                let target = get("target").map(apply);
                let mut parts = Vec::new();
                if let Some(t) = get("type") {
                    parts.push(format!("type={t}"));
                }
                if let Some(s) = &source {
                    parts.push(format!("source={s}"));
                }
                if let Some(t) = &target {
                    parts.push(format!("target={t}"));
                }
                (!parts.is_empty()).then(|| Self {
                    spec: parts.join(","),
                    source,
                    target,
                })
            }
            _ => None,
        }
    }

    /// The compose `volumes` entry, `source:target`, when both are known.
    fn compose_volume(&self) -> Option<String> {
        Some(format!(
            "{}:{}",
            self.source.as_ref()?,
            self.target.as_ref()?
        ))
    }
}

/// A YAML double-quoted scalar (JSON string syntax is valid YAML).
fn yaml_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devcontainer::backend::LABEL_PROJECT;
    use crate::devcontainer::vars::Vars;
    use serde_json::json;
    use std::path::Path;

    fn as_is(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn mounts_parse_from_string_and_object() {
        let m = Mount::parse(&json!("type=bind,source=/a,target=/b,readonly"), &as_is).unwrap();
        assert_eq!(m.spec, "type=bind,source=/a,target=/b,readonly");
        assert_eq!(m.compose_volume().as_deref(), Some("/a:/b"));
        let m = Mount::parse(
            &json!({"type": "volume", "source": "vol", "target": "/data"}),
            &as_is,
        )
        .unwrap();
        assert_eq!(m.spec, "type=volume,source=vol,target=/data");
        // Without a target there is no compose volume to write.
        let m = Mount::parse(&json!("source=/a"), &as_is).unwrap();
        assert_eq!(m.compose_volume(), None);
        assert!(Mount::parse(&json!(3), &as_is).is_none());
    }

    #[test]
    fn mounts_resolve_variables_including_the_container_workspace() {
        let p = Path::new("/home/me/proj");
        let vars = Vars::new(p).with_container_workspace("/workspaces/proj");
        let obj = json!({
            "type": "bind",
            "source": "${localWorkspaceFolder}/cache",
            "target": "${containerWorkspaceFolder}/.cache"
        });
        assert_eq!(
            Mount::parse(&obj, &|s| vars.apply(s)).unwrap().spec,
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
            mounts: vec![Mount::parse(&json!("type=volume,source=v,target=/d"), &as_is).unwrap()],
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
                "--mount",
                "type=volume,source=v,target=/d",
                "-u",
                "dev",
                "-p",
                "127.0.0.1:3000:3000"
            ]
        );
        assert!(CreateSettings::default().to_args().is_empty());
    }

    #[test]
    fn merged_settings_keep_both_sides() {
        let mut a = CreateSettings {
            cap_add: vec!["A".into()],
            container_user: Some("dev".into()),
            ..Default::default()
        };
        a.merge(CreateSettings {
            privileged: true,
            cap_add: vec!["B".into()],
            ..Default::default()
        });
        assert!(a.privileged);
        assert_eq!(a.cap_add, ["A", "B"]);
        assert_eq!(a.container_user.as_deref(), Some("dev"));
    }

    #[test]
    fn the_compose_override_is_quoted_yaml() {
        let s = CreateSettings {
            privileged: true,
            init: true,
            cap_add: vec!["SYS_PTRACE".into()],
            mounts: vec![Mount::parse(&json!("source=v,target=/d"), &as_is).unwrap()],
            ..Default::default()
        };
        let ov = s.compose_override("dev", &[(LABEL_PROJECT, "/p/it's: here")]);
        assert_eq!(
            ov,
            "services:\n  \"dev\":\n    privileged: true\n    init: true\n    \
             cap_add:\n      - \"SYS_PTRACE\"\n    volumes:\n      - \"v:/d\"\n    \
             labels:\n      \"dev.lodge.project\": \"/p/it's: here\"\n"
        );
    }

    #[test]
    fn app_ports_are_published_on_loopback_only() {
        let ports = parse_app_ports(Some(json!([3000, "4000", "8080:80", "0.0.0.0:9:9"])));
        assert_eq!(
            ports,
            [
                "127.0.0.1:3000:3000",
                "127.0.0.1:4000:4000",
                "127.0.0.1:8080:80",
                "0.0.0.0:9:9" // an explicit address is the user's decision
            ]
        );
        assert_eq!(parse_app_ports(Some(json!(5000))), ["127.0.0.1:5000:5000"]);
        assert!(parse_app_ports(None).is_empty());
    }
}
