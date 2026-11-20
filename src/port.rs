//! A port to forward, as devcontainer.json declares it and the forward daemon
//! carries it out. Its own module, so the devcontainer spec and the forwarder
//! share the type without depending on each other.

/// One port forward: local port `local` on the host tunnels to port `port` of
/// a target on the container network — the primary container itself when
/// `host` is None, or a named host (compose service, container name, network
/// alias) otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForward {
    pub local: u16,
    pub host: Option<String>,
    pub port: u16,
}

impl PortForward {
    /// Local port == target port on the primary container (the common case).
    pub fn same(port: u16) -> Self {
        Self {
            local: port,
            host: None,
            port,
        }
    }

    /// Serialized form for the internal `ldg _forward --port` flag.
    pub fn encode(&self) -> String {
        match &self.host {
            Some(h) => format!("{}:{}:{}", self.local, h, self.port),
            None => format!("{}:{}", self.local, self.port),
        }
    }

    pub fn decode(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        match parts.as_slice() {
            [l, p] => Some(Self {
                local: l.parse().ok()?,
                host: None,
                port: p.parse().ok()?,
            }),
            [l, h, p] => Some(Self {
                local: l.parse().ok()?,
                host: Some(h.to_string()),
                port: p.parse().ok()?,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_forward_encode_decode_roundtrip() {
        for f in [
            PortForward::same(8080),
            PortForward {
                local: 3000,
                host: None,
                port: 8080,
            },
            PortForward {
                local: 5432,
                host: Some("db".into()),
                port: 5432,
            },
        ] {
            assert_eq!(PortForward::decode(&f.encode()), Some(f));
        }
        assert_eq!(PortForward::decode("garbage"), None);
        assert_eq!(PortForward::decode("1:2:3:4"), None);
    }
}
