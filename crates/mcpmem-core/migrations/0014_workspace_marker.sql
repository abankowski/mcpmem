-- A pre-workspace binary refuses a graph that has this schema version.
-- The registry checks the owner before it applies this marker.
UPDATE oauth_token SET revoked = 1 WHERE revoked = 0;
UPDATE oauth_code SET spent = 1 WHERE spent = 0;
DELETE FROM oauth_login;
