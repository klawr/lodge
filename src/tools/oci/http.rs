//! The transport the registry client runs on: one request, one answer. The only
//! file that names the HTTP crate, so another transport (or a test) is another
//! [`Http`].

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
}

impl Method {
    /// As it is written on the wire.
    pub fn name(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
        }
    }
}

/// How long a write may wait for the registry's answer: after the last byte is
/// sent it verifies and commits the blob, which for a large bundle takes a while,
/// so the wait grows with what was sent: two minutes, and a second more for every
/// 8 MiB, at most half an hour. The stall limits of a read do not apply to it.
fn write_wait(len: u64) -> Duration {
    const BASE: u64 = 2 * 60;
    const CAP: u64 = 30 * 60;
    Duration::from_secs((BASE + len / (8 * 1024 * 1024)).min(CAP))
}

/// What is sent with a request. Either can be sent again, which a request that is
/// answered with a 401 needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    None,
    Bytes(Vec<u8>),
    /// A file, streamed: a bundle is not read into memory to be sent.
    File(PathBuf),
}

impl Body {
    /// How many bytes it holds (a file that cannot be read counts as none; sending
    /// it says why).
    pub fn len(&self) -> u64 {
        match self {
            Body::None => 0,
            Body::Bytes(b) => b.len() as u64,
            Body::File(p) => std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
        }
    }

    /// What it holds, from the start, to be read in pieces.
    pub fn open(&self) -> Result<Box<dyn Read>> {
        Ok(match self {
            Body::None => Box::new(std::io::empty()),
            Body::Bytes(b) => Box::new(std::io::Cursor::new(b.clone())),
            Body::File(p) => Box::new(
                std::fs::File::open(p).with_context(|| format!("reading {}", p.display()))?,
            ),
        })
    }
}

pub struct Request<'a> {
    pub method: Method,
    pub url: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: Body,
}

/// An answer. Whatever the status, it is returned: the client decides what a 401
/// or a 404 means.
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read>,
}

impl Response {
    /// A header's value, by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The whole body, at most `limit` bytes; more is an error.
    pub fn read_limited(mut self, limit: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        (&mut self.body).take(limit + 1).read_to_end(&mut out)?;
        if out.len() as u64 > limit {
            anyhow::bail!("the answer is larger than {limit} bytes");
        }
        Ok(out)
    }
}

/// An HTTP client. An implementation follows redirects, and must never send the
/// request's headers to another host than the one asked (a registry redirects a
/// blob to a CDN with a signed URL). Any status is an answer, not an error.
pub trait Http: Send + Sync {
    fn send(&self, request: Request<'_>) -> Result<Response>;

    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
        self.send(Request {
            method: Method::Get,
            url,
            headers,
            body: Body::None,
        })
    }
}

/// A shared transport is a transport: one network handed to more than one
/// holder, or kept by a test to read what it was asked.
impl<T: Http + ?Sized> Http for std::sync::Arc<T> {
    fn send(&self, request: Request<'_>) -> Result<Response> {
        (**self).send(request)
    }
}

/// The real network.
pub struct UreqHttp(ureq::Agent);

impl UreqHttp {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            // A stall, not a slow link: the total is not capped, a blob can be large.
            .timeout_per_read(Some(Duration::from_secs(30)))
            .user_agent(concat!("lodge/", env!("CARGO_PKG_VERSION")))
            .build();
        Self(config.into())
    }
}

impl Http for UreqHttp {
    fn send(&self, request: Request<'_>) -> Result<Response> {
        let url = request.url;
        let called = match (request.method, request.body) {
            (Method::Get, Body::None) => with_headers(self.0.get(url), request.headers).call(),
            (Method::Head, Body::None) => with_headers(self.0.head(url), request.headers).call(),
            (method @ (Method::Post | Method::Put | Method::Patch), body) => {
                let builder = match method {
                    Method::Post => self.0.post(url),
                    Method::Put => self.0.put(url),
                    _ => self.0.patch(url),
                };
                let wait = write_wait(body.len());
                let builder = with_headers(builder, request.headers)
                    .config()
                    .timeout_recv_response(Some(wait))
                    .timeout_per_read(Some(wait))
                    .build();
                match body {
                    Body::None => builder.send_empty(),
                    Body::Bytes(bytes) => builder.send(bytes),
                    Body::File(path) => {
                        let file = std::fs::File::open(&path)
                            .with_context(|| format!("reading {}", path.display()))?;
                        builder.send(file)
                    }
                }
            }
            (method, _) => bail!("a {method:?} request has no body"),
        };
        let response = called.with_context(|| format!("requesting {url}"))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
            .collect();
        Ok(Response {
            status,
            headers,
            body: Box::new(response.into_body().into_reader()),
        })
    }
}

fn with_headers<B>(
    mut builder: ureq::RequestBuilder<B>,
    headers: &[(&str, &str)],
) -> ureq::RequestBuilder<B> {
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_for_a_write_grows_with_what_was_sent_up_to_a_cap() {
        assert_eq!(write_wait(0), Duration::from_secs(120));
        assert_eq!(
            write_wait(1024 * 1024 * 1024),
            Duration::from_secs(120 + 128)
        );
        assert_eq!(write_wait(u64::MAX), Duration::from_secs(30 * 60));
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    type Headers = Vec<(String, String)>;

    /// What a request is answered with: a reply, or a connection that broke.
    enum Answer {
        Reply(u16, Headers, Vec<u8>),
        Broken,
    }

    /// What the fake was asked.
    pub struct Recorded {
        pub method: Method,
        pub url: String,
        pub headers: Headers,
        /// What was sent, read when the request was made.
        pub body: Vec<u8>,
    }

    /// A network that answers each method and URL from a script, in order, and
    /// records what it was asked.
    #[derive(Default)]
    pub struct FakeHttp {
        answers: Mutex<BTreeMap<(String, String), Vec<Answer>>>,
        pub requests: Mutex<Vec<Recorded>>,
    }

    fn key(method: Method, url: &str) -> (String, String) {
        (format!("{method:?}"), url.to_string())
    }

    impl FakeHttp {
        pub fn new() -> Self {
            Self::default()
        }

        /// The next answer to a `GET` of `url`.
        pub fn answer(self, url: &str, status: u16, headers: &[(&str, &str)], body: &[u8]) -> Self {
            self.answer_to(Method::Get, url, status, headers, body)
        }

        /// The next answer to a request of `method` for `url`.
        pub fn answer_to(
            self,
            method: Method,
            url: &str,
            status: u16,
            headers: &[(&str, &str)],
            body: &[u8],
        ) -> Self {
            self.answers
                .lock()
                .unwrap()
                .entry(key(method, url))
                .or_default()
                .push(Answer::Reply(
                    status,
                    headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    body.to_vec(),
                ));
            self
        }

        /// The next request of `method` for `url` fails as a broken connection does.
        pub fn break_on(self, method: Method, url: &str) -> Self {
            self.answers
                .lock()
                .unwrap()
                .entry(key(method, url))
                .or_default()
                .push(Answer::Broken);
            self
        }

        /// The header `name` sent on the `n`th request for `url`.
        pub fn sent(&self, url: &str, n: usize, name: &str) -> Option<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.url == url)
                .nth(n)?
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        }

        /// The methods and URLs asked, in order.
        pub fn asked(&self) -> Vec<(Method, String)> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|r| (r.method, r.url.clone()))
                .collect()
        }

        /// What was sent with the `n`th request of `method` for `url`.
        pub fn body_of(&self, method: Method, url: &str, n: usize) -> Option<Vec<u8>> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.method == method && r.url == url)
                .nth(n)
                .map(|r| r.body.clone())
        }
    }

    impl Http for FakeHttp {
        fn send(&self, request: Request<'_>) -> Result<Response> {
            let body = match &request.body {
                Body::None => Vec::new(),
                Body::Bytes(b) => b.clone(),
                Body::File(p) => std::fs::read(p)?,
            };
            self.requests.lock().unwrap().push(Recorded {
                method: request.method,
                url: request.url.into(),
                headers: request
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body,
            });
            let mut answers = self.answers.lock().unwrap();
            let queue = answers
                .get_mut(&key(request.method, request.url))
                .filter(|q| !q.is_empty())
                .with_context(|| {
                    format!(
                        "FakeHttp: unexpected {:?} for {}",
                        request.method, request.url
                    )
                })?;
            match queue.remove(0) {
                Answer::Reply(status, headers, body) => Ok(Response {
                    status,
                    headers,
                    body: Box::new(std::io::Cursor::new(body)),
                }),
                Answer::Broken => bail!("FakeHttp: the connection broke"),
            }
        }
    }
}
