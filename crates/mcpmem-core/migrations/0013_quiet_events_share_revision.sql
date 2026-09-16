-- Quiet events share one structural revision.
--
-- Attribute writes and relation observation writes persist change_event rows
-- without bumping entity_revision: the event carries the current structural
-- revision, and consecutive quiet events on one entity deliberately repeat
-- it. The UNIQUE(entity_id, entity_revision) constraint from migration 0001
-- therefore rejects the second quiet event of any sequence, which would fail
-- the write itself. Rebuild the table without the constraint; immutability
-- stays enforced by the before-update and before-delete triggers.
CREATE TABLE change_event_v3 (
    event_id TEXT PRIMARY KEY,
    transaction_id TEXT NOT NULL,
    entity_id INTEGER NOT NULL,
    entity_revision INTEGER NOT NULL,
    occurred_at_us INTEGER NOT NULL,
    payload TEXT NOT NULL
) STRICT;
INSERT INTO change_event_v3
    SELECT event_id, transaction_id, entity_id, entity_revision, occurred_at_us, payload
    FROM change_event;
DROP TABLE change_event;
ALTER TABLE change_event_v3 RENAME TO change_event;
CREATE TRIGGER change_event_immutable_update BEFORE UPDATE ON change_event
BEGIN SELECT RAISE(ABORT, 'change_event is immutable'); END;
CREATE TRIGGER change_event_immutable_delete BEFORE DELETE ON change_event
BEGIN SELECT RAISE(ABORT, 'change_event is immutable'); END;