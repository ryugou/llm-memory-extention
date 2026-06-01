-- vegapunk-memory-server 専用 DB schema。
-- llm-memory-server (wiki) とは別 SQLite ファイルで運用する。
-- wiki / raw データは vegapunk 側に持つので、ここでは認証/メタ情報のみ。

CREATE TABLE users (
  id TEXT PRIMARY KEY,
  provider TEXT NOT NULL,
  subject TEXT NOT NULL,
  email TEXT,
  -- 対応する vegapunk schema 名。1 user 1 schema 想定。
  -- 値は事前に admin が vegapunk Console / CLI で作成し、ここに UPDATE する。
  vegapunk_schema TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  UNIQUE(provider, subject)
);

CREATE TABLE shared_memories (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  -- 対応する vegapunk schema 名。shared_memory 1 つにつき 1 schema。
  vegapunk_schema TEXT NOT NULL,
  created_at INTEGER NOT NULL
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
  refresh_token_hash BLOB PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  client_id TEXT NOT NULL REFERENCES oauth_clients(id),
  expires_at INTEGER NOT NULL,
  revoked_at INTEGER
);

-- token revocation 系クエリ (= 「ある user / client の全 token を revoke」)
-- は user_id / client_id でフィルタするため、両方に index を張って
-- 行数増加時の full-scan を防ぐ。
CREATE INDEX idx_tokens_user_id ON tokens(user_id);
CREATE INDEX idx_tokens_client_id ON tokens(client_id);
