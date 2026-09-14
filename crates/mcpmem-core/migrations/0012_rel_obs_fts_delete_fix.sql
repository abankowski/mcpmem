-- Repair obs_fts_bd on databases created before the delete-body fix: the
-- external-content FTS5 delete command must name the original body, or the
-- posting survives the row deletion and a later MATCH can report the database
-- image as malformed. Fresh databases get the fixed trigger from the bootstrap
-- in schema.rs; this migration repairs existing ones.
DROP TRIGGER obs_fts_bd;
CREATE TRIGGER obs_fts_bd BEFORE DELETE ON observation BEGIN
  INSERT INTO obs_fts(obs_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;