//! One repository on one registry: the API calls and the handshake that lets
//! them through. It speaks to [`Http`] and asks [`Credentials`] for a login, and
//! checks everything it is given against the digest it was asked for.

use super::auth::{self, Challenge};
use super::credentials::Credentials;
use super::digest::{Digest, copy_verified};
use super::http::{Http, Response};
use super::manifest::Descriptor;
use super::reference::Target;
use crate::output::warning;
use anyhow::{Context, Result, bail};
use std::path::Path;

/// A manifest is a few KB; this is generous, and keeps a registry from filling memory.
const MANIFEST_LIMIT: u64 = 4 * 1024 * 1024;

const ACCEPT: &[&str] = &[
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

pub struct Registry<'a> {
    http: &'a dyn Http,
    credentials: &'a dyn Credentials,
    registry: &'a str,
    repository: &'a str,
    /// What was last agreed with the registry: sent with every request after.
    authorization: Option<String>,
}

impl<'a> Registry<'a> {
    pub fn new(
        http: &'a dyn Http,
        credentials: &'a dyn Credentials,
        registry: &'a str,
        repository: &'a str,
    ) -> Self {
        Self {
            http,
            credentials,
            registry,
            repository,
            authorization: None,
        }
    }

    /// A registry on this machine is spoken to without TLS, as docker does.
    fn scheme(&self) -> &'static str {
        // What follows the last `:` is a port, unless it is inside an IPv6 `[...]`.
        let host = match self.registry.rsplit_once(':') {
            Some((h, port)) if !port.contains(']') => h,
            _ => self.registry,
        };
        if matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
            "http"
        } else {
            "https"
        }
    }

    fn url(&self, kind: &str, reference: &str) -> String {
        format!(
            "{}://{}/v2/{}/{kind}/{reference}",
            self.scheme(),
            self.registry,
            self.repository
        )
    }

    /// The manifest `target` names. A digest is checked against what came.
    pub fn manifest(&mut self, target: &Target) -> Result<Vec<u8>> {
        let url = self.url("manifests", target.as_str());
        let response = self.get(&url, ACCEPT)?;
        let bytes = response.read_limited(MANIFEST_LIMIT)?;
        if let Target::Digest(expected) = target {
            let got = Digest::of(&bytes);
            if &got != expected {
                bail!("the manifest is {got}, expected {expected}");
            }
        }
        Ok(bytes)
    }

    /// The manifest the descriptor names, checked against its digest.
    pub fn manifest_of(&mut self, d: &Descriptor) -> Result<Vec<u8>> {
        self.manifest(&Target::Digest(Digest::parse(&d.digest)?))
    }

    /// Downloads a blob to `dest`, which only exists once it has been checked
    /// against the descriptor's size and digest.
    pub fn blob_to(&mut self, d: &Descriptor, dest: &Path) -> Result<()> {
        let digest = Digest::parse(&d.digest)?;
        let url = self.url("blobs", digest.as_str());
        let mut response = self.get(&url, &[])?;
        let partial = dest.with_extension("part");
        let result = (|| {
            let mut file = std::fs::File::create(&partial)
                .with_context(|| format!("creating {}", partial.display()))?;
            copy_verified(&mut response.body, &mut file, d.size, &digest)?;
            file.sync_all()?;
            std::fs::rename(&partial, dest).context("placing the downloaded file")
        })();
        if result.is_err() {
            std::fs::remove_file(&partial).ok();
        }
        result.with_context(|| format!("downloading {digest}"))
    }

    /// A `GET` that answers a 401 once, with whatever the registry asks for. A
    /// token that has gone stale is answered the same way, so a long pull outlives
    /// it; a second 401 is the registry's answer.
    fn get(&mut self, url: &str, accept: &[&str]) -> Result<Response> {
        let mut response = self.send(url, accept)?;
        if response.status == 401 {
            let challenge = response
                .header("www-authenticate")
                .and_then(auth::parse_challenge);
            if let Some(challenge) = challenge {
                self.authorization = Some(self.answer(&challenge)?);
                response = self.send(url, accept)?;
            }
        }
        match response.status {
            200 => Ok(response),
            401 | 403 => bail!(
                "{} refused {} ({}); a private repository needs `docker login`, which lodge reads \
                 only when the login is in docker's config file itself, not in a credential helper",
                self.registry,
                self.repository,
                response.status
            ),
            404 => bail!("not found: {}/{}", self.registry, self.repository),
            status => bail!("{url} answered {status}"),
        }
    }

    fn send(&self, url: &str, accept: &[&str]) -> Result<Response> {
        crate::output::trace_line(&format!("GET {url}"));
        let accept = accept.join(", ");
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if !accept.is_empty() {
            headers.push(("Accept", &accept));
        }
        if let Some(a) = &self.authorization {
            headers.push(("Authorization", a));
        }
        self.http.get(url, &headers)
    }

    /// The `Authorization` value that answers a challenge.
    fn answer(&self, challenge: &Challenge) -> Result<String> {
        let login = match self.credentials.login(self.registry) {
            Ok(login) => login,
            Err(e) => {
                warning!("not using a registry login: {e:#}");
                None
            }
        };
        match challenge {
            Challenge::Basic => match login {
                Some(login) => Ok(auth::basic(&login)),
                None => bail!(
                    "{} needs a login (`docker login {}`)",
                    self.registry,
                    self.registry
                ),
            },
            Challenge::Bearer {
                realm,
                service,
                scope,
            } => {
                // A login is only sent over TLS, wherever the realm is.
                if !realm.starts_with("https://") && !(login.is_none() && self.scheme() == "http") {
                    bail!("the token service {realm} is not https");
                }
                // A registry that names no scope is asking for what a pull needs.
                let pull = format!("repository:{}:pull", self.repository);
                let mut url = realm.clone();
                let mut sep = if realm.contains('?') { '&' } else { '?' };
                for (k, v) in [
                    ("service", service.as_deref()),
                    ("scope", Some(scope.as_deref().unwrap_or(&pull))),
                ] {
                    if let Some(v) = v {
                        url.push_str(&format!("{sep}{k}={}", auth::percent_encode(v)));
                        sep = '&';
                    }
                }
                let basic = login.as_ref().map(auth::basic);
                let headers: Vec<(&str, &str)> = basic
                    .iter()
                    .map(|b| ("Authorization", b.as_str()))
                    .collect();
                crate::output::trace_line(&format!("GET {url}"));
                let response = self.http.get(&url, &headers)?;
                if response.status != 200 {
                    bail!("the token service refused ({})", response.status);
                }
                let body = response.read_limited(MANIFEST_LIMIT)?;
                let token: Token = serde_json::from_slice(&body).context("reading the token")?;
                let token = token
                    .token
                    .or(token.access_token)
                    .context("the token service sent no token")?;
                Ok(format!("Bearer {token}"))
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct Token {
    token: Option<String>,
    access_token: Option<String>,
}
