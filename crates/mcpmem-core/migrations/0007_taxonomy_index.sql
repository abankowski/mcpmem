ALTER TABLE type_dict ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE relation ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;

CREATE TABLE taxonomy_relation (
    id INTEGER PRIMARY KEY,
    from_id INTEGER NOT NULL,
    to_id   INTEGER NOT NULL,
    type_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    UNIQUE(from_id, to_id, type_id)
) STRICT;

CREATE TABLE taxonomy_job (
    subject_kind INTEGER NOT NULL CHECK (subject_kind IN (0,1,2)),
    subject_id   INTEGER NOT NULL,
    profile_id   TEXT NOT NULL,
    subject_revision INTEGER NOT NULL,
    operation TEXT NOT NULL CHECK (operation IN ('upsert','delete')),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','leased','held','done','dead')),
    lease_token TEXT,
    lease_epoch INTEGER NOT NULL DEFAULT 0,
    lease_until_us INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_us INTEGER NOT NULL DEFAULT 0,
    last_error TEXT CHECK (length(last_error) <= 2048),
    PRIMARY KEY(subject_kind, subject_id, profile_id)
) STRICT;
CREATE INDEX taxonomy_job_due ON taxonomy_job(state, next_attempt_us, lease_until_us);

CREATE TABLE taxonomy_vector (
    profile_id TEXT NOT NULL,
    subject_kind INTEGER NOT NULL CHECK (subject_kind IN (0,1,2)),
    subject_id INTEGER NOT NULL,
    subject_revision INTEGER NOT NULL,
    blob BLOB NOT NULL,
    created_at_us INTEGER NOT NULL,
    source TEXT NOT NULL,
    PRIMARY KEY(profile_id, subject_kind, subject_id)
) STRICT;

CREATE TABLE taxonomy_ann_generation (
    profile_id TEXT NOT NULL,
    subject_kind INTEGER NOT NULL CHECK (subject_kind IN (0,1,2)),
    durable_generation INTEGER NOT NULL DEFAULT 0,
    published_generation INTEGER NOT NULL DEFAULT -1,
    full_scan_generation INTEGER,
    PRIMARY KEY(profile_id, subject_kind),
    CHECK(published_generation <= durable_generation)
) STRICT;

INSERT INTO taxonomy_relation(id, from_id, to_id, type_id, revision, deleted)
SELECT rowid, from_id, to_id, type_id, 1, 0 FROM relation;