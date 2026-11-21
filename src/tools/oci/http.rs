//! The transport the registry client runs on: one `GET`. The only file that
//! names the HTTP crate, so another transport (or a test) is another [`Http`].

use anyhow::{Context, Result};
use std::io::Read;
use std::time::Duration;

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
/// blob to a CDN with a signed URL).
pub trait Http: Send + Sync {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response>;
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
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
        let mut request = self.0.get(url);
        for (k, v) in headers {
            request = request.header(*k, *v);
        }
        let response = request
            .call()
            .with_context(|| format!("requesting {url}"))?;
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

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    type Headers = Vec<(String, String)>;
    type Answer = (u16, Headers, Vec<u8>);

    /// A network that answers each URL from a script, in order, and records what
    /// it was asked.
    #[derive(Default)]
    pub struct FakeHttp {
        answers: Mutex<BTreeMap<String, Vec<Answer>>>,
        pub requests: Mutex<Vec<(String, Headers)>>,
    }

    impl FakeHttp {
        pub fn new() -> Self {
            Self::default()
        }

        /// The next answer for `url`.
        pub fn answer(self, url: &str, status: u16, headers: &[(&str, &str)], body: &[u8]) -> Self {
            self.answers
                .lock()
                .unwrap()
                .entry(url.into())
                .or_default()
                .push((
                    status,
                    headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    body.to_vec(),
                ));
            self
        }

        /// The header `name` sent on the `n`th request for `url`.
        pub fn sent(&self, url: &str, n: usize, name: &str) -> Option<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(u, _)| u == url)
                .nth(n)?
                .1
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        }
    }

    impl Http for FakeHttp {
        fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
            self.requests.lock().unwrap().push((
                url.into(),
                headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ));
            let mut answers = self.answers.lock().unwrap();
            let queue = answers
                .get_mut(url)
                .filter(|q| !q.is_empty())
                .with_context(|| format!("FakeHttp: unexpected request for {url}"))?;
            let (status, headers, body) = queue.remove(0);
            Ok(Response {
                status,
                headers,
                body: Box::new(std::io::Cursor::new(body)),
            })
        }
    }
}
