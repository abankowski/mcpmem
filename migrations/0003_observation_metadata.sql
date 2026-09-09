-- Preserve existing immutable created_us; legacy metadata is explicitly unknown.
-- Origin IDs are audit values, not foreign keys: merge deletes the source entity.
ALTER TABLE observation ADD COLUMN origin_entity_id INTEGER;
ALTER TABLE observation ADD COLUMN origin_entity_name TEXT;
ALTER TABLE observation ADD COLUMN occurred_us INTEGER CHECK (occurred_us IS NULL OR occurred_us >= 0);
