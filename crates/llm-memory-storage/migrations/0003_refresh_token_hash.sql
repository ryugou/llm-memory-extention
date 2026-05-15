-- Refresh tokens are now stored only as SHA-256 hashes. The plain token is
-- returned to the client once at issuance and never again touches disk.
-- This way a DB / backup leak does not yield directly usable bearer tokens.
--
-- SQLite cannot ALTER TABLE to drop a column or change a primary key, so we
-- rebuild the table. Any pre-existing refresh tokens are invalidated; clients
-- must obtain a fresh token via the OAuth flow.

CREATE TABLE tokens_new (
  refresh_token_hash BLOB PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  client_id TEXT NOT NULL REFERENCES oauth_clients(id),
  expires_at INTEGER NOT NULL,
  revoked_at INTEGER
);

DROP TABLE tokens;
ALTER TABLE tokens_new RENAME TO tokens;
