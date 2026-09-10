//! The two discovery documents this server publishes.
//!
//! Both are pure functions of the canonical public URL and the advertised
//! scopes. The URL comes from `--public-url`, never from the `Host` header, so
//! a request cannot move the resource identifier or the issuer.

use serde_json::{Value, json};

/// RFC 9728 protected resource metadata. `public_url` carries no trailing slash.
pub fn protected_resource(public_url: &str, scopes: &[&str]) -> Value {
    json!({
        "resource": format!("{public_url}/mcp"),
        "authorization_servers": [public_url],
        "scopes_supported": scopes,
        "bearer_methods_supported": ["header"]
    })
}

/// RFC 8414 authorization server metadata.
pub fn authorization_server(public_url: &str, scopes: &[&str]) -> Value {
    json!({
        "issuer": public_url,
        "authorization_endpoint": format!("{public_url}/oauth/authorize"),
        "token_endpoint": format!("{public_url}/oauth/token"),
        "registration_endpoint": format!("{public_url}/oauth/register"),
        "revocation_endpoint": format!("{public_url}/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
        "scopes_supported": scopes
    })
}
