//! RFC 2617 / RFC 7616 HTTP authentication primitives.
//!
//! Both transports used by XGView have to answer a `WWW-Authenticate`
//! challenge with credentials:
//!
//! * the ONVIF SOAP calls go over HTTP ([`crate::discovery::onvif`]),
//! * the RTSP session goes over its own request/response format
//!   ([`crate::rtsp`]).
//!
//! The digest computation is identical for both; only the request URI differs,
//! so the caller passes the URI it actually puts on the request line.
//!
//! `SHA-256` digests (RFC 7616) are *not* implemented: ONVIF devices and IP
//! cameras answer with `MD5` in practice. [`Challenge::authorization`] returns
//! `None` for anything else so the caller reports an authentication failure
//! instead of sending a wrong response.

use base64::Engine;
use md5::{Digest, Md5};

/// A parsed `WWW-Authenticate` challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    /// Lower case scheme, `digest` or `basic`.
    pub scheme: String,
    pub realm: String,
    pub nonce: String,
    pub qop: Option<String>,
    pub opaque: Option<String>,
    pub algorithm: Option<String>,
}

impl Challenge {
    /// `Authorization` header value for one request.
    ///
    /// Returns `None` when the scheme or the hash algorithm is not implemented.
    pub fn authorization(
        &self,
        method: &str,
        uri: &str,
        username: &str,
        password: &str,
    ) -> Option<String> {
        match self.scheme.as_str() {
            "basic" => Some(basic(username, password)),
            "digest" => self.digest(method, uri, username, password),
            _ => None,
        }
    }

    /// RFC 2617 / RFC 7616 `Digest` header value.
    fn digest(&self, method: &str, uri: &str, username: &str, password: &str) -> Option<String> {
        let algorithm = self.algorithm.as_deref().unwrap_or("MD5").to_ascii_uppercase();
        if algorithm.trim_end_matches("-SESS") != "MD5" {
            return None;
        }

        let qop = self.qop();
        let cnonce = uuid::Uuid::new_v4().simple().to_string();
        let nc = "00000001";
        let response = digest_response(self, method, uri, username, password, qop, nc, &cnonce);

        let mut header = format!(
            "Digest username={}, realm={}, nonce={}, uri={}, response={}",
            quote(username),
            quote(&self.realm),
            quote(&self.nonce),
            quote(uri),
            quote(&response)
        );
        if let Some(value) = self.algorithm.as_deref() {
            header.push_str(&format!(", algorithm={value}"));
        }
        if let Some(qop) = qop {
            header.push_str(&format!(", qop={qop}, nc={nc}, cnonce={}", quote(&cnonce)));
        }
        if let Some(opaque) = self.opaque.as_deref() {
            header.push_str(&format!(", opaque={}", quote(opaque)));
        }
        Some(header)
    }

    /// The `auth` quality of protection offered by this challenge, if any.
    ///
    /// `auth-int` is ignored: it would require hashing the entity body, which
    /// neither ONVIF devices nor cameras ask for.
    fn qop(&self) -> Option<&str> {
        self.qop.as_deref().and_then(|value| {
            value
                .split(',')
                .map(str::trim)
                .find(|item| item.eq_ignore_ascii_case("auth"))
        })
    }
}

/// Parses a `WWW-Authenticate` header value, ignoring unsupported schemes.
pub fn parse(header: &str) -> Option<Challenge> {
    let (scheme, rest) = header.trim().split_once(char::is_whitespace)?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "digest" && scheme != "basic" {
        return None;
    }
    let params = parse_params(rest);
    let get = |name: &str| {
        params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    Some(Challenge {
        scheme,
        realm: get("realm").unwrap_or_default(),
        nonce: get("nonce").unwrap_or_default(),
        qop: get("qop"),
        opaque: get("opaque"),
        algorithm: get("algorithm"),
    })
}

/// Parses a challenge and immediately answers it.
pub fn authorization(header: &str, method: &str, uri: &str, username: &str, password: &str) -> Option<String> {
    parse(header)?.authorization(method, uri, username, password)
}

/// `Basic` header value.
pub fn basic(username: &str, password: &str) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    format!("Basic {token}")
}

/// Computes the digest `response` value.
fn digest_response(
    challenge: &Challenge,
    method: &str,
    uri: &str,
    username: &str,
    password: &str,
    qop: Option<&str>,
    nc: &str,
    cnonce: &str,
) -> String {
    let session = challenge
        .algorithm
        .as_deref()
        .unwrap_or("MD5")
        .to_ascii_uppercase()
        .ends_with("-SESS");

    let mut ha1 = md5_hex(&format!("{username}:{}:{password}", challenge.realm));
    if session {
        ha1 = md5_hex(&format!("{ha1}:{}:{cnonce}", challenge.nonce));
    }
    let ha2 = md5_hex(&format!("{method}:{uri}"));

    match qop {
        Some(qop) => md5_hex(&format!(
            "{ha1}:{}:{nc}:{cnonce}:{qop}:{ha2}",
            challenge.nonce
        )),
        None => md5_hex(&format!("{ha1}:{}:{ha2}", challenge.nonce)),
    }
}

/// Splits `key=value` pairs on commas that are not inside a quoted string.
fn parse_params(value: &str) -> Vec<(String, String)> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in value.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            ',' if !quoted => items.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    items.push(current);

    items
        .into_iter()
        .filter_map(|item| {
            let (key, raw) = item.split_once('=')?;
            let key = key.trim().to_ascii_lowercase();
            let raw = raw.trim();
            let raw = raw
                .strip_prefix('"')
                .and_then(|text| text.strip_suffix('"'))
                .unwrap_or(raw);
            if key.is_empty() {
                return None;
            }
            Some((key, raw.to_string()))
        })
        .collect()
}

/// Quotes and escapes a digest header value.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        if character == '"' || character == '\\' {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
    out
}

/// Lower case hexadecimal MD5 of a string.
fn md5_hex(value: &str) -> String {
    let digest = Md5::digest(value.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn parses_digest_challenge() {
        let challenge = parse(
            r#"Digest realm="IP Camera", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth", opaque="5ccc069c403ebaf9f0171e9517f40e41""#,
        )
        .unwrap();
        assert_eq!(challenge.scheme, "digest");
        assert_eq!(challenge.realm, "IP Camera");
        assert_eq!(challenge.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
        assert_eq!(challenge.qop.as_deref(), Some("auth"));
        assert_eq!(challenge.opaque.as_deref(), Some("5ccc069c403ebaf9f0171e9517f40e41"));
        assert_eq!(challenge.algorithm, None);
    }

    #[test]
    fn parses_basic_challenge() {
        let challenge = parse(r#"Basic realm="Camera""#).unwrap();
        assert_eq!(challenge.scheme, "basic");
        assert_eq!(challenge.realm, "Camera");
    }

    #[test]
    fn ignores_unsupported_schemes() {
        assert!(parse("Negotiate").is_none());
        assert!(parse("Bearer realm=\"api\"").is_none());
    }

    #[test]
    fn rejects_unsupported_digest_algorithm() {
        let challenge = Challenge {
            scheme: "digest".into(),
            realm: "r".into(),
            nonce: "n".into(),
            qop: None,
            opaque: None,
            algorithm: Some("SHA-256".into()),
        };
        assert!(challenge.authorization("DESCRIBE", "rtsp://host/s", "admin", "secret").is_none());
    }

    #[test]
    fn builds_basic_authorization() {
        // base64("admin:secret")
        assert_eq!(basic("admin", "secret"), "Basic YWRtaW46c2VjcmV0");
    }

    #[test]
    fn digest_response_matches_rfc2617_example() {
        // Canonical example from RFC 2617 section 3.5.
        let challenge = Challenge {
            scheme: "digest".into(),
            realm: "testrealm@host.com".into(),
            nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093".into(),
            qop: Some("auth".into()),
            opaque: None,
            algorithm: None,
        };
        let response = digest_response(
            &challenge,
            "GET",
            "/dir/index.html",
            "Mufasa",
            "Circle Of Life",
            Some("auth"),
            "00000001",
            "0a4f113b",
        );
        assert_eq!(response, "6629fae49393a05397450978507c4ef1");
    }

    #[test]
    fn digest_response_without_qop() {
        // RFC 2069 style: HA1:nonce:HA2.
        let challenge = Challenge {
            scheme: "digest".into(),
            realm: "IP Camera".into(),
            nonce: "abc".into(),
            qop: None,
            opaque: None,
            algorithm: None,
        };
        let ha1 = md5_hex("admin:IP Camera:12345");
        let expected = md5_hex(&format!("{ha1}:abc:{}", md5_hex("DESCRIBE:rtsp://host/stream")));
        let response =
            digest_response(&challenge, "DESCRIBE", "rtsp://host/stream", "admin", "12345", None, "00000001", "ignored");
        assert_eq!(response, expected);
        assert_eq!(response.len(), 32);
    }

    #[test]
    fn builds_digest_authorization_for_an_rtsp_uri() {
        let header = parse(r#"Digest realm="IP Camera", nonce="abc123", qop="auth", algorithm=MD5"#)
            .unwrap()
            .authorization("DESCRIBE", "rtsp://192.168.1.10:554/Streaming/Channels/101", "admin", "secret")
            .unwrap();
        assert!(header.starts_with("Digest "));
        assert!(header.contains(r#"username="admin""#));
        assert!(header.contains(r#"realm="IP Camera""#));
        assert!(header.contains(r#"nonce="abc123""#));
        assert!(header.contains(r#"uri="rtsp://192.168.1.10:554/Streaming/Channels/101""#));
        assert!(header.contains("qop=auth, nc=00000001, cnonce="));
        assert!(header.contains("algorithm=MD5"));
    }

    #[test]
    fn authorization_helper_parses_and_answers() {
        let header = authorization(
            r#"Basic realm="Camera""#,
            "POST",
            "/onvif/device_service",
            "admin",
            "secret",
        )
        .unwrap();
        assert_eq!(header, "Basic YWRtaW46c2VjcmV0");
        assert!(authorization("Negotiate", "POST", "/", "a", "b").is_none());
    }
}
