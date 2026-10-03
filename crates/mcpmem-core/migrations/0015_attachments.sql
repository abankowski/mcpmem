CREATE TABLE attachment (
    id          INTEGER PRIMARY KEY,
    entity_id   INTEGER NOT NULL,
    filename    TEXT    NOT NULL CHECK (length(trim(filename)) > 0),
    mime        TEXT    NOT NULL,
    size_bytes  INTEGER NOT NULL CHECK (size_bytes >= 0),
    sha256      BLOB    NOT NULL CHECK (length(sha256) = 32),
    content     BLOB    NOT NULL,
    status      TEXT    NOT NULL CHECK (status IN ('uploaded','extracting','ready','error')),
    revision    INTEGER NOT NULL,
    last_error  TEXT,
    error_stage TEXT    CHECK (error_stage IN ('config','render','provider','decode','storage')),
    created_us  INTEGER NOT NULL,
    CHECK ((error_stage IS NULL AND last_error IS NULL)
        OR (error_stage IS NOT NULL AND last_error IS NOT NULL AND status IN ('extracting','error'))),
    CHECK (status != 'error' OR error_stage IS NOT NULL)
) STRICT;

CREATE UNIQUE INDEX attachment_entity_filename ON attachment(entity_id, filename);
CREATE INDEX attachment_entity ON attachment(entity_id);

CREATE TABLE attachment_text (
    attachment_id INTEGER NOT NULL,
    page          INTEGER NOT NULL CHECK (page >= 1),
    text          TEXT    NOT NULL,
    chars         INTEGER NOT NULL CHECK (chars >= 0),
    PRIMARY KEY (attachment_id, page)
) STRICT;

CREATE TABLE attachment_chunk (
    attachment_id INTEGER NOT NULL,
    chunk_index   INTEGER NOT NULL CHECK (chunk_index >= 0),
    page          INTEGER NOT NULL CHECK (page >= 1),
    segment_index INTEGER NOT NULL CHECK (segment_index >= 0),
    text          TEXT    NOT NULL,
    PRIMARY KEY (attachment_id, chunk_index),
    UNIQUE (attachment_id, page, segment_index)
) STRICT;

CREATE TABLE attachment_job (
    attachment_id  INTEGER PRIMARY KEY,
    state          TEXT    NOT NULL CHECK (state IN ('pending','leased','done','dead')),
    lease_token    TEXT,
    lease_epoch    INTEGER NOT NULL,
    lease_until_us INTEGER NOT NULL,
    next_attempt_us INTEGER NOT NULL,
    attempts       INTEGER NOT NULL,
    last_error     TEXT
) STRICT;

CREATE INDEX attachment_job_due ON attachment_job(state, next_attempt_us, lease_until_us);

CREATE TABLE attachment_upload (
    upload_id       TEXT PRIMARY KEY,
    principal_id    TEXT    NOT NULL,
    entity_id       INTEGER NOT NULL,
    filename        TEXT    NOT NULL,
    mime            TEXT    NOT NULL,
    expected_bytes  INTEGER NOT NULL CHECK (expected_bytes >= 0),
    expected_sha256 BLOB    NOT NULL CHECK (length(expected_sha256) = 32),
    received_bytes  INTEGER NOT NULL CHECK (received_bytes >= 0 AND received_bytes <= expected_bytes),
    next_index      INTEGER NOT NULL CHECK (next_index >= 0),
    expires_us      INTEGER NOT NULL,
    attachment_id   INTEGER
) STRICT;

CREATE INDEX attachment_upload_expires ON attachment_upload(expires_us);

CREATE TABLE attachment_upload_chunk (
    upload_id    TEXT    NOT NULL,
    chunk_index INTEGER NOT NULL CHECK (chunk_index >= 0),
    content     BLOB    NOT NULL CHECK (length(content) <= 1048576),
    PRIMARY KEY (upload_id, chunk_index)
) STRICT;

-- Rebuild both vector tables so their owner checks accept attachments.
-- The graph migrator wraps this file and its ledger row in one transaction.
CREATE TABLE chunk_vector_next (
    profile_id     TEXT    NOT NULL,
    kind           TEXT    NOT NULL CHECK (kind IN ('identity','observation','relation','attachment')),
    owner_kind     TEXT    NOT NULL CHECK (owner_kind IN ('entity','relation','attachment')),
    owner_id       INTEGER NOT NULL,
    chunk_index    INTEGER NOT NULL,
    type_id        INTEGER NOT NULL,
    owner_revision INTEGER NOT NULL,
    blob           BLOB    NOT NULL,
    created_at_us  INTEGER NOT NULL,
    source         TEXT    NOT NULL,
    PRIMARY KEY (profile_id, kind, owner_kind, owner_id, chunk_index)
) STRICT;

INSERT INTO chunk_vector_next
    (profile_id, kind, owner_kind, owner_id, chunk_index, type_id, owner_revision, blob, created_at_us, source)
SELECT profile_id, kind, owner_kind, owner_id, chunk_index, type_id, owner_revision, blob, created_at_us, source
FROM chunk_vector;
DROP TABLE chunk_vector;
ALTER TABLE chunk_vector_next RENAME TO chunk_vector;
CREATE INDEX chunk_vector_owner ON chunk_vector(profile_id, owner_kind, owner_id);
CREATE INDEX chunk_vector_type ON chunk_vector(profile_id, type_id);

CREATE TABLE chunk_index_job_next (
    profile_id      TEXT    NOT NULL,
    owner_kind      TEXT    NOT NULL CHECK (owner_kind IN ('entity','relation','attachment')),
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

INSERT INTO chunk_index_job_next
    (profile_id, owner_kind, owner_id, owner_revision, operation, state, lease_token, lease_epoch,
     lease_until_us, next_attempt_us, attempts, last_error)
SELECT profile_id, owner_kind, owner_id, owner_revision, operation, state, lease_token, lease_epoch,
       lease_until_us, next_attempt_us, attempts, last_error
FROM chunk_index_job;
DROP TABLE chunk_index_job;
ALTER TABLE chunk_index_job_next RENAME TO chunk_index_job;
CREATE INDEX chunk_index_job_due ON chunk_index_job(state, next_attempt_us, lease_until_us);
CREATE INDEX chunk_index_job_owner ON chunk_index_job(owner_kind, owner_id);
