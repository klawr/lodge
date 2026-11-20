//! Container-creation settings that features ask for (mounts, `privileged`,
//! capabilities, `init`), applied when the container is created.

use serde_json::Value;

/// Container-creation settings a feature contributes, aggregated across all
/// features. Applied to the backend at `up` time (compose override / `docker
/// run` flags). `entrypoint` is intentionally not represented — lodge runs the
/// container as `sleep infinity`.
#[derive(Default)]
pub struct CreateFlags {
    pub(super) mounts: Vec<FeatureMount>,
    pub(super) privileged: bool,
    pub(super) cap_add: Vec<String>,
    pub(super) security_opt: Vec<String>,
    pub(super) init: bool,
}

pub(super) struct FeatureMount {
    source: String,
    target: String,
    kind: Option<String>,
}

impl CreateFlags {
    /// `docker run` flags (for the image/build backend).
    pub fn run_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if self.privileged {
            a.push("--privileged".into());
        }
        if self.init {
            a.push("--init".into());
        }
        for c in &self.cap_add {
            a.push("--cap-add".into());
            a.push(c.clone());
        }
        for s in &self.security_opt {
            a.push("--security-opt".into());
            a.push(s.clone());
        }
        for m in &self.mounts {
            a.push("--mount".into());
            a.push(m.to_mount_arg());
        }
        a
    }

    /// A docker-compose override snippet adding these settings to `service`.
    pub fn compose_override(&self, service: &str) -> String {
        let mut s = format!("services:\n  {service}:\n");
        if self.privileged {
            s.push_str("    privileged: true\n");
        }
        if self.init {
            s.push_str("    init: true\n");
        }
        if !self.cap_add.is_empty() {
            s.push_str("    cap_add:\n");
            for c in &self.cap_add {
                s.push_str(&format!("      - {c}\n"));
            }
        }
        if !self.security_opt.is_empty() {
            s.push_str("    security_opt:\n");
            for o in &self.security_opt {
                s.push_str(&format!("      - {o}\n"));
            }
        }
        if !self.mounts.is_empty() {
            s.push_str("    volumes:\n");
            for m in &self.mounts {
                s.push_str(&format!("      - \"{}:{}\"\n", m.source, m.target));
            }
        }
        s
    }
}

impl FeatureMount {
    fn to_mount_arg(&self) -> String {
        let mut parts = Vec::new();
        if let Some(k) = &self.kind {
            parts.push(format!("type={k}"));
        }
        parts.push(format!("source={}", self.source));
        parts.push(format!("target={}", self.target));
        parts.join(",")
    }
}

pub(super) fn parse_mount(v: &Value) -> Option<FeatureMount> {
    match v {
        // "type=bind,source=/a,target=/b"
        Value::String(s) => {
            let (mut source, mut target, mut kind) = (None, None, None);
            for kv in s.split(',') {
                if let Some((k, val)) = kv.split_once('=') {
                    match k.trim() {
                        "source" | "src" => source = Some(val.to_string()),
                        "target" | "dst" | "destination" => target = Some(val.to_string()),
                        "type" => kind = Some(val.to_string()),
                        _ => {}
                    }
                }
            }
            Some(FeatureMount {
                source: source?,
                target: target?,
                kind,
            })
        }
        Value::Object(o) => Some(FeatureMount {
            source: o.get("source").and_then(|v| v.as_str())?.to_string(),
            target: o.get("target").and_then(|v| v.as_str())?.to_string(),
            kind: o.get("type").and_then(|v| v.as_str()).map(String::from),
        }),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_mount_string_and_object() {
        let m = parse_mount(&json!("type=bind,source=/a,target=/b")).unwrap();
        assert_eq!(
            (m.source.as_str(), m.target.as_str(), m.kind.as_deref()),
            ("/a", "/b", Some("bind"))
        );
        let m = parse_mount(&json!({"source": "vol", "target": "/d"})).unwrap();
        assert_eq!(
            (m.source.as_str(), m.target.as_str(), m.kind),
            ("vol", "/d", None)
        );
        assert!(parse_mount(&json!("source=/a")).is_none()); // missing target
    }

    #[test]
    fn create_flags_render_run_args_and_compose() {
        let flags = CreateFlags {
            mounts: vec![FeatureMount {
                source: "v".into(),
                target: "/d".into(),
                kind: Some("volume".into()),
            }],
            privileged: true,
            cap_add: vec!["SYS_PTRACE".into()],
            security_opt: vec![],
            init: true,
        };
        let args = flags.run_args();
        assert!(args.contains(&"--privileged".to_string()));
        assert!(args.contains(&"--init".to_string()));
        assert!(
            args.contains(&"--cap-add".to_string()) && args.contains(&"SYS_PTRACE".to_string())
        );
        assert!(args.iter().any(|a| a == "type=volume,source=v,target=/d"));

        let ov = flags.compose_override("dev");
        assert!(ov.starts_with("services:\n  dev:\n"));
        assert!(ov.contains("privileged: true"));
        assert!(ov.contains("init: true"));
        assert!(ov.contains("- SYS_PTRACE"));
        assert!(ov.contains("\"v:/d\""));
    }
}
