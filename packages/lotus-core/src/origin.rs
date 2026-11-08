//! Shared origin normalization for Lotus IPC trust and HTTP broker paths.
//!
//! `NormalizeMode::IpcTrust` aligns with browser WHATWG `URL.origin` for
//! http(s)/ws(s)/blob/opaque schemes, while preserving Lotus's custom
//! `lotus-resource://host` serialization (WHATWG would yield `"null"` for that
//! non-special scheme, which would break Track-1 trustedOrigins).
//!
//! `NormalizeMode::BrokerHttp` is intentionally narrow (http/https only) for
//! anti-SSRF egress — it must not accept lotus-resource / ws / opaque origins.
//!
//! Golden vectors: `testdata/origin_golden.json` (kept in sync with
//! `origin_normalize.js` and lotus-dev tests).

use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizeMode {
    IpcTrust,
    BrokerHttp,
}

/// Normalize an origin / URL string.
///
/// Returns `None` for empty / unparseable input (mode-dependent).
/// Opaque schemes under IpcTrust serialize to the string `"null"` (not Rust `None`).
pub fn normalize_origin(raw: &str, mode: NormalizeMode) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    match mode {
        NormalizeMode::IpcTrust => normalize_ipc_trust(trimmed),
        NormalizeMode::BrokerHttp => normalize_broker_http(trimmed),
    }
}

/// Convenience: IpcTrust mode (Track-1 / trust lists / Axum Origin checks).
pub fn normalize_origin_ipc(raw: &str) -> Option<String> {
    normalize_origin(raw, NormalizeMode::IpcTrust)
}

/// Convenience: BrokerHttp mode (NetworkBroker SSRF path).
pub fn normalize_origin_broker(raw: &str) -> Option<String> {
    normalize_origin(raw, NormalizeMode::BrokerHttp)
}

/// Opaque / empty normalized origins must never grant trust via set membership
/// (`"null"` is not a wildcard). Callers still check the trusted set separately.
pub fn origin_grants_trust(normalized: &str) -> bool {
    !normalized.is_empty() && normalized != "null"
}

/// Trust membership helper for HashSet-backed lists (unit tests / JS-parity).
pub fn is_trusted_origin(normalized: &str, trusted: &std::collections::HashSet<String>) -> bool {
    origin_grants_trust(normalized) && trusted.contains(normalized)
}

fn normalize_broker_http(trimmed: &str) -> Option<String> {
    let parsed = Url::parse(trimmed).ok()?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    serialize_special_origin(&parsed)
}

fn normalize_ipc_trust(trimmed: &str) -> Option<String> {
    if trimmed == "null" {
        return Some("null".to_string());
    }

    // Lotus custom scheme — do not use WHATWG origin (which is "null").
    if trimmed.starts_with("lotus-resource:") {
        return Some(normalize_lotus_resource(trimmed));
    }

    let parsed = match Url::parse(trimmed) {
        Ok(u) => u,
        Err(_) => return None,
    };

    let scheme = parsed.scheme().to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" | "ws" | "wss" => serialize_special_origin(&parsed),
        "blob" => normalize_blob_inner(parsed.as_str()),
        "data" | "javascript" | "about" | "file" => Some("null".to_string()),
        _ => Some("null".to_string()),
    }
}

fn normalize_lotus_resource(trimmed: &str) -> String {
    let after_scheme = trimmed
        .trim_start_matches("lotus-resource:")
        .trim_start_matches('/');
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let host = if host.is_empty() {
        "localhost"
    } else {
        host
    };
    format!("lotus-resource://{}", host.to_ascii_lowercase())
}

fn normalize_blob_inner(blob_url: &str) -> Option<String> {
    // blob:<inner-url>
    let inner = blob_url.strip_prefix("blob:")?;
    if inner.starts_with("lotus-resource:") {
        // WHATWG: custom-scheme blob nested origins are opaque.
        return Some("null".to_string());
    }
    if let Ok(inner_url) = Url::parse(inner) {
        let scheme = inner_url.scheme();
        if scheme == "http" || scheme == "https" {
            return serialize_special_origin(&inner_url);
        }
    }
    Some("null".to_string())
}

/// Serialize http(s)/ws(s) like WHATWG `URL.origin`: lowercase host, omit default ports.
fn serialize_special_origin(parsed: &Url) -> Option<String> {
    let scheme = parsed.scheme().to_ascii_lowercase();
    let host = parsed.host_str()?.to_ascii_lowercase();
    // url crate's host_str() for IPv6 is without brackets; WHATWG origin includes them.
    let host_part = match parsed.host() {
        Some(url::Host::Ipv6(addr)) => format!("[{}]", addr.to_string().to_ascii_lowercase()),
        _ => host,
    };

    let port_part = match parsed.port() {
        Some(p) => {
            let is_default = (scheme == "http" && p == 80)
                || (scheme == "https" && p == 443)
                || (scheme == "ws" && p == 80)
                || (scheme == "wss" && p == 443);
            if is_default {
                String::new()
            } else {
                format!(":{}", p)
            }
        }
        None => String::new(),
    };

    Some(format!("{}://{}{}", scheme, host_part, port_part))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;

    fn golden_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/origin_golden.json")
    }

    #[test]
    fn golden_ipc_trust_and_broker_http() {
        let raw = fs::read_to_string(golden_path()).expect("read origin_golden.json");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parse golden json");

        for case in v["ipc_trust"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let expected = if case["output"].is_null() {
                None
            } else {
                Some(case["output"].as_str().unwrap().to_string())
            };
            let got = normalize_origin(input, NormalizeMode::IpcTrust);
            assert_eq!(got, expected, "IpcTrust failed for input {:?}", input);
        }

        for case in v["broker_http"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let expected = if case["output"].is_null() {
                None
            } else {
                Some(case["output"].as_str().unwrap().to_string())
            };
            let got = normalize_origin(input, NormalizeMode::BrokerHttp);
            assert_eq!(got, expected, "BrokerHttp failed for input {:?}", input);
        }
    }

    #[test]
    fn null_origin_never_grants_trust() {
        let mut set = HashSet::new();
        set.insert("null".to_string());
        set.insert("https://example.com".to_string());
        assert!(!is_trusted_origin("null", &set));
        assert!(!is_trusted_origin("", &set));
        assert!(is_trusted_origin("https://example.com", &set));
    }
}
