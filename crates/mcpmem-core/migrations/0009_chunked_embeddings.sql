CREATE TABLE chunk_vector (
    profile_id     TEXT    NOT NULL,
    kind           TEXT    NOT NULL CHECK (kind IN ('identity','observation','relation')),
    owner_kind     TEXT    NOT NULL CHECK (owner_kind IN ('entity','relation')),
    owner_id       INTEGER NOT NULL,
    chunk_index    INTEGER NOT NULL,
    type_id        INTEGER NOT NULL,
    owner_revision INTEGER NOT NULL,
    blob           BLOB    NOT NULL,
    created_at_us  INTEGER NOT NULL,
    source         TEXT    NOT NULL,
    PRIMARY KEY (profile_id, kind, owner_kind, owner_id, chunk_index)
) STRICT;

CREATE INDEX chunk_vector_owner ON chunk_vector(profile_id, owner_kind, owner_id);
CREATE INDEX chunk_vector_type ON chunk_vector(profile_id, type_id);

CREATE TABLE chunk_index_job (
    profile_id      TEXT    NOT NULL,
    owner_kind      TEXT    NOT NULL CHECK (owner_kind IN ('entity','relation')),
    owner_id        INTEGER NOT NULL,
    owner_revision  INTEGER NOT NULL,
    operation       TEXT    NOT NULL CHECK (operation IN ('upsert','delete')),
    state           TEXT    NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','leased','held','done','dead')),
    lease_token     TEXT,
    lease_epoch     INTEGER NOT NULL DEFAULT 0,
    lease_until_us  INTEGER NOT NULL DEFAULT 0,
    next_attempt_us INTEGER NOT NULL DEFAULT 0,
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT    CHECK (length(last_error) <= 2048),
    PRIMARY KEY (profile_id, owner_kind, owner_id)
) STRICT;

CREATE INDEX chunk_index_job_due ON chunk_index_job(state, next_attempt_us, lease_until_us);
CREATE INDEX chunk_index_job_owner ON chunk_index_job(owner_kind, owner_id);
