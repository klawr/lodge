//! One repository on one registry: the API calls and the handshake that lets
//! them through. It speaks to [`Http`] and asks [`Credentials`] for a login, and
//! checks everything it is given against the digest it was asked for.

use super::auth::{self, Challenge};
use super::credentials::{Credentials, Login, LoginPrompt, NoPrompt};
use super::digest::{Digest, copy_verified};
use super::http::{Body, Http, Method, Request, Response};
use super::manifest::Descriptor;
use super::reference::Target;
use crate::output::warning;
use crate::tools::Unreachable;
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

/// A manifest is a few KB; this is generous, and keeps a registry from filling memory.
const MANIFEST_LIMIT: u64 = 4 * 1024 * 1024;

const ACCEPT: &[&str] = &[
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// How long to wait before each new try of a request the registry answered with
/// "not now" (a rate limit, an overloaded server); once per entry, then the
/// answer is the caller's.
pub const RETRY_WAITS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(9),
];

/// The longest a registry's `Retry-After` is obeyed: a longer wait is a failure.
const RETRY_AFTER_LIMIT: Duration = Duration::from_secs(30);

/// A blob that a registry refused in one piece is sent in pieces of this size,
/// each held in memory while it is sent.
const CHUNK: u64 = 32 * 1024 * 1024;

/// What the token is asked to allow. A pull is anonymous-friendly; a push is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Pull,
    Push,
}

pub struct Registry<'a> {
    http: &'a dyn Http,
    credentials: &'a dyn Credentials,
    registry: &'a str,
    repository: &'a str,
    access: Access,
    /// What was last agreed with the registry: sent with every request after.
    authorization: Option<Agreed>,
    waits: Vec<Duration>,
    /// Who to ask for a login when the registry refuses, once per registry.
    prompt: &'a dyn LoginPrompt,
    /// The login a person typed, which beats the ones that were found.
    typed: Option<Login>,
    asked: bool,
}

/// An `Authorization` the registry accepted a challenge with, and whether a login
/// went into it (so a later refusal can tell "refused the login" from "wants one").
struct Agreed {
    header: String,
    with_login: bool,
}

/// What a registry said when it wanted a login and there was none that it
/// accepted: the one thing a person asked can mend. `rejected` is whether a login
/// was sent and refused, rather than none having been there.
#[derive(Debug)]
struct NeedsLogin {
    message: String,
    rejected: bool,
}

impl std::fmt::Display for NeedsLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NeedsLogin {}

impl<'a> Registry<'a> {
    pub fn new(
        http: &'a dyn Http,
        credentials: &'a dyn Credentials,
        registry: &'a str,
        repository: &'a str,
        access: Access,
    ) -> Self {
        Self {
            http,
            credentials,
            registry,
            repository,
            access,
            authorization: None,
            waits: RETRY_WAITS.to_vec(),
            prompt: &NoPrompt,
            typed: None,
            asked: false,
        }
    }

    /// Asks `prompt` for a login when the registry refuses the one there is.
    pub fn asking(mut self, prompt: &'a dyn LoginPrompt) -> Self {
        self.prompt = prompt;
        self
    }

    /// The waits before each new try of a request answered with "not now".
    pub fn retrying(mut self, waits: &[Duration]) -> Self {
        self.waits = waits.to_vec();
        self
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

    /// The manifest `tag` points at, or `None` when there is none yet.
    pub fn current_manifest(&mut self, tag: &str) -> Result<Option<Vec<u8>>> {
        let url = self.url("manifests", tag);
        let accept = ACCEPT.join(", ");
        let response = self.request(Method::Get, &url, &[("Accept", &accept)], Body::None)?;
        match response.status {
            200 => Ok(Some(response.read_limited(MANIFEST_LIMIT)?)),
            404 => Ok(None),
            status => Err(self.failure(status, &url)),
        }
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

    /// Whether the registry already holds the blob, so it is not sent again.
    pub fn has_blob(&mut self, digest: &Digest) -> Result<bool> {
        let url = self.url("blobs", digest.as_str());
        match self.request(Method::Head, &url, &[], Body::None)?.status {
            200 => Ok(true),
            404 => Ok(false),
            status => Err(self.failure(status, &url)),
        }
    }

    /// Uploads a blob in one piece, unless the registry has it. The digest it
    /// reports for what it stored must be the one sent.
    pub fn upload_blob(&mut self, digest: &Digest, body: Body) -> Result<()> {
        self.upload(digest, body, true)
    }

    fn upload(&mut self, digest: &Digest, body: Body, may_retry: bool) -> Result<()> {
        if self.has_blob(digest)? {
            return Ok(());
        }
        let start = self.url("blobs", "uploads/");
        let started = self.request(Method::Post, &start, &[], Body::None)?;
        if started.status != 202 {
            return Err(self.failure(started.status, &start));
        }
        let location = started
            .header("location")
            .context("the registry did not say where to upload")?;
        let target = self.upload_url(location, Some(digest))?;
        let headers = [("Content-Type", "application/octet-stream")];
        let done = match self.request(Method::Put, &target, &headers, body.clone()) {
            Ok(done) => done,
            // A registry that refuses a write answers at once and hangs up, which
            // the sender sees as a broken connection and not as a 401: a stale
            // token looks like this. Once more, from the start, with a new handshake.
            Err(_) if may_retry => {
                self.authorization = None;
                return self.upload(digest, body, false);
            }
            Err(e) => return Err(e),
        };
        // A registry (or a proxy in front of it) that limits one request's size.
        if done.status == 413 {
            return self.upload_chunked(digest, &body);
        }
        if done.status != 201 {
            return Err(self.failure(done.status, &target));
        }
        Self::check_echo(&done, digest)
    }

    /// Uploads a blob in pieces, in a new upload: each piece is `PATCH`ed where the
    /// last answer said, and an empty `PUT` with the digest closes it.
    fn upload_chunked(&mut self, digest: &Digest, body: &Body) -> Result<()> {
        let start = self.url("blobs", "uploads/");
        let started = self.request(Method::Post, &start, &[], Body::None)?;
        if started.status != 202 {
            return Err(self.failure(started.status, &start));
        }
        let mut location = started
            .header("location")
            .context("the registry did not say where to upload")?
            .to_string();
        let mut reader = body.open()?;
        let mut sent = 0u64;
        loop {
            let mut piece = Vec::new();
            (&mut reader).take(CHUNK).read_to_end(&mut piece)?;
            if piece.is_empty() {
                break;
            }
            let range = format!("{sent}-{}", sent + piece.len() as u64 - 1);
            let target = self.upload_url(&location, None)?;
            let headers = [
                ("Content-Type", "application/octet-stream"),
                ("Content-Range", range.as_str()),
            ];
            let len = piece.len() as u64;
            let answer = self.request(Method::Patch, &target, &headers, Body::Bytes(piece))?;
            if answer.status != 202 {
                return Err(self.failure(answer.status, &target));
            }
            if let Some(next) = answer.header("location") {
                location = next.to_string();
            }
            sent += len;
        }
        let target = self.upload_url(&location, Some(digest))?;
        let done = self.request(Method::Put, &target, &[], Body::None)?;
        if done.status != 201 {
            return Err(self.failure(done.status, &target));
        }
        Self::check_echo(&done, digest)
    }

    /// Writes the manifest `bytes` under `tag`, and returns its digest.
    pub fn put_manifest(&mut self, tag: &str, media_type: &str, bytes: Vec<u8>) -> Result<Digest> {
        let digest = Digest::of(&bytes);
        let url = self.url("manifests", tag);
        let headers = [("Content-Type", media_type)];
        let done = self.request(Method::Put, &url, &headers, Body::Bytes(bytes))?;
        if !matches!(done.status, 200 | 201) {
            return Err(self.failure(done.status, &url));
        }
        Self::check_echo(&done, &digest)?;
        Ok(digest)
    }

    /// Where the registry wants the bytes: its `Location`, with the digest added
    /// when the upload is being closed.
    /// It must be this registry, because the token is sent along. Only the host
    /// is compared: the URL is rebuilt with the scheme lodge chose, so a registry
    /// behind a proxy that writes `http://` in its answers is still spoken to
    /// the way it was asked.
    fn upload_url(&self, location: &str, digest: Option<&Digest>) -> Result<String> {
        let base = format!("{}://{}", self.scheme(), self.registry);
        let path = match location.split_once("://") {
            Some((_, rest)) => match rest.split_once('/') {
                Some((host, path)) if host == self.registry => format!("/{path}"),
                _ => bail!(
                    "the registry sent uploads to {location}, which is not {}",
                    self.registry
                ),
            },
            None if location.starts_with('/') => location.to_string(),
            // Relative to where the upload was started.
            None => format!("/v2/{}/blobs/uploads/{location}", self.repository),
        };
        let Some(digest) = digest else {
            return Ok(format!("{base}{path}"));
        };
        let sep = if path.contains('?') { '&' } else { '?' };
        Ok(format!(
            "{base}{path}{sep}digest={}",
            auth::percent_encode(digest.as_str())
        ))
    }

    /// A registry that says what digest it stored must say the one that was sent.
    fn check_echo(response: &Response, sent: &Digest) -> Result<()> {
        match response.header("docker-content-digest") {
            Some(echoed) if echoed != sent.as_str() => {
                bail!("the registry stored {echoed}, not the {sent} that was sent")
            }
            _ => Ok(()),
        }
    }

    /// A `GET` that must be answered with the thing asked for.
    fn get(&mut self, url: &str, accept: &[&str]) -> Result<Response> {
        let accept = accept.join(", ");
        let headers: Vec<(&str, &str)> = if accept.is_empty() {
            Vec::new()
        } else {
            vec![("Accept", accept.as_str())]
        };
        let response = self.request(Method::Get, url, &headers, Body::None)?;
        match response.status {
            200 => Ok(response),
            status => Err(self.failure(status, url)),
        }
    }

    /// A request that answers a 401 once, with whatever the registry asks for. A
    /// token that has gone stale is answered the same way, so a long pull outlives
    /// it; a second 401 is the registry's answer, unless a person can be asked for
    /// a login (once), in which case the request is made again with it. Any status
    /// comes back: what it means is the caller's to say.
    fn request(
        &mut self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> Result<Response> {
        let first = self.answering(method, url, headers, body.clone());
        // Refused, and whether a login was in it when it was.
        let refused = match &first {
            Ok(response) if matches!(response.status, 401 | 403) => {
                Some(self.authorization.as_ref().is_some_and(|a| a.with_login))
            }
            Ok(_) => None,
            Err(e) => e.downcast_ref::<NeedsLogin>().map(|n| n.rejected),
        };
        let Some(rejected) = refused.filter(|_| !self.asked) else {
            return first;
        };
        self.asked = true;
        let Some(login) = self.prompt.ask(self.registry, rejected)? else {
            return first;
        };
        self.typed = Some(login.clone());
        self.authorization = None;
        let again = self.answering(method, url, headers, body);
        match &again {
            Ok(response) if !matches!(response.status, 401 | 403) => {
                self.prompt.accepted(self.registry, &login)
            }
            // Not a login to go on with.
            _ => self.typed = None,
        }
        again
    }

    /// Sends the request, and answers one challenge to it.
    fn answering(
        &mut self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> Result<Response> {
        let mut response = self.send(method, url, headers, body.clone())?;
        if response.status == 401 {
            let challenge = response
                .header("www-authenticate")
                .and_then(auth::parse_challenge);
            if let Some(challenge) = challenge {
                self.authorization = Some(self.answer(&challenge)?);
                response = self.send(method, url, headers, body)?;
            }
        }
        Ok(response)
    }

    /// What a status that was not wanted says.
    fn failure(&self, status: u16, url: &str) -> anyhow::Error {
        match (status, self.access) {
            (401 | 403, Access::Pull) => anyhow::anyhow!(
                "{} refused {} ({status}); a private repository needs a login: \
                 `ldg registry login {}` (lodge also reads docker's config file and its \
                 credential helper)",
                self.registry,
                self.repository,
                self.registry
            ),
            (401 | 403, Access::Push) => anyhow::anyhow!(
                "{} refused to write {} ({status}); pushing needs a login that may write \
                 there: `ldg registry login {}` (lodge also reads docker's config file \
                 and its credential helper)",
                self.registry,
                self.repository,
                self.registry
            ),
            (404, _) => anyhow::anyhow!("not found: {}/{}", self.registry, self.repository),
            _ => anyhow::anyhow!("{url} answered {status}"),
        }
    }

    fn send(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> Result<Response> {
        let mut waits = self.waits.iter();
        loop {
            crate::output::trace_line(&format!("{} {url}", method.name()));
            let mut all: Vec<(&str, &str)> = headers.to_vec();
            if let Some(a) = &self.authorization {
                all.push(("Authorization", &a.header));
            }
            // `Http` answers every status as a response, so an error is the network.
            let response = self
                .http
                .send(Request {
                    method,
                    url,
                    headers: &all,
                    body: body.clone(),
                })
                .map_err(|e| e.context(Unreachable))?;
            if !Self::is_not_now(method, response.status) {
                return Ok(response);
            }
            let Some(wait) = waits.next() else {
                return Ok(response);
            };
            let asked = response
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let wait = match asked {
                Some(asked) if asked > RETRY_AFTER_LIMIT => return Ok(response),
                Some(asked) if !wait.is_zero() => asked.max(*wait),
                _ => *wait,
            };
            crate::output::trace_line(&format!(
                "{} answered {}; trying again in {}s",
                self.registry,
                response.status,
                wait.as_secs()
            ));
            std::thread::sleep(wait);
        }
    }

    /// A status that says "not now", so the same request may be sent again: a
    /// rate limit, or a server that is overloaded or behind a gateway that is. A
    /// plain 500 is only retried for a read, which cannot have done anything.
    fn is_not_now(method: Method, status: u16) -> bool {
        match status {
            429 | 502 | 503 | 504 => true,
            500 => matches!(method, Method::Get | Method::Head),
            _ => false,
        }
    }

    /// The login to answer a challenge with: what a person typed, else what the
    /// sources hold. A source that is broken is said so and goes without.
    fn find_login(&self) -> Option<Login> {
        if let Some(typed) = &self.typed {
            return Some(typed.clone());
        }
        match self.credentials.login(self.registry) {
            Ok(login) => login,
            Err(e) => {
                warning!("not using a registry login: {e:#}");
                None
            }
        }
    }

    /// What answers a challenge.
    fn answer(&self, challenge: &Challenge) -> Result<Agreed> {
        let login = self.find_login();
        let with_login = login.is_some();
        let agreed = |header: String| Agreed { header, with_login };
        match challenge {
            Challenge::Basic => match login {
                Some(login) => Ok(agreed(auth::basic(&login))),
                None => Err(NeedsLogin {
                    message: format!(
                        "{} needs a login (`ldg registry login {}`)",
                        self.registry, self.registry
                    ),
                    rejected: false,
                }
                .into()),
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
                let scope = match self.access {
                    // What the registry names; one that names nothing wants a pull.
                    Access::Pull => scope
                        .clone()
                        .unwrap_or_else(|| format!("repository:{}:pull", self.repository)),
                    // Asked for as docker does, whatever the challenge says.
                    Access::Push => format!("repository:{}:pull,push", self.repository),
                };
                let mut url = realm.clone();
                let mut sep = if realm.contains('?') { '&' } else { '?' };
                for (k, v) in [
                    ("service", service.as_deref()),
                    ("scope", Some(scope.as_str())),
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
                let response = self
                    .http
                    .get(&url, &headers)
                    .map_err(|e| e.context(Unreachable))?;
                match response.status {
                    200 => {}
                    401 | 403 => {
                        return Err(NeedsLogin {
                            message: format!("the token service refused ({})", response.status),
                            rejected: with_login,
                        }
                        .into());
                    }
                    status => bail!("the token service refused ({status})"),
                }
                let body = response.read_limited(MANIFEST_LIMIT)?;
                let token: Token = serde_json::from_slice(&body).context("reading the token")?;
                let token = token
                    .token
                    .or(token.access_token)
                    .context("the token service sent no token")?;
                Ok(agreed(format!("Bearer {token}")))
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct Token {
    token: Option<String>,
    access_token: Option<String>,
}
