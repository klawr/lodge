//! The pieces of the registry token handshake that are plain text: reading a
//! `WWW-Authenticate` challenge, and writing what answers it. Pure.

use super::credentials::Login;
use base64::{Engine as _, engine::general_purpose::STANDARD};

#[derive(Debug, PartialEq, Eq)]
pub enum Challenge {
    Basic,
    Bearer {
        realm: String,
        service: Option<String>,
        scope: Option<String>,
    },
}

/// `Bearer realm="https://auth.example/token",service="registry",scope="…"`.
pub fn parse_challenge(header: &str) -> Option<Challenge> {
    let header = header.trim();
    let (scheme, rest) = header
        .split_once(char::is_whitespace)
        .unwrap_or((header, ""));
    let params = parse_params(rest);
    let get = |k: &str| {
        params
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.clone())
    };
    match scheme.to_ascii_lowercase().as_str() {
        "basic" => Some(Challenge::Basic),
        "bearer" => Some(Challenge::Bearer {
            realm: get("realm")?,
            service: get("service"),
            scope: get("scope"),
        }),
        _ => None,
    }
}

// `k="v with, commas",k2=v2`
fn parse_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace() || *c == ',').is_some() {}
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && *c != ',') {
            key.push(c);
        }
        if chars.next_if_eq(&'=').is_none() {
            break;
        }
        let mut value = String::new();
        if chars.next_if_eq(&'"').is_some() {
            while let Some(c) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next()),
                    '"' => break,
                    c => value.push(c),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| *c != ',') {
                value.push(c);
            }
        }
        out.push((key.trim().to_string(), value.trim().to_string()));
    }
    out
}

/// The `Authorization` value for HTTP basic auth.
pub fn basic(login: &Login) -> String {
    format!(
        "Basic {}",
        STANDARD.encode(format!("{}:{}", login.username, login.password))
    )
}

/// Escapes a query value.
pub fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_challenge_is_read() {
        let c = parse_challenge(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:a/b:pull,push""#,
        )
        .unwrap();
        assert_eq!(
            c,
            Challenge::Bearer {
                realm: "https://ghcr.io/token".into(),
                service: Some("ghcr.io".into()),
                scope: Some("repository:a/b:pull,push".into()),
            }
        );
    }

    #[test]
    fn basic_unquoted_values_and_nonsense() {
        assert_eq!(
            parse_challenge(r#"Basic realm="x""#),
            Some(Challenge::Basic)
        );
        assert_eq!(parse_challenge("basic"), Some(Challenge::Basic));
        let c = parse_challenge("bearer realm=https://a/t,service=s").unwrap();
        assert!(matches!(c, Challenge::Bearer { service: Some(s), scope: None, .. } if s == "s"));
        assert_eq!(parse_challenge("Negotiate"), None);
        assert_eq!(parse_challenge(r#"Bearer service="s""#), None);
    }

    #[test]
    fn encoding_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(
            percent_encode("repository:a/b:pull"),
            "repository%3Aa%2Fb%3Apull"
        );
        assert_eq!(percent_encode("a-b_c.d~"), "a-b_c.d~");
    }

    #[test]
    fn basic_auth_is_base64_of_user_colon_password() {
        let l = Login {
            username: "u".into(),
            password: "p".into(),
        };
        assert_eq!(basic(&l), "Basic dTpw");
    }
}
