//! Issuing, refreshing, revoking and validating this server's own tokens.
//!
//! Tokens are opaque strings. Nothing is encoded in them and nothing is signed:
//! the database holds a digest and the grant beside it, so a token is worth
//! exactly one row lookup and a stolen database copy yields no usable
//! credential. That is the whole reason [`validate`] can answer questions a
//! self-contained token cannot — whether the family has been revoked since it
//! was issued, and whether the row is still there at all.
//!
//! Three rules hold this module together.
//!
//! - **A token is bound to one resource.** Every grant carries the canonical
//!   identifier of the resource the authorization request named, and
//!   [`validate`] compares it against the caller's own. RFC 8707 exists so that
//!   a token issued for one resource cannot be spent at another, and a
//!   comparison that happens only in the store, or only at issuance, is not a
//!   comparison at all: the transport has to make it on every request.
//! - **A refresh token is spent once and rotates.** [`grant_refresh`] goes
//!   through [`crate::store::Store::take_refresh`], which decides *valid*,
//!   *replayed* or *unknown* inside one immediate transaction. A replay means
//!   two parties hold the token and this server cannot tell which one is the
//!   client, so the whole family dies — RFC 9700 section 4.14.2.
//! - **Nothing here reads a clock.** `now_us` is a parameter on every function
//!   that needs one, so a test can observe an expiry instead of waiting for it.

use serde::Serialize;
use thiserror::Error;

use crate::store::{Grant, RefreshOutcome, Store, StoreError, TokenKind};

/// How long an access token stays valid, in microseconds.
///
/// One hour. It is the window in which a token leaked from a proxy log, a
/// crash dump or a client's memory is worth anything, and the refresh token
/// beside it means a client pays nothing for the short life.
pub const ACCESS_TTL_US: i64 = 60 * 60 * 1_000_000;

/// How long a refresh token stays valid, in microseconds.
///
/// Thirty days. It is how long a human goes without seeing the consent page
/// again, so it trades directly against how often they are asked; the rotation
/// in [`grant_refresh`] is what keeps the long life safe.
pub const REFRESH_TTL_US: i64 = 30 * 24 * 60 * 60 * 1_000_000;

/// The RFC 6749 section 5.1 success response.
///
/// `token_type` is a constant because this server issues one type. Serializing
/// it from the struct rather than splicing it into the handler keeps the shape
/// of the response in one place.
#[derive(Clone, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    /// RFC 6750. Always `Bearer`.
    pub token_type: &'static str,
    /// The access token's remaining life in **seconds**, which is the unit RFC
    /// 6749 section 5.1 uses. Every other duration here is microseconds.
    pub expires_in: i64,
    pub refresh_token: String,
    /// The granted scopes, space-delimited, as RFC 6749 section 3.3 spells a
    /// scope list. It is the set the human approved, never the set the client
    /// asked for.
    pub scope: String,
}

/// Redacts both credentials, so a `tracing::debug!(?response)` in a handler
/// cannot put a live token in a log. Every other credential-bearing type in
/// this crate follows the same rule.
impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("refresh_token", &"<redacted>")
            .field("scope", &self.scope)
            .finish()
    }
}

/// Why a token request was not honoured.
///
/// The payload of each variant is the reason, for the log. It is deliberately
/// not what the client is told: [`TokenError::code`] is, and it is one of the
/// four codes RFC 6749 section 5.2 defines. An unauthenticated caller holding
/// a guessed code learns whether it was unknown, expired or simply not theirs
/// from a description written for a developer, and none of those three answers
/// is any of its business.
#[derive(Debug, Error)]
pub enum TokenError {
    /// The request is malformed: a parameter is missing, empty or repeated.
    #[error("the token request is malformed: {0}")]
    InvalidRequest(&'static str),
    /// A grant type this server does not issue.
    #[error("unsupported grant type")]
    UnsupportedGrantType,
    /// The presented grant is not redeemable. Unknown, expired, spent, revoked,
    /// or bound to another client, another redirect URI or another verifier —
    /// one answer for all of them.
    #[error("the presented grant cannot be redeemed: {0}")]
    InvalidGrant(&'static str),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl TokenError {
    /// The RFC 6749 section 5.2 error code this refusal reports.
    ///
    /// The match is exhaustive on purpose: a new refusal must name its own
    /// code, and `invalid_request` is not a safe default for one that means
    /// *this grant is not yours*.
    pub const fn code(&self) -> &'static str {
        match self {
            TokenError::InvalidRequest(_) => "invalid_request",
            TokenError::UnsupportedGrantType => "unsupported_grant_type",
            TokenError::InvalidGrant(_) => "invalid_grant",
            // RFC 6749 section 5.2 has no code for a server fault, and the
            // caller answers 500 rather than 400 for this one. The value is
            // here so the type is total.
            TokenError::Store(_) => "server_error",
        }
    }
}

/// What a client presents to redeem an authorization code.
///
/// Every field is required by RFC 6749 section 4.1.3 and RFC 7636 section 4.5
/// for a public client, and every one of them is checked against what the
/// authorization request stored with the code.
pub struct CodeExchange<'a> {
    pub code: &'a str,
    pub code_verifier: &'a str,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
}

/// What a client presents to refresh a grant. RFC 6749 section 6.
pub struct RefreshExchange<'a> {
    pub refresh_token: &'a str,
    pub client_id: &'a str,
}

/// Redeem an authorization code for a token pair.
///
/// The code is consumed by the read that fetches it, **before** any of the
/// three checks below runs. That is deliberate: a code presented with the wrong
/// verifier has been presented by somebody, and RFC 6749 section 4.1.2 asks for
/// a code used more than once to be refused. Whoever can burn a code this way
/// already holds it, so nothing is given away — and the client's retry is one
/// authorization request, while the alternative leaves a code alive after an
/// attacker has been seen guessing at it.
pub fn grant_authorization_code(
    store: &Store,
    params: &CodeExchange<'_>,
    now_us: i64,
) -> Result<TokenResponse, TokenError> {
    let Some(code) = store.take_code(params.code, now_us)? else {
        return Err(TokenError::InvalidGrant(
            "no live authorization code carries this value",
        ));
    };

    // RFC 7636 section 4.6, in constant time. Both sides are the base64url of
    // a SHA-256 digest, so they are the same length whatever the presented
    // verifier was and the comparison cannot answer early.
    if !crate::digest_eq(
        &crate::s256_challenge(params.code_verifier),
        &code.code_challenge,
    ) {
        return Err(TokenError::InvalidGrant(
            "the code verifier does not match the stored challenge",
        ));
    }
    // Neither of these is a secret — the client identifier is public and the
    // redirect URI is registered — so a plain comparison is the right one.
    // Byte for byte, for the reason `GET /oauth/authorize` gives: a comparison
    // that normalises anything is a comparison an attacker picks the
    // normalisation of.
    if params.client_id != code.grant.client_id {
        return Err(TokenError::InvalidGrant(
            "this code was issued to another client",
        ));
    }
    if params.redirect_uri != code.redirect_uri {
        return Err(TokenError::InvalidGrant(
            "the redirect URI does not match the one the code was issued for",
        ));
    }

    issue(store, &code.grant, now_us)
}

/// Spend a refresh token and issue the next pair in its family.
///
/// The decision is [`crate::store::Store::take_refresh`]'s, taken inside one
/// immediate transaction, so two presentations arriving together cannot both
/// succeed and the second is seen as the replay it is.
pub fn grant_refresh(
    store: &Store,
    params: &RefreshExchange<'_>,
    now_us: i64,
) -> Result<TokenResponse, TokenError> {
    match store.take_refresh(params.refresh_token, now_us)? {
        RefreshOutcome::Unknown => Err(TokenError::InvalidGrant(
            "no live refresh token carries this value",
        )),
        // The store has already revoked the family. Nothing more to do here.
        RefreshOutcome::Replayed => Err(TokenError::InvalidGrant(
            "this refresh token was already spent",
        )),
        RefreshOutcome::Valid(grant) => {
            if params.client_id != grant.client_id {
                // A live refresh token presented by a client it was not issued
                // to is the same signal as a replay: two parties hold it. The
                // token is spent by the call above, so leaving the family alive
                // would only postpone this to the real client's next refresh.
                store.revoke_family(&grant.family)?;
                tracing::warn!(
                    client_id = %grant.client_id,
                    "a refresh token was presented by another client: the family is revoked"
                );
                return Err(TokenError::InvalidGrant(
                    "this refresh token was issued to another client",
                ));
            }
            // The grant is carried forward exactly as the store returned it.
            // Re-deriving the scopes from anywhere else could widen them, and
            // the human who approved them is no longer at the keyboard.
            issue(store, &grant, now_us)
        }
    }
}

/// Revoke the family the presented token belongs to. RFC 7009.
///
/// Either kind of token names the family, and both halves die. Section 2.1
/// permits exactly this, and partial revocation would leave a client that has
/// logged out holding a credential that still works.
///
/// It is idempotent, and a token this server does not recognise is not an
/// error: section 2.2 requires the success answer, so that a client cleaning
/// up need not tell the two cases apart and a guesser learns nothing.
///
/// `now_us` is here because [`crate::store::Store::family_of`] ignores an
/// expired row. A caller presenting an expired access token therefore revokes
/// nothing — which is why a client logging out should present the refresh
/// token, the half that outlives the session.
pub fn revoke(store: &Store, token: &str, now_us: i64) -> Result<(), TokenError> {
    if let Some(family) = store.family_of(token, now_us)? {
        store.revoke_family(&family)?;
    }
    Ok(())
}

/// The grant behind a bearer token presented at `resource`, or `None`.
///
/// `None` covers every reason a token is worthless: the digest is unknown, the
/// row has expired, the family was revoked, the value is a refresh token rather
/// than an access token, or the grant names a different resource.
///
/// The resource comparison is the audience check, and it is the reason this
/// takes the caller's own identifier rather than reading one from anywhere. A
/// token minted by another deployment of this server, against the same
/// database or restored from its backup, carries that deployment's identifier
/// and is refused here.
///
/// A store fault answers `None` and logs. The alternative is a transport that
/// has to decide what a database error means about a credential, and the only
/// safe answer to *I cannot tell* is the same as the answer to *no*.
pub fn validate(store: &Store, token: &str, resource: &str, now_us: i64) -> Option<Grant> {
    let grant = match store.find_access(token, now_us) {
        Ok(grant) => grant?,
        Err(e) => {
            tracing::error!(error = %e, "the OAuth store refused to read a token");
            return None;
        }
    };
    if grant.resource != resource {
        tracing::warn!(
            client_id = %grant.client_id,
            "a token bound to another resource was presented"
        );
        return None;
    }
    Some(grant)
}

/// Mint one access token and one refresh token in `grant`'s family.
///
/// One family per grant, set when the authorization code was minted, so every
/// token descended from one consent decision is revoked together.
fn issue(store: &Store, grant: &Grant, now_us: i64) -> Result<TokenResponse, TokenError> {
    let access_token = crate::new_token();
    let refresh_token = crate::new_token();
    store.put_token(
        &access_token,
        TokenKind::Access,
        grant,
        now_us,
        now_us + ACCESS_TTL_US,
    )?;
    store.put_token(
        &refresh_token,
        TokenKind::Refresh,
        grant,
        now_us,
        now_us + REFRESH_TTL_US,
    )?;
    Ok(TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ACCESS_TTL_US / 1_000_000,
        refresh_token,
        scope: grant.scopes.join(" "),
    })
}
