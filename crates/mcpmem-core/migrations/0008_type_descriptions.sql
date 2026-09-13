-- One optional description per taxonomy entry. The column is shared by
-- entity types (kind 0) and relation types (kind 1); NULL means "no
-- description". A type can carry a description before any member uses it,
-- so the column is independent of the live count.
ALTER TABLE type_dict ADD COLUMN desc TEXT;