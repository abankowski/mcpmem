CREATE TABLE IF NOT EXISTS oauth_client(
  client_id      TEXT PRIMARY KEY,
  client_name    TEXT NOT NULL,
  redirect_uris  TEXT NOT NULL,
  source         TEXT NOT NULL,
  created_us     INTEGER NOT NULL,
  last_used_us   INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS oauth_login(
  state            TEXT PRIMARY KEY,
  client_id        TEXT NOT NULL,
  redirect_uri     TEXT NOT NULL,
  client_state     TEXT,
  code_challenge   TEXT NOT NULL,
  resource         TEXT NOT NULL,
  scopes           TEXT NOT NULL,
  upstream_verifier TEXT NOT NULL,
  nonce            TEXT NOT NULL,
  csrf             TEXT NOT NULL,
  principal        TEXT,
  created_us       INTEGER NOT NULL,
  expires_us       INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS oauth_code(
  code_digest    TEXT PRIMARY KEY,
  client_id      TEXT NOT NULL,
  redirect_uri   TEXT NOT NULL,
  code_challenge TEXT NOT NULL,
  resource       TEXT NOT NULL,
  scopes         TEXT NOT NULL,
  principal      TEXT NOT NULL,
  family         TEXT NOT NULL,
  spent          INTEGER NOT NULL DEFAULT 0,
  created_us     INTEGER NOT NULL,
  expires_us     INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS oauth_token(
  token_digest TEXT PRIMARY KEY,
  kind         INTEGER NOT NULL,
  family       TEXT NOT NULL,
  client_id    TEXT NOT NULL,
  principal    TEXT NOT NULL,
  scopes       TEXT NOT NULL,
  resource     TEXT NOT NULL,
  spent        INTEGER NOT NULL DEFAULT 0,
  revoked      INTEGER NOT NULL DEFAULT 0,
  created_us   INTEGER NOT NULL,
  expires_us   INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS oauth_token_family ON oauth_token(family);
CREATE INDEX IF NOT EXISTS oauth_token_expiry ON oauth_token(expires_us);
CREATE INDEX IF NOT EXISTS oauth_code_expiry ON oauth_code(expires_us);
CREATE INDEX IF NOT EXISTS oauth_login_expiry ON oauth_login(expires_us);
