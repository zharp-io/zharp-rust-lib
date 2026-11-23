//! Request and response headers on HTTP spans, as `http.request.header.<name>` and
//! `http.response.header.<name>`, so a failed request shows what was sent and what came back.
//! Secret headers (authorization, cookies, API keys, tokens, sessions) are recorded as
//! `[REDACTED]`: you see one was sent, never its value. The same list as the other Zharp SDKs.

use opentelemetry::KeyValue;

pub(crate) const REDACTED: &str = "[REDACTED]";
const MAX_VALUE: usize = 1000;
const MAX_HEADERS: usize = 100;

const SECRET_PARTS: &[&str] = &[
    "authorization",
    "cookie",
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api-key",
    "accesskey",
    "access-key",
    "privatekey",
    "private-key",
    "credential",
    "signature",
    "session",
    "csrf",
    "xsrf",
    "jwt",
];

pub(crate) fn is_secret(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "auth" || name == "sid" || SECRET_PARTS.iter().any(|part| name.contains(part))
}

/// The attributes for one side's headers. `headers` yields (name, value) pairs; a name seen twice
/// has its values joined with ", ".
pub(crate) fn attributes<'a>(
    side: &str,
    headers: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Vec<KeyValue> {
    let mut merged: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        let value = String::from_utf8_lossy(value).into_owned();
        if let Some(i) = merged.iter().position(|(n, _)| *n == name) {
            merged[i].1.push_str(", ");
            merged[i].1.push_str(&value);
        } else if merged.len() < MAX_HEADERS {
            merged.push((name, value));
        }
    }
    merged.sort_by(|a, b| a.0.cmp(&b.0));
    merged
        .into_iter()
        .map(|(name, value)| {
            let value = if is_secret(&name) {
                REDACTED.to_string()
            } else if value.len() > MAX_VALUE {
                let mut end = MAX_VALUE;
                while !value.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &value[..end])
            } else {
                value
            };
            KeyValue::new(format!("http.{side}.header.{name}"), value)
        })
        .collect()
}

/// A URL's query string with the values of secret-looking keys (`token`, `api_key`, `password`…)
/// as `[REDACTED]`.
pub(crate) fn mask_query(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if is_secret(&key.replace('_', "-")) => format!("{key}={REDACTED}"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_and_keeps_the_rest() {
        let attrs = attributes(
            "request",
            [
                ("Authorization", b"Bearer x".as_slice()),
                ("Cookie", b"a=b".as_slice()),
                ("X-Api-Key", b"k".as_slice()),
                ("Accept", b"text/html".as_slice()),
                ("accept", b"application/json".as_slice()),
                ("User-Agent", b"curl/8".as_slice()),
            ],
        );
        let pairs: Vec<(String, String)> = attrs
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect();
        assert_eq!(
            pairs,
            [
                (
                    "http.request.header.accept".to_string(),
                    "text/html, application/json".to_string()
                ),
                (
                    "http.request.header.authorization".into(),
                    "[REDACTED]".into()
                ),
                ("http.request.header.cookie".into(), "[REDACTED]".into()),
                ("http.request.header.user-agent".into(), "curl/8".into()),
                ("http.request.header.x-api-key".into(), "[REDACTED]".into()),
            ]
        );
        assert!(
            is_secret("set-cookie")
                && is_secret("x-csrf-token")
                && is_secret("sid")
                && !is_secret("x-request-id")
        );
    }

    #[test]
    fn masks_secret_query_values() {
        assert_eq!(
            mask_query("page=2&api_key=abc&token=t"),
            "page=2&api_key=[REDACTED]&token=[REDACTED]"
        );
    }
}
