CREATE TABLE relation_observation (
    id          INTEGER PRIMARY KEY,
    relation_id INTEGER NOT NULL,
    idx         INTEGER NOT NULL,
    body        TEXT    NOT NULL,
    created_us  INTEGER NOT NULL,
    occurred_us INTEGER CHECK (occurred_us IS NULL OR occurred_us >= 0)
) STRICT;

CREATE INDEX rel_obs_by_relation ON relation_observation(relation_id, idx);

CREATE VIRTUAL TABLE rel_obs_fts
    USING fts5(body, content='relation_observation', content_rowid='id',
               tokenize='unicode61 remove_diacritics 2');

CREATE TRIGGER rel_obs_fts_ai AFTER INSERT ON relation_observation BEGIN
  INSERT INTO rel_obs_fts(rowid, body) VALUES (new.id, new.body);
END;

CREATE TRIGGER rel_obs_fts_bd BEFORE DELETE ON relation_observation BEGIN
  INSERT INTO rel_obs_fts(rel_obs_fts, rowid, body) VALUES ('delete', old.id, '');
END;

CREATE TABLE attribute (
    owner_kind TEXT    NOT NULL CHECK (owner_kind IN ('entity','relation')),
    owner_id   INTEGER NOT NULL,
    key        TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    created_us INTEGER NOT NULL,
    updated_us INTEGER NOT NULL,
    PRIMARY KEY (owner_kind, owner_id, key)
) STRICT;

INSERT OR IGNORE INTO graph_stat(key, value) VALUES
    ('rel_obs_seq', 0), ('relation_obs', 0);