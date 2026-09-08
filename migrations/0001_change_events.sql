CREATE TABLE entity_revision (
    entity_id INTEGER PRIMARY KEY,
    revision INTEGER NOT NULL CHECK (revision > 0),
    deleted INTEGER NOT NULL CHECK (deleted IN (0,1))
) STRICT;
CREATE TABLE change_event (
    event_id TEXT PRIMARY KEY,
    transaction_id TEXT NOT NULL,
    entity_id INTEGER NOT NULL,
    entity_revision INTEGER NOT NULL,
    occurred_at_us INTEGER NOT NULL,
    payload TEXT NOT NULL,
    UNIQUE(entity_id, entity_revision)
) STRICT;
CREATE TRIGGER change_event_immutable_update BEFORE UPDATE ON change_event
BEGIN SELECT RAISE(ABORT, 'change_event is immutable'); END;
CREATE TRIGGER change_event_immutable_delete BEFORE DELETE ON change_event
BEGIN SELECT RAISE(ABORT, 'change_event is immutable'); END;
CREATE TABLE event_outbox (
    delivery_id TEXT PRIMARY KEY,
    event_id TEXT NOT NULL,
    subscription_id TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','leased','done','dead')),
    lease_token TEXT,
    lease_epoch INTEGER NOT NULL DEFAULT 0,
    lease_until_us INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_us INTEGER NOT NULL DEFAULT 0,
    last_error TEXT CHECK (length(last_error) <= 2048),
    UNIQUE(event_id, subscription_id)
) STRICT;
CREATE INDEX event_outbox_due ON event_outbox(state, next_attempt_us, lease_until_us);
CREATE TABLE index_profile (
    id TEXT PRIMARY KEY,
    store_key TEXT NOT NULL,
    fingerprint TEXT NOT NULL UNIQUE,
    definition TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('Rebuilding','Active','Retired'))
) STRICT;
CREATE UNIQUE INDEX one_active_profile ON index_profile(store_key) WHERE state='Active';
CREATE TABLE index_profile_registry (
    store_key TEXT PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('LegacyCompat','Active','Rebuilding','Failed')),
    serving_profile TEXT,
    candidate_profile TEXT,
    failure_reason TEXT,
    CHECK ((state='LegacyCompat' AND serving_profile IS NULL AND candidate_profile IS NULL AND failure_reason IS NULL)
        OR (state='Active' AND serving_profile IS NOT NULL AND candidate_profile IS NULL AND failure_reason IS NULL)
        OR (state='Rebuilding' AND candidate_profile IS NOT NULL AND failure_reason IS NULL)
        OR (state='Failed' AND candidate_profile IS NOT NULL AND failure_reason IS NOT NULL))
) STRICT;
INSERT INTO index_profile_registry(store_key,state) VALUES('default','LegacyCompat');
CREATE TABLE index_job (
    entity_id INTEGER NOT NULL,
    profile_id TEXT NOT NULL,
    entity_revision INTEGER NOT NULL,
    operation TEXT NOT NULL CHECK (operation IN ('upsert','delete')),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','leased','held','done','dead')),
    lease_token TEXT,
    lease_epoch INTEGER NOT NULL DEFAULT 0,
    lease_until_us INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_us INTEGER NOT NULL DEFAULT 0,
    last_error TEXT CHECK (length(last_error) <= 2048),
    PRIMARY KEY(entity_id, profile_id)
) STRICT;
CREATE INDEX index_job_due ON index_job(state, next_attempt_us, lease_until_us);
CREATE TABLE idempotency_record (
    principal_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_fingerprint TEXT NOT NULL,
    response TEXT NOT NULL,
    created_at_us INTEGER NOT NULL,
    PRIMARY KEY(principal_id, idempotency_key)
) STRICT;
CREATE TABLE profile_vector (
    profile_id TEXT NOT NULL,
    entity_id INTEGER NOT NULL,
    entity_revision INTEGER NOT NULL,
    blob BLOB NOT NULL,
    created_at_us INTEGER NOT NULL,
    source TEXT NOT NULL,
    PRIMARY KEY(profile_id, entity_id)
) STRICT;
CREATE TABLE ann_generation (
    profile_id TEXT PRIMARY KEY,
    durable_generation INTEGER NOT NULL DEFAULT 0,
    published_generation INTEGER NOT NULL DEFAULT -1,
    full_scan_generation INTEGER,
    CHECK(published_generation <= durable_generation)
) STRICT;
