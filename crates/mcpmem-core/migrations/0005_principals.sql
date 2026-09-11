-- Runtime principals and the approval waitlist. The principals file owns
-- the built-in entries; these tables hold the rest.
CREATE TABLE runtime_principal (
    iss        TEXT NOT NULL,
    sub        TEXT NOT NULL,
    name       TEXT NOT NULL,
    label      TEXT,
    scopes     TEXT NOT NULL,
    created_us INTEGER NOT NULL,
    updated_us INTEGER NOT NULL,
    PRIMARY KEY (iss, sub)
) STRICT;

CREATE TABLE principal_waitlist (
    iss           TEXT NOT NULL,
    sub           TEXT NOT NULL,
    name          TEXT NOT NULL,
    first_seen_us INTEGER NOT NULL,
    last_seen_us  INTEGER NOT NULL,
    PRIMARY KEY (iss, sub)
) STRICT;
