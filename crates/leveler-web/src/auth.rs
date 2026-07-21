//! Bearer-token authentication for the loopback WebUI server.
//!
//! One 256-bit token authenticates every endpoint. It may arrive as a
//! `?token=` query parameter (how the browser URL printed at startup carries
//! it) or as an `Authorization: Bearer` header. Comparison is constant-time:
//! the token's *length* is not the secret (we mint fixed-length tokens), its
//! *value* is, and equal-length values compare with no early exit so a timing
//! side channel cannot recover the token byte by byte.

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;

/// Constant-time equality for bearer tokens.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Pull the `token=` value out of a raw query string. Tokens are hex, so no
/// percent-decoding is needed.
fn query_token(raw_query: Option<&str>) -> Option<&str> {
    raw_query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
}

/// Pull the token out of an `Authorization: Bearer <token>` header.
fn header_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Whether the request carries the expected token, in either credential slot.
pub(crate) fn is_authorized(raw_query: Option<&str>, headers: &HeaderMap, expected: &str) -> bool {
    let presented = query_token(raw_query).or_else(|| header_token(headers));
    match presented {
        Some(token) => constant_time_eq(token.as_bytes(), expected.as_bytes()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_token_parses_among_other_params() {
        assert_eq!(query_token(Some("session=s1&token=abc123")), Some("abc123"));
        assert_eq!(query_token(Some("token=abc123")), Some("abc123"));
        assert_eq!(query_token(Some("session=s1")), None);
        assert_eq!(query_token(None), None);
        // A `token` value that is a prefix of a later key is not confused.
        assert_eq!(query_token(Some("tokens=abc")), None);
    }

    #[test]
    fn header_token_requires_bearer_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert_eq!(header_token(&headers), Some("abc123"));
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Basic abc123".parse().unwrap());
        assert_eq!(header_token(&headers), None);
    }

    #[test]
    fn authorization_accepts_query_or_header() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert!(is_authorized(None, &headers, "secret"));
        assert!(is_authorized(
            Some("token=secret"),
            &HeaderMap::new(),
            "secret"
        ));
        assert!(!is_authorized(
            Some("token=wrong"),
            &HeaderMap::new(),
            "secret"
        ));
        assert!(!is_authorized(None, &HeaderMap::new(), "secret"));
        // Length mismatch must not match even with a shared prefix.
        assert!(!is_authorized(
            Some("token=secret2"),
            &HeaderMap::new(),
            "secret"
        ));
    }
}
