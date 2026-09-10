//! The `mcpmem` OAuth 2.1 authorization server.
//!
//! `mcpmem` issues its own tokens. An upstream OpenID Connect provider only
//! authenticates the human. Tokens are opaque; the database holds their digest.

pub mod store;

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

/// Constant-time comparison for a digest.
pub fn digest_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}
