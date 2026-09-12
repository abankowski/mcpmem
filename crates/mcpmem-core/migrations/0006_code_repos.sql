-- Managed remote repositories for code-symbol indexing. One row per
-- repository key (= code project identifier). Credentials sit in this
-- database, the same trust boundary as the knowledge graph itself.
CREATE TABLE code_repo (
    key             TEXT PRIMARY KEY,
    url             TEXT NOT NULL,
    auth_kind       TEXT NOT NULL CHECK (auth_kind IN ('none','token','ssh')),
    auth_secret     TEXT,
    snippets        INTEGER NOT NULL DEFAULT 0 CHECK (snippets IN (0,1)),
    state           TEXT NOT NULL DEFAULT 'pending'
                    CHECK (state IN ('pending','cloning','indexing','indexed','error','removing')),
    last_error      TEXT,
    last_indexed_us INTEGER
) STRICT;

CREATE INDEX code_repo_state ON code_repo(state);