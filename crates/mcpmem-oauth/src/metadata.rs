//! The two discovery documents this server publishes.
//!
//! Both are pure functions of the canonical public URL and the advertised
//! scopes. The URL comes from `--public-url`, never from the `Host` header, so
//! a request cannot move the resource identifier or the issuer.

use serde_json::{Value, json};

/// RFC 9728 protected resource metadata.
///
/// `resource` is the resource identifier this document describes. RFC 9728
/// section 3.3 requires it to be identical to the identifier the client built
/// the request URL from, so the caller derives it from the request and passes
/// it in; this function never guesses it. `authorization_server` is the issuer
/// identifier, and carries no trailing slash.
pub fn protected_resource(resource: &str, authorization_server: &str, scopes: &[&str]) -> Value {
    json!({
        "resource": resource,
        "authorization_servers": [authorization_server],
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
