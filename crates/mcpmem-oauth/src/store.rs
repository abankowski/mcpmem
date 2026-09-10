//! Durable storage for OAuth clients, logins, authorization codes and tokens.
//!
//! No token or authorization code value reaches SQLite. [`Store::put_token`]
//! and [`Store::put_code`] take the value, hash it with [`crate::digest`], and
//! store the digest alone. A read hashes the presented value and matches on the
//! digest, so a stolen database copy yields no usable token.
//!
//! `oauth_login` is the exception, and it is deliberate: it holds the upstream
//! verifier, the nonce and the CSRF value in the clear, because the callback
//! must replay all three. Those three columns are secret-equivalent. Never log
//! or export a [`LoginRecord`]; its [`std::fmt::Debug`] output redacts them.
//!
//! The four tables arrive with migration `0004_oauth.sql`, registered in
//! `mcpmem_core::events::MIGRATIONS`. A caller opens the connection and runs
//! `mcpmem_core::schema::initialize_database` before it builds a [`Store`].
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use thiserror::Error;

use crate::digest;

/// Everything that can go wrong in the store.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("stored json: {0}")]
    Json(#[from] serde_json::Error),
}

/// The result of a store operation.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Everything an issued credential carries, apart from the credential itself.
/// The credential never enters the database: only its digest does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub client_id: String,
    pub principal: String,
    pub scopes: Vec<String>,
    pub resource: String,
    pub family: String,
}

/// Which credential a stored digest belongs to. The value is the `kind` column.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Access = 0,
    Refresh = 1,
}

impl TokenKind {
    /// The `kind` column value.
    const fn code(self) -> i64 {
        self as i64
    }
}

/// A code grant adds the two values that bind the code to one client exchange.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeGrant {
    pub grant: Grant,
    pub redirect_uri: String,
    pub code_challenge: String,
}

/// What one refresh-token presentation means.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefreshOutcome {
    /// The token was live. Here is its grant, and the token is now spent.
    Valid(Grant),
    /// The token was already spent. The store has revoked the family.
    Replayed,
    /// Unknown, expired or revoked.
    Unknown,
}

/// A registered client. `source` is `dcr` for dynamic registration, or `cimd`
/// for a client identifier metadata document.
// `Deserialize` is deliberately absent. `client_id`, `source`, `created_us` and
// `last_used_us` are server-controlled, so a registration request must never
// deserialize into this shape. A request type carries the two client-supplied
// fields instead.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientRecord {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub source: String,
    pub created_us: i64,
    pub last_used_us: i64,
}

impl ClientRecord {
    /// The `source` of a client this server registered dynamically, under
    /// RFC 7591. The identifier is opaque and server-issued.
    pub const DCR: &'static str = "dcr";
    /// The `source` of a client whose metadata this server fetched from the
    /// https URL the client presents as its identifier.
    pub const CIMD: &'static str = "cimd";
}

/// One authorization request in flight, keyed by the state this server sent
/// upstream. `client_state` is the state the client sent to this server, which
/// is opaque here and returned unchanged.
#[derive(Clone, Eq, PartialEq)]
pub struct LoginRecord {
    pub state: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub client_state: Option<String>,
    pub code_challenge: String,
    pub resource: String,
    pub scopes: Vec<String>,
    pub upstream_verifier: String,
    pub nonce: String,
    pub csrf: String,
    pub principal: Option<String>,
    pub created_us: i64,
    pub expires_us: i64,
}

/// Prints the routing fields and redacts the three secret-equivalent ones, so
/// that `tracing::debug!(?login)` in a route handler cannot leak them.
impl std::fmt::Debug for LoginRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginRecord")
            .field("state", &self.state)
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("client_state", &self.client_state)
            .field("code_challenge", &self.code_challenge)
            .field("resource", &self.resource)
            .field("scopes", &self.scopes)
            .field("upstream_verifier", &"<redacted>")
            .field("nonce", &"<redacted>")
            .field("csrf", &"<redacted>")
            .field("principal", &self.principal)
            .field("created_us", &self.created_us)
            .field("expires_us", &self.expires_us)
            .finish()
    }
}

/// The columns a grant occupies, in the order every query selects them.
type GrantColumns = (String, String, String, String, String);

/// Read the five grant columns by name, so no query depends on the order of
/// its select list.
fn grant_columns(r: &Row<'_>) -> rusqlite::Result<GrantColumns> {
    Ok((
        r.get("client_id")?,
        r.get("principal")?,
        r.get("scopes")?,
        r.get("resource")?,
        r.get("family")?,
    ))
}

fn grant_from_columns(
    (client_id, principal, scopes, resource, family): GrantColumns,
) -> Result<Grant> {
    Ok(Grant {
        client_id,
        principal,
        scopes: serde_json::from_str(&scopes)?,
        resource,
        family,
    })
}

/// A `BEGIN IMMEDIATE` guard. The write lock is taken up front, so a caller's
/// busy timeout applies to lock acquisition rather than to the first write. An
/// early return rolls the whole statement group back. [`Store::new`] states the
/// precondition that makes the timeout part true.
struct Tx<'a> {
    conn: &'a Connection,
    done: bool,
}

impl<'a> Tx<'a> {
    fn begin(conn: &'a Connection) -> Result<Self> {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(Self { conn, done: false })
    }

    fn commit(mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        self.done = true;
        Ok(())
    }
}

impl Drop for Tx<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

/// The OAuth tables over one owned connection.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Take ownership of a connection whose schema is already migrated.
    ///
    /// # Precondition
    ///
    /// The caller must set `PRAGMA busy_timeout` before it builds a `Store`.
    /// [`Store::take_refresh`] takes the write lock with `BEGIN IMMEDIATE`, and
    /// without a busy timeout a second concurrent presentation of one refresh
    /// token fails with `SQLITE_BUSY` instead of reporting the replay, so the
    /// token family stays live. Every other component here sets the timeout
    /// right after it opens the connection; the default is 5000 ms, at
    /// `mcpmem_core::storage`.
    pub const fn new(conn: Connection) -> Store {
        Store { conn }
    }

    /// The connection, for tests that assert on the stored rows themselves.
    #[doc(hidden)]
    pub const fn connection(&self) -> &Connection {
        &self.conn
    }

    // ── Clients ───────────────────────────────────────────────────────────

    /// Insert the client, or refresh the metadata of one already registered.
    /// A repeat registration never rewrites `created_us`.
    pub fn put_client(&self, c: &ClientRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO oauth_client(
                 client_id, client_name, redirect_uris, source, created_us, last_used_us)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(client_id) DO UPDATE SET
                 client_name   = excluded.client_name,
                 redirect_uris = excluded.redirect_uris,
                 source        = excluded.source,
                 last_used_us  = excluded.last_used_us",
            params![
                c.client_id,
                c.client_name,
                serde_json::to_string(&c.redirect_uris)?,
                c.source,
                c.created_us,
                c.last_used_us,
            ],
        )?;
        Ok(())
    }

    /// The registered client, or `None` when the identifier is unknown.
    pub fn get_client(&self, client_id: &str) -> Result<Option<ClientRecord>> {
        let row: Option<(String, String, String, i64, i64)> = self
            .conn
            .query_row(
                "SELECT client_name, redirect_uris, source, created_us, last_used_us
                 FROM oauth_client WHERE client_id = ?1",
                params![client_id],
                |r| {
                    Ok((
                        r.get("client_name")?,
                        r.get("redirect_uris")?,
                        r.get("source")?,
                        r.get("created_us")?,
                        r.get("last_used_us")?,
                    ))
                },
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((client_name, redirect_uris, source, created_us, last_used_us)) => {
                Ok(Some(ClientRecord {
                    client_id: client_id.to_string(),
                    client_name,
                    redirect_uris: serde_json::from_str(&redirect_uris)?,
                    source,
                    created_us,
                    last_used_us,
                }))
            }
        }
    }

    /// Record that the client presented itself. An unknown client is a no-op.
    pub fn touch_client(&self, client_id: &str, now_us: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE oauth_client SET last_used_us = ?2 WHERE client_id = ?1",
            params![client_id, now_us],
        )?;
        Ok(())
    }

    // ── Logins ────────────────────────────────────────────────────────────

    /// Store one authorization request before the upstream redirect.
    pub fn put_login(&self, l: &LoginRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO oauth_login(
                 state, client_id, redirect_uri, client_state, code_challenge, resource,
                 scopes, upstream_verifier, nonce, csrf, principal, created_us, expires_us)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                l.state,
                l.client_id,
                l.redirect_uri,
                l.client_state,
                l.code_challenge,
                l.resource,
                serde_json::to_string(&l.scopes)?,
                l.upstream_verifier,
                l.nonce,
                l.csrf,
                l.principal,
                l.created_us,
                l.expires_us,
            ],
        )?;
        Ok(())
    }

    /// Consume the login. The `DELETE ... RETURNING` makes a second upstream
    /// callback with the same state find nothing. An expired login is never
    /// returned; the sweep removes its row later.
    pub fn take_login(&self, state: &str, now_us: i64) -> Result<Option<LoginRecord>> {
        type Columns = (
            String,
            String,
            Option<String>,
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            i64,
            i64,
        );
        let row: Option<Columns> = self
            .conn
            .prepare(
                "DELETE FROM oauth_login WHERE state = ?1 AND expires_us > ?2
                 RETURNING client_id, redirect_uri, client_state, code_challenge, resource,
                           scopes, upstream_verifier, nonce, csrf, principal,
                           created_us, expires_us",
            )?
            .query_row(params![state, now_us], |r| {
                Ok((
                    r.get("client_id")?,
                    r.get("redirect_uri")?,
                    r.get("client_state")?,
                    r.get("code_challenge")?,
                    r.get("resource")?,
                    r.get("scopes")?,
                    r.get("upstream_verifier")?,
                    r.get("nonce")?,
                    r.get("csrf")?,
                    r.get("principal")?,
                    r.get("created_us")?,
                    r.get("expires_us")?,
                ))
            })
            .optional()?;
        let Some((
            client_id,
            redirect_uri,
            client_state,
            code_challenge,
            resource,
            scopes,
            upstream_verifier,
            nonce,
            csrf,
            principal,
            created_us,
            expires_us,
        )) = row
        else {
            return Ok(None);
        };
        Ok(Some(LoginRecord {
            state: state.to_string(),
            client_id,
            redirect_uri,
            client_state,
            code_challenge,
            resource,
            scopes: serde_json::from_str(&scopes)?,
            upstream_verifier,
            nonce,
            csrf,
            principal,
            created_us,
            expires_us,
        }))
    }

    // ── Authorization codes ───────────────────────────────────────────────

    /// Store the digest of an authorization code. The value stays with the
    /// caller, which sends it to the client and then forgets it.
    pub fn put_code(
        &self,
        code: &str,
        g: &CodeGrant,
        created_us: i64,
        expires_us: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO oauth_code(
                 code_digest, client_id, redirect_uri, code_challenge, resource,
                 scopes, principal, family, created_us, expires_us)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                digest(code),
                g.grant.client_id,
                g.redirect_uri,
                g.code_challenge,
                g.grant.resource,
                serde_json::to_string(&g.grant.scopes)?,
                g.grant.principal,
                g.grant.family,
                created_us,
                expires_us,
            ],
        )?;
        Ok(())
    }

    /// Redeem an authorization code. The row is deleted in the statement that
    /// reads it, so a replay finds nothing. An expired code is never returned.
    pub fn take_code(&self, code: &str, now_us: i64) -> Result<Option<CodeGrant>> {
        let row: Option<(GrantColumns, String, String)> = self
            .conn
            .prepare(
                "DELETE FROM oauth_code WHERE code_digest = ?1 AND expires_us > ?2
                 RETURNING client_id, principal, scopes, resource, family,
                           redirect_uri, code_challenge",
            )?
            .query_row(params![digest(code), now_us], |r| {
                Ok((
                    grant_columns(r)?,
                    r.get("redirect_uri")?,
                    r.get("code_challenge")?,
                ))
            })
            .optional()?;
        let Some((columns, redirect_uri, code_challenge)) = row else {
            return Ok(None);
        };
        Ok(Some(CodeGrant {
            grant: grant_from_columns(columns)?,
            redirect_uri,
            code_challenge,
        }))
    }

    // ── Tokens ────────────────────────────────────────────────────────────

    /// Store the digest of an issued token. The value never reaches SQLite.
    pub fn put_token(
        &self,
        token: &str,
        kind: TokenKind,
        g: &Grant,
        created_us: i64,
        expires_us: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO oauth_token(
                 token_digest, kind, family, client_id, principal, scopes, resource,
                 spent, revoked, created_us, expires_us)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, ?8, ?9)",
            params![
                digest(token),
                kind.code(),
                g.family,
                g.client_id,
                g.principal,
                serde_json::to_string(&g.scopes)?,
                g.resource,
                created_us,
                expires_us,
            ],
        )?;
        Ok(())
    }

    /// The grant behind a bearer token, or `None` when the digest is unknown,
    /// the token has expired, its family was revoked, or it is a refresh token.
    pub fn find_access(&self, token: &str, now_us: i64) -> Result<Option<Grant>> {
        let row: Option<GrantColumns> = self
            .conn
            .query_row(
                "SELECT client_id, principal, scopes, resource, family FROM oauth_token
                 WHERE token_digest = ?1 AND kind = ?2 AND revoked = 0 AND expires_us > ?3",
                params![digest(token), TokenKind::Access.code(), now_us],
                grant_columns,
            )
            .optional()?;
        row.map(grant_from_columns).transpose()
    }

    /// Spend a refresh token, once. One immediate transaction reads the row and
    /// writes the decision, so two concurrent presentations cannot both succeed.
    /// A token already spent is a replay: the whole family dies, which ends both
    /// the stolen and the legitimate branch, as RFC 9700 requires.
    pub fn take_refresh(&self, token: &str, now_us: i64) -> Result<RefreshOutcome> {
        let token_digest = digest(token);
        let tx = Tx::begin(&self.conn)?;
        let row: Option<(i64, GrantColumns)> = self
            .conn
            .query_row(
                "SELECT spent, client_id, principal, scopes, resource, family
                 FROM oauth_token
                 WHERE token_digest = ?1 AND kind = ?2 AND revoked = 0 AND expires_us > ?3",
                params![token_digest, TokenKind::Refresh.code(), now_us],
                |r| Ok((r.get("spent")?, grant_columns(r)?)),
            )
            .optional()?;
        let outcome = match row {
            None => RefreshOutcome::Unknown,
            Some((spent, columns)) => {
                let grant = grant_from_columns(columns)?;
                if spent == 0 {
                    self.conn.execute(
                        "UPDATE oauth_token SET spent = 1 WHERE token_digest = ?1",
                        params![token_digest],
                    )?;
                    RefreshOutcome::Valid(grant)
                } else {
                    let killed = self.conn.execute(
                        "UPDATE oauth_token SET revoked = 1 WHERE family = ?1",
                        params![grant.family],
                    )?;
                    tracing::warn!(
                        client_id = %grant.client_id,
                        revoked = killed,
                        "refresh token replay: the token family is revoked"
                    );
                    RefreshOutcome::Replayed
                }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Revoke every token in a family. Used by the revocation endpoint and by
    /// replay detection.
    pub fn revoke_family(&self, family: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE oauth_token SET revoked = 1 WHERE family = ?1",
            params![family],
        )?;
        Ok(())
    }

    /// The family of a live token, whatever else its state.
    ///
    /// Spent and revoked rows still answer, because the revocation endpoint
    /// must find the family of a token it has already spent, and revocation
    /// must stay idempotent. An expired token answers `None`: it is worthless
    /// to its holder, and letting it name a family would let that holder revoke
    /// the live family of the same principal.
    pub fn family_of(&self, token: &str, now_us: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT family FROM oauth_token
                 WHERE token_digest = ?1 AND expires_us > ?2",
                params![digest(token), now_us],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Delete expired logins, codes and tokens, and return how many rows went.
    /// `oauth_client` has no expiry: a registration lives until it is revoked.
    pub fn sweep(&self, now_us: i64) -> Result<u64> {
        let tx = Tx::begin(&self.conn)?;
        let mut removed = 0u64;
        for table in ["oauth_login", "oauth_code", "oauth_token"] {
            let sql = format!("DELETE FROM {table} WHERE expires_us <= ?1");
            removed += self.conn.execute(&sql, params![now_us])? as u64;
        }
        tx.commit()?;
        Ok(removed)
    }
}
