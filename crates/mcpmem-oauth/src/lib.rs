//! The `mcpmem` OAuth 2.1 authorization server.
//!
//! `mcpmem` issues its own tokens. An upstream OpenID Connect provider only
//! authenticates the human. Tokens are opaque; the database holds their digest.

/// The consent page and the authorization code it produces. No HTTP and no
/// template engine: the page is two compiled-in files and one substitution
/// pass, so a build without an HTTP client still carries the whole decision.
pub mod consent;
/// Per-peer request limits for the endpoints that answer an anonymous caller.
pub mod limits;
pub mod metadata;
pub mod registration;
pub mod store;
/// Issuing, refreshing, revoking and validating the tokens this server mints.
pub mod token;
/// The upstream OpenID Connect provider that authenticates the human.
///
/// Behind a feature because it is the only part of this crate that speaks
/// HTTP, and `mcpmem` must stay buildable with no HTTP client at all.
#[cfg(feature = "upstream")]
pub mod upstream;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of a token value. The store holds this, never the value.
///
/// This delegates to `mcpmem_core::events::sha256`, the same primitive that
/// computes the migration checksums verified at every startup. One hash
/// spelling in the workspace, and the `mcpmem-core` dependency states a fact.
pub fn digest(token: &str) -> String {
    mcpmem_core::events::sha256(token.as_bytes())
}

/// 32 random bytes, base64url without padding. Panics only when the operating
/// system random source fails, which is not a recoverable condition.
pub fn new_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("operating system random source");
    URL_SAFE_NO_PAD.encode(buf)
}

/// The S256 PKCE challenge for a verifier, base64url without padding.
pub fn s256_challenge(verifier: &str) -> String {
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(h.finalize())
}

/// The reserved client id of this server's own admin UI, seeded at OAuth
/// startup. The UI is a public PKCE client of this server's AS.
pub const ADMIN_CLIENT_ID: &str = "mcpmem-admin-ui";

/// The reserved client id of this server's own graph viewer, seeded at OAuth
/// startup like the admin UI. The viewer is a public PKCE client of this
/// server's AS, so a human who opens `/ui` on an OAuth server completes the
/// same login the admin SPA does.
pub const GRAPH_CLIENT_ID: &str = "mcpmem-graph-ui";

/// The id of a principal in the admin API: base64url of `iss\0sub`.
///
/// One path segment, so a route never needs to split an issuer URL.
pub fn principal_id(iss: &str, sub: &str) -> String {
    URL_SAFE_NO_PAD.encode(format!("{iss}\0{sub}"))
}

/// Split a [`principal_id`] back into `(iss, sub)`, or `None` for
/// anything this server did not issue.
pub fn parse_principal_id(id: &str) -> Option<(String, String)> {
    let raw = URL_SAFE_NO_PAD.decode(id.as_bytes()).ok()?;
    let sep = raw.iter().position(|&b| b == 0)?;
    let iss = std::str::from_utf8(&raw[..sep]).ok()?;
    let sub = std::str::from_utf8(&raw[sep + 1..]).ok()?;
    if sub.is_empty() {
        return None;
    }
    Some((iss.to_owned(), sub.to_owned()))
}

/// Constant-time comparison for a digest.
pub fn digest_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_ids_round_trip() {
        let id = principal_id("https://accounts.google.com", "u-1");
        assert_eq!(
            parse_principal_id(&id),
            Some(("https://accounts.google.com".to_owned(), "u-1".to_owned()))
        );
        assert_eq!(parse_principal_id("not-base64!"), None);
        // A sub of zero length is refused.
        assert_eq!(parse_principal_id(&principal_id("iss", "")), None);
    }
}
