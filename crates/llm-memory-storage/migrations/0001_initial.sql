CREATE TABLE users (
  id TEXT PRIMARY KEY,
  provider TEXT NOT NULL,
  subject TEXT NOT NULL,
  email TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE(provider, subject)
);

CREATE TABLE shared_memories (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE raws (
  id TEXT PRIMARY KEY,
  scope TEXT NOT NULL CHECK (scope IN ('personal','shared')),
  owner_id TEXT NOT NULL,
  title TEXT NOT NULL,
  content TEXT NOT NULL,
  source TEXT NOT NULL,
  tags TEXT,
  created_by TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_raws_scope_owner ON raws(scope, owner_id);
CREATE INDEX idx_raws_created_at ON raws(created_at);

CREATE TABLE wikis (
  scope TEXT NOT NULL CHECK (scope IN ('personal','shared')),
  owner_id TEXT NOT NULL,
  concept TEXT NOT NULL,
  content TEXT NOT NULL,
  source_refs TEXT NOT NULL,
  last_rebuilt_at INTEGER NOT NULL,
  PRIMARY KEY (scope, owner_id, concept)
);

CREATE TABLE schemas (
  scope TEXT NOT NULL CHECK (scope IN ('personal','shared')),
  owner_id TEXT NOT NULL,
  content TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (scope, owner_id)
);

CREATE TABLE oauth_clients (
  id TEXT PRIMARY KEY,
  redirect_uris TEXT NOT NULL,
  grant_types TEXT NOT NULL,
  token_endpoint_auth_method TEXT NOT NULL,
  client_name TEXT,
  created_at INTEGER NOT NULL,
  last_seen_at INTEGER,
  revoked_at INTEGER
);

CREATE TABLE tokens (
  refresh_token TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  client_id TEXT NOT NULL REFERENCES oauth_clients(id),
  expires_at INTEGER NOT NULL,
  revoked_at INTEGER
);
