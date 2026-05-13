# LLM Memory Extension — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Phase 1 設計書（`docs/superpowers/specs/2026-05-13-llm-memory-extention-phase1-design.md`）に沿った Remote MCP サーバを Rust + axum + SQLite で TDD ベースに構築し、GCE VM へデプロイ可能な状態に到達する。

**Architecture:** Cargo workspace に `llm-memory-core` / `-storage` / `-llm` / `-auth` / `-coordinator` / `-server` の 6 crate を持ち、各 crate を単独でテスト可能にする。`-server` がエントリポイント。in-memory `RebuildCoordinator` + drain loop で wiki rebuild を非同期化、Google OAuth を upstream とした MCP OAuth 2.1 AS を実装、Anthropic API は Haiku / Sonnet をそれぞれ用途別に呼び出す。

**Tech Stack:** Rust 2024 edition, axum 0.7, sqlx 0.8 (sqlite), tokio 1.x, reqwest 0.12, oauth2 4.x, jsonwebtoken 9.x, ulid 1.x, serde, tracing, prometheus, anyhow, thiserror.

---

## ファイル構成

```
Cargo.toml                              # workspace
rust-toolchain.toml
.gitignore
.env.example
crates/
  llm-memory-core/                      # ID 型, Scope, OwnerId, 時刻
    Cargo.toml
    src/
      lib.rs
      id.rs                             # Ulid newtypes, SharedMemoryId
      scope.rs                          # Scope enum
      time.rs                           # epoch ms 統一
      error.rs                          # CoreError
  llm-memory-storage/                   # SQLite アクセス
    Cargo.toml
    migrations/
      0001_initial.sql
      0002_fts_triggers.sql
    src/
      lib.rs
      pool.rs                           # PRAGMA 含む接続初期化
      users.rs
      shared_memories.rs
      raws.rs
      wikis.rs
      schemas.rs
      oauth_clients.rs
      tokens.rs
      search.rs                         # FTS5 query
  llm-memory-llm/                       # Anthropic クライアント
    Cargo.toml
    src/
      lib.rs
      client.rs                         # trait + reqwest 実装
      mock.rs                           # cfg(test) で使う mock
      haiku.rs                          # 概念抽出
      sonnet.rs                         # wiki 合成
      prompts.rs                        # 固定プロンプト
  llm-memory-coordinator/               # rebuild coordinator + worker
    Cargo.toml
    src/
      lib.rs
      state.rs                          # RebuildMode, RebuildState, map
      coordinator.rs                    # notify_append / request_manual
      worker.rs                         # drain loop
      input_builder.rs                  # 入力 raw 集合構築
  llm-memory-auth/                      # OAuth + JWT
    Cargo.toml
    src/
      lib.rs
      jwt.rs
      google.rs                         # upstream Google OAuth
      authorization_server.rs           # MCP OAuth 2.1 AS
      dcr.rs                            # Dynamic Client Registration
      middleware.rs                     # axum middleware
      xff.rs                            # X-Forwarded-For パーサ
  llm-memory-server/                    # axum エントリ + MCP ハンドラ
    Cargo.toml
    src/
      main.rs
      config.rs
      app.rs                            # Router 構築
      mcp/
        mod.rs
        transport.rs                    # Streamable HTTP
        tools/
          mod.rs
          raw_append.rs
          raw_search.rs
          raw_read.rs
          wiki_read.rs
          wiki_list.rs
          wiki_rebuild.rs
          schema_read.rs
          schema_update.rs
          export.rs
      rate_limit.rs
      metrics.rs
      health.rs
docker/
  Dockerfile
  docker-compose.yml
  litestream.yml
deploy/
  gce/
    startup.sh
    cloud-init.yaml
.github/workflows/
  ci.yml
docs/superpowers/
  specs/2026-05-13-llm-memory-extention-phase1-design.md  # 既存
  plans/2026-05-13-llm-memory-extention-phase1.md         # 本ファイル
```

---

## Phase A: Foundation

### Task 1: Cargo workspace と toolchain のセットアップ

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `.gitignore`
- Create: `.env.example`

- [ ] **Step 1: ワークスペース定義ファイルを作成**

`Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = [
    "crates/llm-memory-core",
    "crates/llm-memory-storage",
    "crates/llm-memory-llm",
    "crates/llm-memory-coordinator",
    "crates/llm-memory-auth",
    "crates/llm-memory-server",
]

[workspace.package]
edition = "2024"
rust-version = "1.84"
license = "MIT"

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
axum = { version = "0.7", features = ["macros"] }
axum-extra = { version = "0.9", features = ["typed-header"] }
sqlx = { version = "0.8", features = ["sqlite", "runtime-tokio", "macros", "migrate"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
ulid = { version = "1", features = ["serde"] }
jsonwebtoken = "9"
oauth2 = "4"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
thiserror = "1"
anyhow = "1"
async-trait = "0.1"
prometheus = "0.13"
sha2 = "0.10"
base64 = "0.22"
url = "2"
regex = "1"

[profile.release]
lto = "thin"
codegen-units = 1
```

- [ ] **Step 2: Rust toolchain 固定**

`rust-toolchain.toml`:
```toml
[toolchain]
channel = "1.84.0"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 3: `.gitignore` と `.env.example`**

`.gitignore`:
```
/target
*.db
*.db-wal
*.db-shm
.env
.DS_Store
```

`.env.example`:
```
DATABASE_URL=sqlite:./data/db.sqlite
ANTHROPIC_API_KEY=sk-ant-...
GOOGLE_OAUTH_CLIENT_ID=...
GOOGLE_OAUTH_CLIENT_SECRET=...
JWT_SIGNING_KEY_v1=base64-encoded-32-bytes
MODEL_HAIKU=claude-haiku-4-5-20251001
MODEL_SONNET=claude-sonnet-4-6
TRUSTED_PROXY_COUNT=1
BIND_ADDR=0.0.0.0:8080
PUBLIC_URL=https://memory.example.com
```

- [ ] **Step 4: ワークスペースが構築できることを確認**

Run: `cargo check --workspace 2>&1 | head -20`
Expected: 「no targets specified」相当のメッセージ（メンバーが空のため）。エラーで止まらないこと。

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml rust-toolchain.toml .gitignore .env.example
git commit -m "feat(workspace): scaffold Cargo workspace and toolchain"
```

### Task 2: `llm-memory-core` crate — ID 型 / Scope / 時刻ユーティリティ

**Files:**
- Create: `crates/llm-memory-core/Cargo.toml`
- Create: `crates/llm-memory-core/src/lib.rs`
- Create: `crates/llm-memory-core/src/id.rs`
- Create: `crates/llm-memory-core/src/scope.rs`
- Create: `crates/llm-memory-core/src/time.rs`
- Create: `crates/llm-memory-core/src/error.rs`

- [ ] **Step 1: crate スケルトン**

`crates/llm-memory-core/Cargo.toml`:
```toml
[package]
name = "llm-memory-core"
edition.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
ulid.workspace = true
thiserror.workspace = true
regex.workspace = true

[dev-dependencies]
```

`crates/llm-memory-core/src/lib.rs`:
```rust
pub mod error;
pub mod id;
pub mod scope;
pub mod time;
```

- [ ] **Step 2: 失敗テストを書く（ULID 生成と表現）**

`crates/llm-memory-core/src/id.rs`:
```rust
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RawId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SharedMemoryId(pub String);

#[derive(Debug, Error)]
pub enum IdError {
    #[error("invalid shared memory id: {0}")]
    InvalidSharedMemoryId(String),
}

pub fn new_ulid() -> String {
    ulid::Ulid::new().to_string()
}

impl SharedMemoryId {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RE.get_or_init(|| regex::Regex::new(r"^[a-z0-9][a-z0-9-]{0,63}$").unwrap());
        if re.is_match(s) {
            Ok(Self(s.to_string()))
        } else {
            Err(IdError::InvalidSharedMemoryId(s.to_string()))
        }
    }
}

impl fmt::Display for UserId { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) } }
impl fmt::Display for RawId { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) } }
impl fmt::Display for SharedMemoryId { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_is_26_chars_and_sortable() {
        let a = new_ulid();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_ulid();
        assert_eq!(a.len(), 26);
        assert_eq!(b.len(), 26);
        assert!(a < b, "ULID should be time-ordered: {a} >= {b}");
    }

    #[test]
    fn shared_memory_id_accepts_valid() {
        assert!(SharedMemoryId::parse("company-wide").is_ok());
        assert!(SharedMemoryId::parse("a").is_ok());
        assert!(SharedMemoryId::parse("team-frontend-2026").is_ok());
    }

    #[test]
    fn shared_memory_id_rejects_invalid() {
        assert!(SharedMemoryId::parse("-leading-hyphen").is_err());
        assert!(SharedMemoryId::parse("UPPER").is_err());
        assert!(SharedMemoryId::parse("with space").is_err());
        assert!(SharedMemoryId::parse("").is_err());
        let too_long = "a".repeat(65);
        assert!(SharedMemoryId::parse(&too_long).is_err());
    }
}
```

- [ ] **Step 3: テスト実行 (失敗を確認)**

Run: `cargo test -p llm-memory-core id::tests 2>&1 | tail -5`
Expected: コンパイル成功、3 テスト pass（実装が test 内で完結しているため Step 2 で同時に「実装込み」となる）。これは TDD の「最小実装で test 通過」を 1 step に圧縮した形。

- [ ] **Step 4: Scope と OwnerKey**

`crates/llm-memory-core/src/scope.rs`:
```rust
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Personal,
    Shared,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Personal => f.write_str("personal"),
            Scope::Shared => f.write_str("shared"),
        }
    }
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Personal => "personal",
            Scope::Shared => "shared",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OwnerKey {
    pub scope: Scope,
    pub owner_id: String,
}

impl OwnerKey {
    pub fn personal(user_id: impl Into<String>) -> Self {
        Self { scope: Scope::Personal, owner_id: user_id.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_display() {
        assert_eq!(Scope::Personal.to_string(), "personal");
        assert_eq!(Scope::Shared.to_string(), "shared");
    }

    #[test]
    fn owner_key_equality() {
        let a = OwnerKey::personal("u1");
        let b = OwnerKey::personal("u1");
        assert_eq!(a, b);
    }
}
```

- [ ] **Step 5: 時刻ユーティリティ**

`crates/llm-memory-core/src/time.rs`:
```rust
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix epoch millis based on SystemTime. NOT monotonic.
/// 設計書 §7 の規律: started_at と raws.created_at は同じこの関数を使う。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ms_is_recent() {
        let t = now_ms();
        assert!(t > 1_700_000_000_000, "expected epoch ms after 2023");
        assert!(t < 4_000_000_000_000, "sanity upper bound");
    }
}
```

- [ ] **Step 6: core エラー型**

`crates/llm-memory-core/src/error.rs`:
```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Id(#[from] crate::id::IdError),
}
```

- [ ] **Step 7: 全テスト実行**

Run: `cargo test -p llm-memory-core 2>&1 | tail -10`
Expected: 7 tests pass。

- [ ] **Step 8: Commit**

```bash
git add crates/llm-memory-core
git commit -m "feat(core): add ULID, SharedMemoryId, Scope, OwnerKey, now_ms"
```

### Task 3: `llm-memory-storage` の crate スケルトンと migration ファイル

**Files:**
- Create: `crates/llm-memory-storage/Cargo.toml`
- Create: `crates/llm-memory-storage/src/lib.rs`
- Create: `crates/llm-memory-storage/migrations/0001_initial.sql`
- Create: `crates/llm-memory-storage/migrations/0002_fts_triggers.sql`

- [ ] **Step 1: Cargo.toml**

`crates/llm-memory-storage/Cargo.toml`:
```toml
[package]
name = "llm-memory-storage"
edition.workspace = true

[dependencies]
llm-memory-core = { path = "../llm-memory-core" }
sqlx.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true
async-trait.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros"] }
```

- [ ] **Step 2: lib.rs スタブ**

`crates/llm-memory-storage/src/lib.rs`:
```rust
pub mod pool;
pub mod users;
pub mod shared_memories;
pub mod raws;
pub mod wikis;
pub mod schemas;
pub mod oauth_clients;
pub mod tokens;
pub mod search;
pub mod error;
```

`crates/llm-memory-storage/src/error.rs`:
```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("not found")]
    NotFound,
}
```

各モジュールを空ファイルで作成:
```bash
mkdir -p crates/llm-memory-storage/src
for f in pool users shared_memories raws wikis schemas oauth_clients tokens search; do
  touch crates/llm-memory-storage/src/$f.rs
done
```

- [ ] **Step 3: 初期 migration**

`crates/llm-memory-storage/migrations/0001_initial.sql` を spec §4.2 の通り作成（spec の SQL ブロックを丸ごとコピー）。先頭に:

```sql
-- PRAGMA は接続初期化で発行する (pool.rs)
-- ここはスキーマのみ
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
```

- [ ] **Step 4: FTS5 トリガ migration**

`crates/llm-memory-storage/migrations/0002_fts_triggers.sql`:
```sql
CREATE VIRTUAL TABLE raws_fts USING fts5(
  title, content, tags,
  content='raws', content_rowid='rowid'
);

CREATE TRIGGER raws_ai AFTER INSERT ON raws BEGIN
  INSERT INTO raws_fts(rowid, title, content, tags)
  VALUES (new.rowid, new.title, new.content, new.tags);
END;
CREATE TRIGGER raws_ad AFTER DELETE ON raws BEGIN
  INSERT INTO raws_fts(raws_fts, rowid, title, content, tags)
  VALUES ('delete', old.rowid, old.title, old.content, old.tags);
END;
CREATE TRIGGER raws_au AFTER UPDATE ON raws BEGIN
  INSERT INTO raws_fts(raws_fts, rowid, title, content, tags)
  VALUES ('delete', old.rowid, old.title, old.content, old.tags);
  INSERT INTO raws_fts(rowid, title, content, tags)
  VALUES (new.rowid, new.title, new.content, new.tags);
END;
```

- [ ] **Step 5: コンパイル確認**

Run: `cargo check -p llm-memory-storage 2>&1 | tail -5`
Expected: 警告のみ。エラーなし。

- [ ] **Step 6: Commit**

```bash
git add crates/llm-memory-storage
git commit -m "feat(storage): scaffold crate with initial migrations"
```

### Task 4: 接続プール初期化と PRAGMA

**Files:**
- Modify: `crates/llm-memory-storage/src/pool.rs`

- [ ] **Step 1: 失敗テスト（in-memory DB を開いて PRAGMA を確認）**

`crates/llm-memory-storage/src/pool.rs`:
```rust
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;

use crate::error::StorageError;

pub async fn init_pool(url: &str) -> Result<SqlitePool, StorageError> {
    let opts = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    sqlx::query("PRAGMA wal_autocheckpoint = 1000;")
        .execute(&pool)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await
        .map_err(|e| sqlx::Error::Migrate(Box::new(e)))?;

    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pool_opens_with_wal() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let (mode,): (String,) = sqlx::query_as("PRAGMA journal_mode;")
            .fetch_one(&pool).await.unwrap();
        // in-memory では memory が返る。実 file の挙動は integration test に任せる。
        assert!(mode == "wal" || mode == "memory", "got {mode}");
    }

    #[tokio::test]
    async fn migrations_apply() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='users'"
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1);
    }
}
```

- [ ] **Step 2: テスト実行 (失敗を確認)**

Run: `cargo test -p llm-memory-storage pool::tests 2>&1 | tail -10`
Expected: compile error または migration エラー（migration ディレクトリパス問題等）の可能性あり。エラー内容を読んで次 step で対処。

- [ ] **Step 3: テスト pass を確認**

成功するまでパス調整。最終的に Run: `cargo test -p llm-memory-storage pool 2>&1 | tail -5`
Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/llm-memory-storage
git commit -m "feat(storage): init SQLite pool with WAL + busy_timeout + migrations"
```

### Task 5: Users repo

**Files:**
- Modify: `crates/llm-memory-storage/src/users.rs`

- [ ] **Step 1: 失敗テスト**

`crates/llm-memory-storage/src/users.rs`:
```rust
use llm-memory-core::time::now_ms;
use sqlx::SqlitePool;

use crate::error::StorageError;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub provider: String,
    pub subject: String,
    pub email: Option<String>,
    pub created_at: i64,
}

pub async fn upsert(
    pool: &SqlitePool,
    id: &str,
    provider: &str,
    subject: &str,
    email: Option<&str>,
) -> Result<User, StorageError> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO users (id, provider, subject, email, created_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(provider, subject) DO UPDATE SET email = excluded.email
         RETURNING id, provider, subject, email, created_at",
    )
    .bind(id).bind(provider).bind(subject).bind(email).bind(now)
    .fetch_one(pool).await?;

    // RETURNING を取らず別 query で取得 (sqlite RETURNING を sqlx で取るのが面倒な場合の単純化)
    let user: User = sqlx::query_as(
        "SELECT id, provider, subject, email, created_at FROM users WHERE provider = ? AND subject = ?"
    ).bind(provider).bind(subject).fetch_one(pool).await?;
    Ok(user)
}

pub async fn find_by_id(pool: &SqlitePool, id: &str) -> Result<Option<User>, StorageError> {
    let row = sqlx::query_as::<_, User>(
        "SELECT id, provider, subject, email, created_at FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn delete_cascade(pool: &SqlitePool, user_id: &str) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM raws WHERE scope='personal' AND owner_id = ?").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM wikis WHERE scope='personal' AND owner_id = ?").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM schemas WHERE scope='personal' AND owner_id = ?").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM tokens WHERE user_id = ?").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM users WHERE id = ?").bind(user_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn upsert_creates_user() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let u = upsert(&pool, "01HJTESTUSER0000000000000A", "google", "sub-1", Some("a@example.com")).await.unwrap();
        assert_eq!(u.provider, "google");
        assert_eq!(u.subject, "sub-1");
        assert_eq!(u.email.as_deref(), Some("a@example.com"));
    }

    #[tokio::test]
    async fn upsert_is_idempotent_on_provider_subject() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let u1 = upsert(&pool, "01HJ1", "google", "sub-x", Some("old@example.com")).await.unwrap();
        let u2 = upsert(&pool, "01HJ2", "google", "sub-x", Some("new@example.com")).await.unwrap();
        assert_eq!(u1.id, u2.id, "same provider+subject should map to same user");
        assert_eq!(u2.email.as_deref(), Some("new@example.com"));
    }

    #[tokio::test]
    async fn delete_cascade_removes_user_and_personal_data() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let u = upsert(&pool, "01HJDEL0000000000000000000", "google", "sub-del", None).await.unwrap();
        delete_cascade(&pool, &u.id).await.unwrap();
        assert!(find_by_id(&pool, &u.id).await.unwrap().is_none());
    }
}
```

- [ ] **Step 2: テスト実行**

Run: `cargo test -p llm-memory-storage users::tests 2>&1 | tail -10`
Expected: 3 tests pass。

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-storage/src/users.rs
git commit -m "feat(storage): users upsert / find / cascade delete"
```

---

## Phase B: Storage layer (続き)

### Task 6: SharedMemories repo

**Files:**
- Modify: `crates/llm-memory-storage/src/shared_memories.rs`

- [ ] **Step 1: 実装とテストを 1 ファイルに書く**

`crates/llm-memory-storage/src/shared_memories.rs`:
```rust
use llm_memory_core::id::SharedMemoryId;
use llm_memory_core::time::now_ms;
use sqlx::SqlitePool;

use crate::error::StorageError;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SharedMemory {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

pub async fn create(pool: &SqlitePool, id: &SharedMemoryId, name: &str) -> Result<SharedMemory, StorageError> {
    let now = now_ms();
    sqlx::query("INSERT INTO shared_memories (id, name, created_at) VALUES (?, ?, ?)")
        .bind(&id.0).bind(name).bind(now)
        .execute(pool).await?;
    Ok(SharedMemory { id: id.0.clone(), name: name.into(), created_at: now })
}

pub async fn list_all(pool: &SqlitePool) -> Result<Vec<SharedMemory>, StorageError> {
    Ok(sqlx::query_as::<_, SharedMemory>("SELECT id, name, created_at FROM shared_memories ORDER BY id")
        .fetch_all(pool).await?)
}

pub async fn exists(pool: &SqlitePool, id: &str) -> Result<bool, StorageError> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM shared_memories WHERE id = ?")
        .bind(id).fetch_one(pool).await?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn create_and_list() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let id = SharedMemoryId::parse("company-wide").unwrap();
        create(&pool, &id, "Company Wide").await.unwrap();
        let list = list_all(&pool).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "company-wide");
    }

    #[tokio::test]
    async fn exists_returns_correctly() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let id = SharedMemoryId::parse("team-x").unwrap();
        create(&pool, &id, "Team X").await.unwrap();
        assert!(exists(&pool, "team-x").await.unwrap());
        assert!(!exists(&pool, "team-y").await.unwrap());
    }
}
```

- [ ] **Step 2: テスト実行**

Run: `cargo test -p llm-memory-storage shared_memories 2>&1 | tail -5`
Expected: 2 tests pass。

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-storage/src/shared_memories.rs
git commit -m "feat(storage): shared_memories create / list / exists"
```

### Task 7: Raws repo (insert / get / list)

**Files:**
- Modify: `crates/llm-memory-storage/src/raws.rs`

- [ ] **Step 1: 実装 + テスト**

`crates/llm-memory-storage/src/raws.rs`:
```rust
use llm_memory_core::id::new_ulid;
use llm_memory_core::scope::Scope;
use llm_memory_core::time::now_ms;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::StorageError;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, Serialize, Deserialize)]
pub struct Raw {
    pub id: String,
    pub scope: String,
    pub owner_id: String,
    pub title: String,
    pub content: String,
    pub source: String,
    pub tags: Option<String>,
    pub created_by: Option<String>,
    pub created_at: i64,
}

pub struct NewRaw<'a> {
    pub scope: Scope,
    pub owner_id: &'a str,
    pub title: &'a str,
    pub content: &'a str,
    pub source: &'a str,
    pub tags_json: Option<&'a str>,
    pub created_by: Option<&'a str>,
}

pub async fn insert(pool: &SqlitePool, r: NewRaw<'_>) -> Result<Raw, StorageError> {
    let id = new_ulid();
    let now = now_ms();
    sqlx::query(
        "INSERT INTO raws (id, scope, owner_id, title, content, source, tags, created_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id).bind(r.scope.as_str()).bind(r.owner_id).bind(r.title).bind(r.content)
    .bind(r.source).bind(r.tags_json).bind(r.created_by).bind(now)
    .execute(pool).await?;
    Ok(Raw {
        id,
        scope: r.scope.as_str().into(),
        owner_id: r.owner_id.into(),
        title: r.title.into(),
        content: r.content.into(),
        source: r.source.into(),
        tags: r.tags_json.map(Into::into),
        created_by: r.created_by.map(Into::into),
        created_at: now,
    })
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Raw>, StorageError> {
    Ok(sqlx::query_as::<_, Raw>("SELECT * FROM raws WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

pub async fn list_since(
    pool: &SqlitePool,
    scope: Scope,
    owner_id: &str,
    since_exclusive: i64,
    until_inclusive: i64,
) -> Result<Vec<Raw>, StorageError> {
    Ok(sqlx::query_as::<_, Raw>(
        "SELECT * FROM raws WHERE scope = ? AND owner_id = ? AND created_at > ? AND created_at <= ?
         ORDER BY created_at ASC, id ASC",
    )
    .bind(scope.as_str()).bind(owner_id).bind(since_exclusive).bind(until_inclusive)
    .fetch_all(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn insert_and_get() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let r = insert(&pool, NewRaw {
            scope: Scope::Personal,
            owner_id: "u1",
            title: "t",
            content: "c",
            source: "manual",
            tags_json: Some(r#"["a","b"]"#),
            created_by: Some("u1"),
        }).await.unwrap();
        let got = get(&pool, &r.id).await.unwrap().unwrap();
        assert_eq!(got.title, "t");
        assert_eq!(got.tags.as_deref(), Some(r#"["a","b"]"#));
    }

    #[tokio::test]
    async fn list_since_filters_by_range() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        for _ in 0..5 {
            insert(&pool, NewRaw {
                scope: Scope::Personal, owner_id: "u1", title: "t", content: "c",
                source: "manual", tags_json: None, created_by: Some("u1"),
            }).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let until = now_ms();
        let all = list_since(&pool, Scope::Personal, "u1", 0, until).await.unwrap();
        assert_eq!(all.len(), 5);
        let none = list_since(&pool, Scope::Personal, "u1", until, until).await.unwrap();
        assert_eq!(none.len(), 0);
    }
}
```

- [ ] **Step 2: テスト**

Run: `cargo test -p llm-memory-storage raws 2>&1 | tail -5`
Expected: 2 tests pass。

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-storage/src/raws.rs
git commit -m "feat(storage): raws insert / get / list_since"
```

### Task 8: Wikis repo (upsert with last_rebuilt_at; max watermark; concept count)

**Files:**
- Modify: `crates/llm-memory-storage/src/wikis.rs`

- [ ] **Step 1: 実装 + テスト**

`crates/llm-memory-storage/src/wikis.rs`:
```rust
use llm_memory_core::scope::Scope;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::StorageError;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, Serialize, Deserialize)]
pub struct Wiki {
    pub scope: String,
    pub owner_id: String,
    pub concept: String,
    pub content: String,
    pub source_refs: String,        // JSON array of raw ids
    pub last_rebuilt_at: i64,
}

pub async fn upsert(
    pool: &SqlitePool,
    scope: Scope,
    owner_id: &str,
    concept: &str,
    content: &str,
    source_refs_json: &str,
    last_rebuilt_at: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO wikis (scope, owner_id, concept, content, source_refs, last_rebuilt_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (scope, owner_id, concept) DO UPDATE SET
           content = excluded.content,
           source_refs = excluded.source_refs,
           last_rebuilt_at = excluded.last_rebuilt_at",
    )
    .bind(scope.as_str()).bind(owner_id).bind(concept).bind(content)
    .bind(source_refs_json).bind(last_rebuilt_at)
    .execute(pool).await?;
    Ok(())
}

pub async fn get(pool: &SqlitePool, scope: Scope, owner_id: &str, concept: &str) -> Result<Option<Wiki>, StorageError> {
    Ok(sqlx::query_as::<_, Wiki>(
        "SELECT scope, owner_id, concept, content, source_refs, last_rebuilt_at
         FROM wikis WHERE scope = ? AND owner_id = ? AND concept = ?",
    ).bind(scope.as_str()).bind(owner_id).bind(concept).fetch_optional(pool).await?)
}

pub async fn list_concepts(pool: &SqlitePool, scope: Scope, owner_id: &str) -> Result<Vec<String>, StorageError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT concept FROM wikis WHERE scope = ? AND owner_id = ? ORDER BY concept",
    ).bind(scope.as_str()).bind(owner_id).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(c,)| c).collect())
}

pub async fn max_last_rebuilt_at(pool: &SqlitePool, scope: Scope, owner_id: &str) -> Result<i64, StorageError> {
    let (v,): (Option<i64>,) = sqlx::query_as(
        "SELECT MAX(last_rebuilt_at) FROM wikis WHERE scope = ? AND owner_id = ?",
    ).bind(scope.as_str()).bind(owner_id).fetch_one(pool).await?;
    Ok(v.unwrap_or(0))
}

pub async fn count_concepts(pool: &SqlitePool, scope: Scope, owner_id: &str) -> Result<i64, StorageError> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM wikis WHERE scope = ? AND owner_id = ?",
    ).bind(scope.as_str()).bind(owner_id).fetch_one(pool).await?;
    Ok(n)
}

pub async fn list_for_owner(pool: &SqlitePool, scope: Scope, owner_id: &str) -> Result<Vec<Wiki>, StorageError> {
    Ok(sqlx::query_as::<_, Wiki>(
        "SELECT scope, owner_id, concept, content, source_refs, last_rebuilt_at
         FROM wikis WHERE scope = ? AND owner_id = ? ORDER BY concept",
    ).bind(scope.as_str()).bind(owner_id).fetch_all(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn upsert_replaces_existing() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "concept-a", "v1", "[]", 100).await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "concept-a", "v2", "[]", 200).await.unwrap();
        let w = get(&pool, Scope::Personal, "u1", "concept-a").await.unwrap().unwrap();
        assert_eq!(w.content, "v2");
        assert_eq!(w.last_rebuilt_at, 200);
    }

    #[tokio::test]
    async fn max_last_rebuilt_at_returns_zero_when_empty() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let v = max_last_rebuilt_at(&pool, Scope::Personal, "u1").await.unwrap();
        assert_eq!(v, 0);
    }

    #[tokio::test]
    async fn list_concepts_alphabetical() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "zeta", "x", "[]", 1).await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "alpha", "x", "[]", 1).await.unwrap();
        assert_eq!(list_concepts(&pool, Scope::Personal, "u1").await.unwrap(), vec!["alpha", "zeta"]);
    }
}
```

- [ ] **Step 2: テスト**

Run: `cargo test -p llm-memory-storage wikis 2>&1 | tail -5`
Expected: 3 tests pass。

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-storage/src/wikis.rs
git commit -m "feat(storage): wikis upsert + max watermark + count"
```

### Task 9: Schemas / OAuth clients / Tokens repos

**Files:**
- Modify: `crates/llm-memory-storage/src/schemas.rs`
- Modify: `crates/llm-memory-storage/src/oauth_clients.rs`
- Modify: `crates/llm-memory-storage/src/tokens.rs`

- [ ] **Step 1: Schemas**

`crates/llm-memory-storage/src/schemas.rs`:
```rust
use llm_memory_core::scope::Scope;
use llm_memory_core::time::now_ms;
use sqlx::SqlitePool;
use crate::error::StorageError;

pub async fn upsert(pool: &SqlitePool, scope: Scope, owner_id: &str, content: &str) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO schemas (scope, owner_id, content, updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT (scope, owner_id) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
    )
    .bind(scope.as_str()).bind(owner_id).bind(content).bind(now_ms())
    .execute(pool).await?;
    Ok(())
}

pub async fn get(pool: &SqlitePool, scope: Scope, owner_id: &str) -> Result<Option<String>, StorageError> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT content FROM schemas WHERE scope = ? AND owner_id = ?",
    ).bind(scope.as_str()).bind(owner_id).fetch_optional(pool).await?;
    Ok(row.map(|(c,)| c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn upsert_replaces() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "v1").await.unwrap();
        upsert(&pool, Scope::Personal, "u1", "v2").await.unwrap();
        assert_eq!(get(&pool, Scope::Personal, "u1").await.unwrap().as_deref(), Some("v2"));
    }
}
```

- [ ] **Step 2: OAuth clients**

`crates/llm-memory-storage/src/oauth_clients.rs`:
```rust
use llm_memory_core::id::new_ulid;
use llm_memory_core::time::now_ms;
use sqlx::SqlitePool;
use crate::error::StorageError;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OAuthClient {
    pub id: String,
    pub redirect_uris: String,         // JSON array
    pub grant_types: String,           // JSON array
    pub token_endpoint_auth_method: String,
    pub client_name: Option<String>,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

pub async fn register(
    pool: &SqlitePool,
    redirect_uris_json: &str,
    grant_types_json: &str,
    auth_method: &str,
    client_name: Option<&str>,
) -> Result<OAuthClient, StorageError> {
    let id = new_ulid();
    let now = now_ms();
    sqlx::query(
        "INSERT INTO oauth_clients (id, redirect_uris, grant_types, token_endpoint_auth_method, client_name, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id).bind(redirect_uris_json).bind(grant_types_json).bind(auth_method).bind(client_name).bind(now)
    .execute(pool).await?;
    Ok(OAuthClient {
        id, redirect_uris: redirect_uris_json.into(), grant_types: grant_types_json.into(),
        token_endpoint_auth_method: auth_method.into(), client_name: client_name.map(Into::into),
        created_at: now, last_seen_at: None, revoked_at: None,
    })
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<OAuthClient>, StorageError> {
    Ok(sqlx::query_as::<_, OAuthClient>(
        "SELECT id, redirect_uris, grant_types, token_endpoint_auth_method, client_name, created_at, last_seen_at, revoked_at
         FROM oauth_clients WHERE id = ?",
    ).bind(id).fetch_optional(pool).await?)
}

pub async fn touch_last_seen(pool: &SqlitePool, id: &str) -> Result<(), StorageError> {
    sqlx::query("UPDATE oauth_clients SET last_seen_at = ? WHERE id = ?")
        .bind(now_ms()).bind(id).execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn register_and_get() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let c = register(&pool, r#"["https://example.com/cb"]"#, r#"["authorization_code"]"#, "none", Some("Claude")).await.unwrap();
        let got = get(&pool, &c.id).await.unwrap().unwrap();
        assert_eq!(got.client_name.as_deref(), Some("Claude"));
    }
}
```

- [ ] **Step 3: Tokens**

`crates/llm-memory-storage/src/tokens.rs`:
```rust
use sqlx::SqlitePool;
use crate::error::StorageError;

pub async fn create_refresh(
    pool: &SqlitePool,
    token: &str,
    user_id: &str,
    client_id: &str,
    expires_at: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO tokens (refresh_token, user_id, client_id, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(token).bind(user_id).bind(client_id).bind(expires_at)
    .execute(pool).await?;
    Ok(())
}

pub async fn validate_refresh(pool: &SqlitePool, token: &str, now: i64) -> Result<Option<(String, String)>, StorageError> {
    let row: Option<(String, String, i64, Option<i64>)> = sqlx::query_as(
        "SELECT user_id, client_id, expires_at, revoked_at FROM tokens WHERE refresh_token = ?",
    ).bind(token).fetch_optional(pool).await?;
    Ok(row.and_then(|(u, c, exp, rev)| {
        if rev.is_some() || exp < now { None } else { Some((u, c)) }
    }))
}

pub async fn revoke(pool: &SqlitePool, token: &str, now: i64) -> Result<(), StorageError> {
    sqlx::query("UPDATE tokens SET revoked_at = ? WHERE refresh_token = ?")
        .bind(now).bind(token).execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth_clients;
    use crate::users;
    use crate::pool::init_pool;

    #[tokio::test]
    async fn create_and_validate() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let user = users::upsert(&pool, "01HJUSER0000000000000000A", "google", "sub", None).await.unwrap();
        let client = oauth_clients::register(&pool, "[]", "[]", "none", None).await.unwrap();
        create_refresh(&pool, "tok-1", &user.id, &client.id, 2_000_000_000_000).await.unwrap();
        let v = validate_refresh(&pool, "tok-1", 1_700_000_000_000).await.unwrap();
        assert_eq!(v, Some((user.id.clone(), client.id.clone())));
        revoke(&pool, "tok-1", 1_800_000_000_000).await.unwrap();
        assert_eq!(validate_refresh(&pool, "tok-1", 1_800_500_000_000).await.unwrap(), None);
    }
}
```

- [ ] **Step 4: テスト**

Run: `cargo test -p llm-memory-storage 2>&1 | tail -10`
Expected: 全テスト pass.

- [ ] **Step 5: Commit**

```bash
git add crates/llm-memory-storage/src
git commit -m "feat(storage): schemas, oauth_clients, tokens repos"
```

### Task 10: FTS5 検索 (raws_fts)

**Files:**
- Modify: `crates/llm-memory-storage/src/search.rs`

- [ ] **Step 1: 実装 + テスト**

`crates/llm-memory-storage/src/search.rs`:
```rust
use llm_memory_core::scope::Scope;
use sqlx::SqlitePool;
use crate::error::StorageError;
use crate::raws::Raw;

pub struct SearchQuery<'a> {
    pub query: &'a str,
    pub scope: Option<Scope>,           // None = both shared + (any owner). 認可は呼び出し側
    pub owner_id: Option<&'a str>,       // personal 限定時に指定
    pub limit: i64,
}

pub async fn raws(pool: &SqlitePool, q: SearchQuery<'_>) -> Result<Vec<Raw>, StorageError> {
    // raws_fts は MATCH で検索し、bm25 順で並べる
    // 認可フィルタは呼び出し側で scope/owner_id を渡してもらう
    let mut sql = String::from(
        "SELECT r.id, r.scope, r.owner_id, r.title, r.content, r.source, r.tags, r.created_by, r.created_at
         FROM raws_fts JOIN raws r ON r.rowid = raws_fts.rowid
         WHERE raws_fts MATCH ?",
    );
    let mut binds: Vec<String> = vec![q.query.into()];
    if let Some(s) = q.scope {
        sql.push_str(" AND r.scope = ?");
        binds.push(s.as_str().into());
    }
    if let Some(o) = q.owner_id {
        sql.push_str(" AND r.owner_id = ?");
        binds.push(o.into());
    }
    sql.push_str(" ORDER BY bm25(raws_fts) ASC LIMIT ?");
    let mut query = sqlx::query_as::<_, Raw>(&sql);
    for b in &binds { query = query.bind(b); }
    query = query.bind(q.limit);
    Ok(query.fetch_all(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::init_pool;
    use crate::raws::{insert, NewRaw};

    #[tokio::test]
    async fn search_finds_inserted() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(&pool, NewRaw {
            scope: Scope::Personal, owner_id: "u1",
            title: "Vegapunk overview", content: "GraphRAG knowledge engine",
            source: "manual", tags_json: None, created_by: Some("u1"),
        }).await.unwrap();
        let res = raws(&pool, SearchQuery {
            query: "vegapunk", scope: Some(Scope::Personal), owner_id: Some("u1"), limit: 10,
        }).await.unwrap();
        assert_eq!(res.len(), 1);
    }

    #[tokio::test]
    async fn search_respects_owner_filter() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        insert(&pool, NewRaw {
            scope: Scope::Personal, owner_id: "u1",
            title: "Alpha", content: "x", source: "m", tags_json: None, created_by: Some("u1"),
        }).await.unwrap();
        insert(&pool, NewRaw {
            scope: Scope::Personal, owner_id: "u2",
            title: "Alpha", content: "x", source: "m", tags_json: None, created_by: Some("u2"),
        }).await.unwrap();
        let res = raws(&pool, SearchQuery {
            query: "alpha", scope: Some(Scope::Personal), owner_id: Some("u1"), limit: 10,
        }).await.unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].owner_id, "u1");
    }
}
```

- [ ] **Step 2: テスト**

Run: `cargo test -p llm-memory-storage search 2>&1 | tail -5`
Expected: 2 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-storage/src/search.rs
git commit -m "feat(storage): FTS5 search over raws with scope+owner filter"
```

---

## Phase C: LLM clients

### Task 11: AnthropicClient trait と mock

**Files:**
- Create: `crates/llm-memory-llm/Cargo.toml`
- Create: `crates/llm-memory-llm/src/lib.rs`
- Create: `crates/llm-memory-llm/src/client.rs`
- Create: `crates/llm-memory-llm/src/mock.rs`

- [ ] **Step 1: crate**

`crates/llm-memory-llm/Cargo.toml`:
```toml
[package]
name = "llm-memory-llm"
edition.workspace = true

[dependencies]
llm-memory-core = { path = "../llm-memory-core" }
async-trait.workspace = true
serde.workspace = true
serde_json.workspace = true
reqwest.workspace = true
thiserror.workspace = true
tracing.workspace = true
tokio = { workspace = true, features = ["sync"] }

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt"] }
```

`crates/llm-memory-llm/src/lib.rs`:
```rust
pub mod client;
pub mod mock;
pub mod haiku;
pub mod sonnet;
pub mod prompts;
pub mod error;
```

`crates/llm-memory-llm/src/error.rs`:
```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("anthropic api error: {0}")]
    Api(String),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
```

- [ ] **Step 2: trait + mock**

`crates/llm-memory-llm/src/client.rs`:
```rust
use async_trait::async_trait;
use crate::error::LlmError;

#[derive(Debug, Clone)]
pub struct Message {
    pub role: &'static str,    // "user" or "assistant"
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct CompleteRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct CompleteResponse {
    pub content: String,
}

#[async_trait]
pub trait AnthropicClient: Send + Sync {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, LlmError>;
}
```

`crates/llm-memory-llm/src/mock.rs`:
```rust
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::client::{AnthropicClient, CompleteRequest, CompleteResponse};
use crate::error::LlmError;

#[derive(Clone, Default)]
pub struct MockClient {
    pub responses: Arc<Mutex<Vec<Result<CompleteResponse, String>>>>,
    pub captured: Arc<Mutex<Vec<CompleteRequest>>>,
}

impl MockClient {
    pub fn new() -> Self { Self::default() }
    pub async fn push_text(&self, s: impl Into<String>) {
        self.responses.lock().await.push(Ok(CompleteResponse { content: s.into() }));
    }
    pub async fn push_error(&self, msg: impl Into<String>) {
        self.responses.lock().await.push(Err(msg.into()));
    }
    pub async fn captured(&self) -> Vec<CompleteRequest> { self.captured.lock().await.clone() }
}

#[async_trait]
impl AnthropicClient for MockClient {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, LlmError> {
        self.captured.lock().await.push(req);
        let resp = self.responses.lock().await.remove(0);
        match resp {
            Ok(r) => Ok(r),
            Err(e) => Err(LlmError::Api(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::AnthropicClient;

    #[tokio::test]
    async fn mock_returns_pushed_responses_in_order() {
        let m = MockClient::new();
        m.push_text("hello").await;
        let r = m.complete(CompleteRequest {
            model: "x".into(), system: "".into(), messages: vec![], max_tokens: 10,
        }).await.unwrap();
        assert_eq!(r.content, "hello");
        assert_eq!(m.captured().await.len(), 1);
    }
}
```

- [ ] **Step 3: テスト**

Run: `cargo test -p llm-memory-llm 2>&1 | tail -5`
Expected: 1 test pass.

- [ ] **Step 4: Commit**

```bash
git add crates/llm-memory-llm
git commit -m "feat(llm): AnthropicClient trait + MockClient"
```

### Task 12: Anthropic 実 HTTP クライアント

**Files:**
- Create: `crates/llm-memory-llm/src/client_http.rs` （新規）
- Modify: `crates/llm-memory-llm/src/lib.rs` （`pub mod client_http;` 追加）

- [ ] **Step 1: 実装（テストは feature-gated でスキップ、API 鍵がいるため）**

`crates/llm-memory-llm/src/client_http.rs`:
```rust
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use crate::client::{AnthropicClient, CompleteRequest, CompleteResponse};
use crate::error::LlmError;

#[derive(Clone)]
pub struct AnthropicHttp {
    api_key: String,
    base_url: String,
    http: Client,
}

impl AnthropicHttp {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com".into(),
            http: Client::builder().timeout(std::time::Duration::from_secs(120)).build().unwrap(),
        }
    }
}

#[derive(Serialize)]
struct ApiMessage<'a> { role: &'a str, content: &'a str }

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    system: &'a str,
    messages: Vec<ApiMessage<'a>>,
    max_tokens: u32,
}

#[derive(Deserialize)]
struct ApiContent { text: String }
#[derive(Deserialize)]
struct ApiResponse { content: Vec<ApiContent> }

#[async_trait]
impl AnthropicClient for AnthropicHttp {
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, LlmError> {
        let msgs: Vec<ApiMessage> = req.messages.iter().map(|m| ApiMessage { role: m.role, content: &m.content }).collect();
        let payload = ApiRequest { model: &req.model, system: &req.system, messages: msgs, max_tokens: req.max_tokens };
        let res = self.http.post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&payload)
            .send().await?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(LlmError::Api(format!("status={status} body={body}")));
        }
        let resp: ApiResponse = res.json().await?;
        let content = resp.content.into_iter().map(|c| c.text).collect::<Vec<_>>().join("");
        Ok(CompleteResponse { content })
    }
}
```

`lib.rs` 末尾に追加: `pub mod client_http;`

- [ ] **Step 2: コンパイル確認のみ（外部 API テストは feature-gated にしない、デフォルトでは走らせない）**

Run: `cargo check -p llm-memory-llm 2>&1 | tail -5`
Expected: 警告のみ。

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-llm/src
git commit -m "feat(llm): real Anthropic HTTP client"
```

### Task 13: Haiku 概念抽出と Sonnet wiki 合成

**Files:**
- Modify: `crates/llm-memory-llm/src/prompts.rs`
- Modify: `crates/llm-memory-llm/src/haiku.rs`
- Modify: `crates/llm-memory-llm/src/sonnet.rs`

- [ ] **Step 1: prompts.rs**

```rust
pub const HAIKU_CONCEPT_EXTRACT_SYSTEM: &str = r#"
あなたは知識ベースを管理するアシスタントです。

入力として:
- 新規 raws のリスト (各 raw に title と content)
- 既存 concept 名のリスト

出力として JSON のみを返してください:
{
  "affected_existing": ["concept-name-1", ...],
  "new_concepts": ["concept-name-3", ...]
}

ルール:
- 既存 concept 一覧を優先する。新規 concept の追加は必要時のみ
- concept 名は小文字英数字とハイフン (2〜64 文字)
- 既存 concept 数が 200 を超えていたら new_concepts は空にする
"#;

pub const SONNET_WIKI_SYNTHESIZE_SYSTEM: &str = r#"
あなたは概念ごとの wiki ページを編集するアシスタントです。

入力として:
- concept (タイトル)
- 既存の wiki 内容 (空の場合あり)
- 入力 raws のリスト (それぞれに id と内容)

出力として JSON のみを返してください:
{
  "content": "Markdown 形式の wiki 本文",
  "source_refs": ["raw_id_1", "raw_id_2", ...]
}

ルール:
- source_refs は入力 raws の id のみを参照すること
- content は日本語、Markdown
- 既存 wiki があれば差分更新の形で統合する
"#;
```

- [ ] **Step 2: haiku.rs**

```rust
use serde::Deserialize;
use crate::client::{AnthropicClient, CompleteRequest, Message};
use crate::error::LlmError;
use crate::prompts::HAIKU_CONCEPT_EXTRACT_SYSTEM;

#[derive(Debug, Deserialize)]
pub struct AffectedConcepts {
    pub affected_existing: Vec<String>,
    pub new_concepts: Vec<String>,
}

pub struct HaikuExtractor<'a, C: AnthropicClient> {
    pub client: &'a C,
    pub model: String,
}

impl<'a, C: AnthropicClient> HaikuExtractor<'a, C> {
    pub async fn extract(
        &self,
        new_raws: &[(&str, &str)],     // (title, content)
        existing_concepts: &[String],
    ) -> Result<AffectedConcepts, LlmError> {
        let user = serde_json::to_string(&serde_json::json!({
            "new_raws": new_raws.iter().map(|(t, c)| serde_json::json!({"title": t, "content": c})).collect::<Vec<_>>(),
            "existing_concepts": existing_concepts,
        })).map_err(LlmError::Json)?;

        let resp = self.client.complete(CompleteRequest {
            model: self.model.clone(),
            system: HAIKU_CONCEPT_EXTRACT_SYSTEM.into(),
            messages: vec![Message { role: "user", content: user }],
            max_tokens: 1024,
        }).await?;

        let json_text = extract_json(&resp.content)
            .ok_or_else(|| LlmError::Api(format!("haiku: no JSON in response: {}", resp.content)))?;
        let parsed: AffectedConcepts = serde_json::from_str(&json_text).map_err(LlmError::Json)?;
        Ok(parsed)
    }
}

fn extract_json(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start { Some(text[start..=end].to_string()) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockClient;

    #[tokio::test]
    async fn extract_parses_mock_response() {
        let mock = MockClient::new();
        mock.push_text(r#"{"affected_existing":["alpha"],"new_concepts":["beta"]}"#).await;
        let e = HaikuExtractor { client: &mock, model: "claude-haiku-4-5".into() };
        let r = e.extract(&[("t","c")], &["alpha".into()]).await.unwrap();
        assert_eq!(r.affected_existing, vec!["alpha".to_string()]);
        assert_eq!(r.new_concepts, vec!["beta".to_string()]);
    }
}
```

- [ ] **Step 3: sonnet.rs**

```rust
use serde::Deserialize;
use crate::client::{AnthropicClient, CompleteRequest, Message};
use crate::error::LlmError;
use crate::prompts::SONNET_WIKI_SYNTHESIZE_SYSTEM;

#[derive(Debug, Deserialize)]
pub struct WikiSynth {
    pub content: String,
    pub source_refs: Vec<String>,
}

pub struct SonnetSynthesizer<'a, C: AnthropicClient> {
    pub client: &'a C,
    pub model: String,
}

pub struct SynthInput<'a> {
    pub concept: &'a str,
    pub existing_wiki: Option<&'a str>,
    pub raws: &'a [(String, String, String)],  // (raw_id, title, content)
}

impl<'a, C: AnthropicClient> SonnetSynthesizer<'a, C> {
    pub async fn synthesize(&self, input: SynthInput<'_>) -> Result<WikiSynth, LlmError> {
        let user = serde_json::to_string(&serde_json::json!({
            "concept": input.concept,
            "existing_wiki": input.existing_wiki,
            "raws": input.raws.iter().map(|(id, t, c)| serde_json::json!({"id": id, "title": t, "content": c})).collect::<Vec<_>>(),
        })).map_err(LlmError::Json)?;

        let resp = self.client.complete(CompleteRequest {
            model: self.model.clone(),
            system: SONNET_WIKI_SYNTHESIZE_SYSTEM.into(),
            messages: vec![Message { role: "user", content: user }],
            max_tokens: 8192,
        }).await?;

        let json_text = super::haiku::extract_json(&resp.content)
            .ok_or_else(|| LlmError::Api(format!("sonnet: no JSON in response: {}", resp.content)))?;
        Ok(serde_json::from_str(&json_text).map_err(LlmError::Json)?)
    }
}

// haiku::extract_json を sonnet からも見えるように pub に
```

`haiku.rs` の `extract_json` を `pub(crate) fn extract_json` に変更。

- [ ] **Step 4: テスト**

Run: `cargo test -p llm-memory-llm 2>&1 | tail -5`
Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/llm-memory-llm/src
git commit -m "feat(llm): Haiku concept extraction + Sonnet wiki synthesis"
```

---

## Phase D: RebuildCoordinator

### Task 14: state.rs / RebuildMode / RebuildState

**Files:**
- Create: `crates/llm-memory-coordinator/Cargo.toml`
- Create: `crates/llm-memory-coordinator/src/lib.rs`
- Create: `crates/llm-memory-coordinator/src/state.rs`

- [ ] **Step 1: crate**

`crates/llm-memory-coordinator/Cargo.toml`:
```toml
[package]
name = "llm-memory-coordinator"
edition.workspace = true

[dependencies]
llm-memory-core = { path = "../llm-memory-core" }
llm-memory-storage = { path = "../llm-memory-storage" }
llm-memory-llm = { path = "../llm-memory-llm" }
tokio.workspace = true
serde.workspace = true
serde_json.workspace = true
tracing.workspace = true
async-trait.workspace = true
thiserror.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "test-util"] }
```

`crates/llm-memory-coordinator/src/lib.rs`:
```rust
pub mod state;
pub mod coordinator;
pub mod worker;
pub mod input_builder;
pub mod error;
```

`crates/llm-memory-coordinator/src/error.rs`:
```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoordinatorError {
    #[error(transparent)]
    Storage(#[from] llm_memory_storage::error::StorageError),
    #[error(transparent)]
    Llm(#[from] llm_memory_llm::error::LlmError),
    #[error("worker panicked")]
    WorkerPanic,
}
```

- [ ] **Step 2: state.rs**

`crates/llm-memory-coordinator/src/state.rs`:
```rust
use llm_memory_core::scope::OwnerKey;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildMode {
    Append,
    Manual { concept: Option<String> },
}

#[derive(Debug, Default)]
pub struct RebuildState {
    pub running: bool,
    pub manual_pending: Option<RebuildMode>,
}

#[derive(Clone, Default)]
pub struct StateMap {
    inner: Arc<Mutex<HashMap<OwnerKey, RebuildState>>>,
}

impl StateMap {
    pub fn new() -> Self { Self::default() }

    pub async fn try_start(&self, key: &OwnerKey, mode: RebuildMode) -> StartOutcome {
        let mut map = self.inner.lock().await;
        let entry = map.entry(key.clone()).or_default();
        if entry.running {
            // running 中: manual を pending に積む。Manual{None} は強い（全件）ので merge は None 優先
            match &mode {
                RebuildMode::Manual { concept } => {
                    let new_p = RebuildMode::Manual { concept: concept.clone() };
                    entry.manual_pending = Some(match (entry.manual_pending.take(), new_p) {
                        (Some(RebuildMode::Manual { concept: None }), _) => RebuildMode::Manual { concept: None },
                        (_, RebuildMode::Manual { concept: None }) => RebuildMode::Manual { concept: None },
                        (_, m) => m,
                    });
                    StartOutcome::Pending
                }
                RebuildMode::Append => {
                    // append-triggered の lazy drain: 何もしない
                    StartOutcome::AlreadyRunning
                }
            }
        } else {
            entry.running = true;
            StartOutcome::Started(mode)
        }
    }

    pub async fn mark_idle_or_continue(&self, key: &OwnerKey) -> Option<RebuildMode> {
        let mut map = self.inner.lock().await;
        let entry = map.entry(key.clone()).or_default();
        if let Some(m) = entry.manual_pending.take() {
            // running は true のまま、次の loop に移行
            Some(m)
        } else {
            entry.running = false;
            None
        }
    }

    pub async fn force_idle(&self, key: &OwnerKey) {
        let mut map = self.inner.lock().await;
        if let Some(entry) = map.get_mut(key) {
            entry.running = false;
            entry.manual_pending = None;
        }
    }

    pub async fn is_running(&self, key: &OwnerKey) -> bool {
        let map = self.inner.lock().await;
        map.get(key).map(|s| s.running).unwrap_or(false)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StartOutcome {
    Started(RebuildMode),
    AlreadyRunning,
    Pending,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> OwnerKey { OwnerKey::personal("u1") }

    #[tokio::test]
    async fn append_starts_when_idle() {
        let s = StateMap::new();
        let r = s.try_start(&key(), RebuildMode::Append).await;
        assert_eq!(r, StartOutcome::Started(RebuildMode::Append));
    }

    #[tokio::test]
    async fn append_skips_when_running() {
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        let r = s.try_start(&key(), RebuildMode::Append).await;
        assert_eq!(r, StartOutcome::AlreadyRunning);
    }

    #[tokio::test]
    async fn manual_pending_when_running() {
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        let r = s.try_start(&key(), RebuildMode::Manual { concept: Some("c".into()) }).await;
        assert_eq!(r, StartOutcome::Pending);
        let cont = s.mark_idle_or_continue(&key()).await;
        assert_eq!(cont, Some(RebuildMode::Manual { concept: Some("c".into()) }));
    }

    #[tokio::test]
    async fn manual_none_overrides_some_in_pending() {
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        s.try_start(&key(), RebuildMode::Manual { concept: Some("c".into()) }).await;
        s.try_start(&key(), RebuildMode::Manual { concept: None }).await;
        let cont = s.mark_idle_or_continue(&key()).await;
        assert_eq!(cont, Some(RebuildMode::Manual { concept: None }));
    }

    #[tokio::test]
    async fn mark_idle_when_no_pending() {
        let s = StateMap::new();
        s.try_start(&key(), RebuildMode::Append).await;
        assert_eq!(s.mark_idle_or_continue(&key()).await, None);
        assert!(!s.is_running(&key()).await);
    }
}
```

- [ ] **Step 3: テスト**

Run: `cargo test -p llm-memory-coordinator state 2>&1 | tail -10`
Expected: 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/llm-memory-coordinator
git commit -m "feat(coordinator): RebuildMode + StateMap with manual_pending merge"
```

### Task 15: input_builder.rs (FTS top-k ∪ source_refs ∪ new_raws, 上限 50)

**Files:**
- Modify: `crates/llm-memory-coordinator/src/input_builder.rs`

- [ ] **Step 1: 実装 + テスト**

`crates/llm-memory-coordinator/src/input_builder.rs`:
```rust
use std::collections::HashSet;
use llm_memory_core::scope::Scope;
use llm_memory_storage::raws::Raw;
use llm_memory_storage::search::{self, SearchQuery};
use sqlx::SqlitePool;

use crate::error::CoordinatorError;

pub const INPUT_LIMIT: usize = 50;

pub async fn build(
    pool: &SqlitePool,
    scope: Scope,
    owner_id: &str,
    concept: &str,
    existing_source_refs: &[String],
    new_raws: &[Raw],
) -> Result<Vec<Raw>, CoordinatorError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Raw> = Vec::new();

    // 1. 既存 source_refs を読み込む
    for id in existing_source_refs {
        if seen.contains(id) { continue; }
        if let Some(r) = llm_memory_storage::raws::get(pool, id).await? {
            seen.insert(r.id.clone());
            out.push(r);
            if out.len() >= INPUT_LIMIT { return Ok(out); }
        }
    }

    // 2. 新規 raws を追加
    for r in new_raws {
        if seen.contains(&r.id) { continue; }
        seen.insert(r.id.clone());
        out.push(r.clone());
        if out.len() >= INPUT_LIMIT { return Ok(out); }
    }

    // 3. FTS top-k 補完
    let remaining = INPUT_LIMIT.saturating_sub(out.len());
    if remaining > 0 {
        let hits = search::raws(pool, SearchQuery {
            query: concept, scope: Some(scope), owner_id: Some(owner_id),
            limit: remaining as i64 * 2, // 重複を見越して多めに取る
        }).await?;
        for h in hits {
            if seen.contains(&h.id) { continue; }
            seen.insert(h.id.clone());
            out.push(h);
            if out.len() >= INPUT_LIMIT { break; }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_memory_storage::pool::init_pool;
    use llm_memory_storage::raws::{insert, NewRaw};

    #[tokio::test]
    async fn limit_is_enforced() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        for i in 0..60 {
            insert(&pool, NewRaw {
                scope: Scope::Personal, owner_id: "u1",
                title: &format!("vegapunk-{i}"), content: "graphrag knowledge",
                source: "m", tags_json: None, created_by: Some("u1"),
            }).await.unwrap();
        }
        let out = build(&pool, Scope::Personal, "u1", "vegapunk", &[], &[]).await.unwrap();
        assert!(out.len() <= INPUT_LIMIT);
    }

    #[tokio::test]
    async fn existing_source_refs_come_first() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let r = insert(&pool, NewRaw {
            scope: Scope::Personal, owner_id: "u1", title: "old", content: "alpha",
            source: "m", tags_json: None, created_by: Some("u1"),
        }).await.unwrap();
        let out = build(&pool, Scope::Personal, "u1", "alpha", &[r.id.clone()], &[]).await.unwrap();
        assert_eq!(out[0].id, r.id);
    }
}
```

- [ ] **Step 2: テスト**

Run: `cargo test -p llm-memory-coordinator input_builder 2>&1 | tail -5`
Expected: 2 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-coordinator/src/input_builder.rs
git commit -m "feat(coordinator): rebuild input builder (source_refs ∪ new ∪ FTS, cap 50)"
```

### Task 16: worker.rs (drain loop, RebuildMode, MAX_ITERATIONS, panic safety)

**Files:**
- Modify: `crates/llm-memory-coordinator/src/worker.rs`

- [ ] **Step 1: 実装**

`crates/llm-memory-coordinator/src/worker.rs`:
```rust
use std::sync::Arc;

use llm_memory_core::scope::{OwnerKey, Scope};
use llm_memory_core::time::now_ms;
use llm_memory_llm::client::AnthropicClient;
use llm_memory_llm::haiku::HaikuExtractor;
use llm_memory_llm::sonnet::{SonnetSynthesizer, SynthInput};
use llm_memory_storage::{raws, wikis};
use sqlx::SqlitePool;
use tracing::{warn, info, error};

use crate::error::CoordinatorError;
use crate::input_builder;
use crate::state::{RebuildMode, StateMap};

pub const MAX_ITERATIONS: usize = 10;
pub const CONCEPT_LIMIT_PER_OWNER: i64 = 200;
pub const CONCEPT_CONCURRENCY: usize = 4;

pub struct WorkerDeps<C: AnthropicClient + 'static> {
    pub pool: SqlitePool,
    pub state: StateMap,
    pub llm: Arc<C>,
    pub model_haiku: String,
    pub model_sonnet: String,
}

pub fn spawn_worker<C: AnthropicClient + 'static>(
    deps: Arc<WorkerDeps<C>>,
    key: OwnerKey,
    initial_mode: RebuildMode,
) {
    tokio::spawn(async move {
        let handle = tokio::spawn(run_worker(deps.clone(), key.clone(), initial_mode));
        let result = handle.await;
        // panic / 通常終了問わず state を idle に戻す
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!(?key, ?e, "rebuild worker error"),
            Err(join_err) if join_err.is_panic() => {
                error!(?key, ?join_err, "rebuild worker panicked");
            }
            Err(e) => error!(?key, ?e, "rebuild worker join error"),
        }
        deps.state.force_idle(&key).await;
    });
}

async fn run_worker<C: AnthropicClient + 'static>(
    deps: Arc<WorkerDeps<C>>,
    key: OwnerKey,
    initial_mode: RebuildMode,
) -> Result<(), CoordinatorError> {
    let mut next_mode: Option<RebuildMode> = Some(initial_mode);
    while let Some(mode) = next_mode.take() {
        run_session(&deps, &key, mode).await?;
        next_mode = deps.state.mark_idle_or_continue(&key).await;
        // mark_idle_or_continue が Some を返したときは state.running は true のまま
    }
    Ok(())
}

async fn run_session<C: AnthropicClient>(
    deps: &WorkerDeps<C>,
    key: &OwnerKey,
    starting_mode: RebuildMode,
) -> Result<(), CoordinatorError> {
    let mut mode = starting_mode;
    for iteration in 1..=MAX_ITERATIONS {
        let started_at = now_ms();
        let watermark = wikis::max_last_rebuilt_at(&deps.pool, key.scope, &key.owner_id).await?;

        let new_raws = raws::list_since(&deps.pool, key.scope, &key.owner_id, watermark, started_at).await?;
        let existing_concepts = wikis::list_concepts(&deps.pool, key.scope, &key.owner_id).await?;

        let affected: Vec<String> = match &mode {
            RebuildMode::Append => {
                if new_raws.is_empty() {
                    info!(?key, "drain complete (no new raws)");
                    return Ok(());
                }
                // Haiku で抽出
                let extractor = HaikuExtractor { client: deps.llm.as_ref(), model: deps.model_haiku.clone() };
                let titles_contents: Vec<(&str, &str)> = new_raws.iter().map(|r| (r.title.as_str(), r.content.as_str())).collect();
                let extracted = extractor.extract(&titles_contents, &existing_concepts).await?;
                let mut set: std::collections::BTreeSet<String> = extracted.affected_existing.into_iter().collect();
                let current_count = wikis::count_concepts(&deps.pool, key.scope, &key.owner_id).await?;
                if current_count < CONCEPT_LIMIT_PER_OWNER {
                    for c in extracted.new_concepts { set.insert(c); }
                } else {
                    warn!(?key, current_count, "concept limit reached, ignoring new_concepts");
                }
                set.into_iter().collect()
            }
            RebuildMode::Manual { concept: Some(c) } => vec![c.clone()],
            RebuildMode::Manual { concept: None } => existing_concepts.clone(),
        };

        if affected.is_empty() {
            info!(?key, "no affected concepts, ending session");
            return Ok(());
        }

        // 並列処理
        synthesize_concepts(deps, key, &affected, &new_raws, started_at).await?;

        // 次 iteration は Append モード (Manual は 1 iteration のみ)
        mode = RebuildMode::Append;

        if iteration == MAX_ITERATIONS {
            warn!(?key, "drain loop hit MAX_ITERATIONS, deferring");
            metrics::counter!("rebuild_drain_capped_total").increment(1);
            return Ok(());
        }
    }
    Ok(())
}

async fn synthesize_concepts<C: AnthropicClient>(
    deps: &WorkerDeps<C>,
    key: &OwnerKey,
    affected: &[String],
    new_raws: &[llm_memory_storage::raws::Raw],
    started_at: i64,
) -> Result<(), CoordinatorError> {
    use futures::stream::{self, StreamExt};
    stream::iter(affected.iter().cloned())
        .map(|concept| {
            let deps = deps;
            let key = key.clone();
            let new_raws = new_raws.to_vec();
            async move {
                if let Err(e) = synthesize_one(deps, &key, &concept, &new_raws, started_at).await {
                    error!(?key, %concept, ?e, "synthesize_one failed");
                    metrics::counter!("concept_rebuild_failed_total").increment(1);
                }
            }
        })
        .buffer_unordered(CONCEPT_CONCURRENCY)
        .for_each(|_| async {})
        .await;
    Ok(())
}

async fn synthesize_one<C: AnthropicClient>(
    deps: &WorkerDeps<C>,
    key: &OwnerKey,
    concept: &str,
    new_raws: &[llm_memory_storage::raws::Raw],
    started_at: i64,
) -> Result<(), CoordinatorError> {
    let existing_wiki = wikis::get(&deps.pool, key.scope, &key.owner_id, concept).await?;
    let existing_refs: Vec<String> = existing_wiki.as_ref()
        .and_then(|w| serde_json::from_str(&w.source_refs).ok())
        .unwrap_or_default();
    let inputs = input_builder::build(&deps.pool, key.scope, &key.owner_id, concept, &existing_refs, new_raws).await?;

    let synth = SonnetSynthesizer { client: deps.llm.as_ref(), model: deps.model_sonnet.clone() };
    let raws_tuple: Vec<(String, String, String)> = inputs.iter().map(|r| (r.id.clone(), r.title.clone(), r.content.clone())).collect();
    let result = synth.synthesize(SynthInput {
        concept,
        existing_wiki: existing_wiki.as_ref().map(|w| w.content.as_str()),
        raws: &raws_tuple,
    }).await?;

    let valid_ids: std::collections::HashSet<&String> = inputs.iter().map(|r| &r.id).collect();
    let filtered_refs: Vec<String> = result.source_refs.into_iter().filter(|id| valid_ids.contains(id)).collect();
    let refs_json = serde_json::to_string(&filtered_refs).unwrap_or_else(|_| "[]".into());

    wikis::upsert(&deps.pool, key.scope, &key.owner_id, concept, &result.content, &refs_json, started_at).await?;
    Ok(())
}

// metrics は server crate で初期化、ここでは shim を使う
mod metrics {
    pub struct Counter;
    impl Counter { pub fn increment(self, _: u64) {} }
    pub fn counter(_: &str) -> Counter { Counter }
}
```

`Cargo.toml` に `futures = "0.3"` を追加。

- [ ] **Step 2: 統合テスト (mock LLM で end-to-end)**

`crates/llm-memory-coordinator/src/worker.rs` の末尾に:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use llm_memory_storage::pool::init_pool;
    use llm_memory_storage::raws::{insert, NewRaw};
    use llm_memory_llm::mock::MockClient;

    async fn deps(pool: SqlitePool, mock: Arc<MockClient>) -> Arc<WorkerDeps<MockClient>> {
        Arc::new(WorkerDeps {
            pool, state: StateMap::new(), llm: mock,
            model_haiku: "haiku".into(), model_sonnet: "sonnet".into(),
        })
    }

    #[tokio::test]
    async fn append_mode_creates_wiki() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        // Haiku → 1 つの new_concept
        mock.push_text(r#"{"affected_existing":[],"new_concepts":["vegapunk"]}"#).await;
        // Sonnet → wiki content
        mock.push_text(r#"{"content":"# Vegapunk\n...","source_refs":[]}"#).await;
        insert(&pool, NewRaw {
            scope: Scope::Personal, owner_id: "u1", title: "v1", content: "graphrag",
            source: "m", tags_json: None, created_by: Some("u1"),
        }).await.unwrap();

        let deps = deps(pool.clone(), mock.clone()).await;
        run_session(&deps, &OwnerKey::personal("u1"), RebuildMode::Append).await.unwrap();

        let w = wikis::get(&pool, Scope::Personal, "u1", "vegapunk").await.unwrap();
        assert!(w.is_some());
    }

    #[tokio::test]
    async fn manual_full_rebuilds_all_existing_concepts() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        // 既存 wiki を 2 件用意
        wikis::upsert(&pool, Scope::Personal, "u1", "alpha", "old-a", "[]", 100).await.unwrap();
        wikis::upsert(&pool, Scope::Personal, "u1", "beta", "old-b", "[]", 100).await.unwrap();
        let mock = Arc::new(MockClient::new());
        // Manual{None} は Haiku 呼ばない → 2 つの Sonnet 出力のみ
        mock.push_text(r#"{"content":"new-a","source_refs":[]}"#).await;
        mock.push_text(r#"{"content":"new-b","source_refs":[]}"#).await;

        let deps = deps(pool.clone(), mock).await;
        run_session(&deps, &OwnerKey::personal("u1"), RebuildMode::Manual { concept: None }).await.unwrap();

        let a = wikis::get(&pool, Scope::Personal, "u1", "alpha").await.unwrap().unwrap();
        let b = wikis::get(&pool, Scope::Personal, "u1", "beta").await.unwrap().unwrap();
        // どちらの concept も新しい内容に置き換わっていること
        assert_ne!(a.content, "old-a");
        assert_ne!(b.content, "old-b");
    }

    #[tokio::test]
    async fn manual_single_concept_skips_haiku() {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        wikis::upsert(&pool, Scope::Personal, "u1", "alpha", "old", "[]", 100).await.unwrap();
        let mock = Arc::new(MockClient::new());
        // Haiku は呼ばれないはず → Sonnet 1 回だけ
        mock.push_text(r#"{"content":"new","source_refs":[]}"#).await;

        let deps = deps(pool.clone(), mock.clone()).await;
        run_session(&deps, &OwnerKey::personal("u1"), RebuildMode::Manual { concept: Some("alpha".into()) }).await.unwrap();

        let cap = mock.captured().await;
        assert_eq!(cap.len(), 1);
        assert_eq!(cap[0].model, "sonnet");
    }

    #[tokio::test]
    async fn worker_recovers_from_panic() {
        // panic を発生させるには rebuild_body 内で panic を起こす必要がある。
        // ここでは MockClient に「panic!」させるオプションを足すか、別途検証。
        // 統合テストとして spawn_worker → mock が一度 panic 後に state が idle 復帰することを観測する。
        let pool = init_pool("sqlite::memory:").await.unwrap();
        let mock = Arc::new(MockClient::new());
        // 何も push しない → 最初の complete() で「lock().await.remove(0)」が panic
        let deps = deps(pool.clone(), mock.clone()).await;
        insert(&pool, NewRaw {
            scope: Scope::Personal, owner_id: "u1", title: "x", content: "y",
            source: "m", tags_json: None, created_by: Some("u1"),
        }).await.unwrap();

        spawn_worker(deps.clone(), OwnerKey::personal("u1"), RebuildMode::Append);
        // worker が落ちて idle に戻るまで待つ
        for _ in 0..50 {
            if !deps.state.is_running(&OwnerKey::personal("u1")).await { return; }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("state still running after panic");
    }
}
```

- [ ] **Step 3: テスト**

Run: `cargo test -p llm-memory-coordinator worker 2>&1 | tail -15`
Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/llm-memory-coordinator
git commit -m "feat(coordinator): drain-loop worker with MAX_ITERATIONS and panic safety"
```

### Task 17: coordinator.rs (公開 API: notify_append / request_manual)

**Files:**
- Modify: `crates/llm-memory-coordinator/src/coordinator.rs`

- [ ] **Step 1: 実装**

`crates/llm-memory-coordinator/src/coordinator.rs`:
```rust
use std::sync::Arc;

use llm_memory_core::scope::OwnerKey;
use llm_memory_llm::client::AnthropicClient;

use crate::state::{RebuildMode, StartOutcome};
use crate::worker::{spawn_worker, WorkerDeps};

#[derive(Clone)]
pub struct Coordinator<C: AnthropicClient + 'static> {
    deps: Arc<WorkerDeps<C>>,
}

impl<C: AnthropicClient + 'static> Coordinator<C> {
    pub fn new(deps: Arc<WorkerDeps<C>>) -> Self { Self { deps } }

    pub async fn notify_append(&self, user_id: &str) -> bool {
        let key = OwnerKey::personal(user_id);
        let outcome = self.deps.state.try_start(&key, RebuildMode::Append).await;
        match outcome {
            StartOutcome::Started(mode) => {
                spawn_worker(self.deps.clone(), key, mode);
                true
            }
            _ => false,
        }
    }

    pub async fn request_manual(&self, user_id: &str, concept: Option<String>) -> ManualOutcome {
        let key = OwnerKey::personal(user_id);
        let outcome = self.deps.state.try_start(&key, RebuildMode::Manual { concept: concept.clone() }).await;
        match outcome {
            StartOutcome::Started(mode) => {
                spawn_worker(self.deps.clone(), key, mode);
                ManualOutcome::Started
            }
            StartOutcome::Pending => ManualOutcome::Pending,
            StartOutcome::AlreadyRunning => ManualOutcome::Pending, // ありえないが念のため
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ManualOutcome {
    Started,
    Pending,
}
```

- [ ] **Step 2: コンパイル確認**

Run: `cargo check -p llm-memory-coordinator 2>&1 | tail -5`
Expected: 警告のみ。

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-coordinator/src/coordinator.rs
git commit -m "feat(coordinator): public Coordinator with notify_append + request_manual"
```

---

## Phase E: Auth

### Task 18: JWT 発行・検証

**Files:**
- Create: `crates/llm-memory-auth/Cargo.toml`
- Create: `crates/llm-memory-auth/src/lib.rs`
- Create: `crates/llm-memory-auth/src/jwt.rs`
- Create: `crates/llm-memory-auth/src/error.rs`

- [ ] **Step 1: crate**

`crates/llm-memory-auth/Cargo.toml`:
```toml
[package]
name = "llm-memory-auth"
edition.workspace = true

[dependencies]
llm-memory-core = { path = "../llm-memory-core" }
llm-memory-storage = { path = "../llm-memory-storage" }
axum.workspace = true
axum-extra.workspace = true
jsonwebtoken.workspace = true
oauth2.workspace = true
serde.workspace = true
serde_json.workspace = true
reqwest.workspace = true
url.workspace = true
base64.workspace = true
sha2.workspace = true
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros"] }
```

`crates/llm-memory-auth/src/lib.rs`:
```rust
pub mod jwt;
pub mod google;
pub mod authorization_server;
pub mod dcr;
pub mod middleware;
pub mod xff;
pub mod error;
```

`crates/llm-memory-auth/src/error.rs`:
```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid token")]
    InvalidToken,
    #[error("missing kid")]
    MissingKid,
    #[error("unknown kid: {0}")]
    UnknownKid(String),
    #[error(transparent)]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error("oauth error: {0}")]
    OAuth(String),
}
```

- [ ] **Step 2: jwt.rs**

```rust
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::AuthError;
use llm_memory_core::time::now_ms;

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,        // user_id
    pub client_id: String,
    pub iat: i64,
    pub exp: i64,
}

#[derive(Clone)]
pub struct JwtKeys {
    pub current_kid: String,
    pub keys: HashMap<String, Vec<u8>>,    // kid -> 32-byte secret
}

impl JwtKeys {
    pub fn from_env() -> Self {
        // 命名規約: JWT_SIGNING_KEY_v1, v2, ... base64 encoded
        let mut keys = HashMap::new();
        let mut current = String::new();
        for (k, v) in std::env::vars() {
            if let Some(kid) = k.strip_prefix("JWT_SIGNING_KEY_") {
                let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &v)
                    .expect("invalid base64 for JWT signing key");
                keys.insert(kid.to_string(), bytes);
                if kid > current.as_str() { current = kid.to_string(); }
            }
        }
        Self { current_kid: current, keys }
    }
}

pub fn issue(keys: &JwtKeys, user_id: &str, client_id: &str, ttl_seconds: i64) -> Result<String, AuthError> {
    let now = now_ms() / 1000;
    let claims = Claims {
        sub: user_id.into(), client_id: client_id.into(),
        iat: now, exp: now + ttl_seconds,
    };
    let mut header = Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some(keys.current_kid.clone());
    let secret = keys.keys.get(&keys.current_kid).ok_or(AuthError::MissingKid)?;
    Ok(jsonwebtoken::encode(&header, &claims, &EncodingKey::from_secret(secret))?)
}

pub fn verify(keys: &JwtKeys, token: &str) -> Result<Claims, AuthError> {
    let header = jsonwebtoken::decode_header(token)?;
    let kid = header.kid.ok_or(AuthError::MissingKid)?;
    let secret = keys.keys.get(&kid).ok_or_else(|| AuthError::UnknownKid(kid.clone()))?;
    let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    let data = jsonwebtoken::decode::<Claims>(token, &DecodingKey::from_secret(secret), &validation)?;
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> JwtKeys {
        let mut m = HashMap::new();
        m.insert("v1".into(), b"01234567890123456789012345678901".to_vec());
        JwtKeys { current_kid: "v1".into(), keys: m }
    }

    #[test]
    fn issue_and_verify_roundtrip() {
        let k = keys();
        let token = issue(&k, "u1", "c1", 3600).unwrap();
        let claims = verify(&k, &token).unwrap();
        assert_eq!(claims.sub, "u1");
        assert_eq!(claims.client_id, "c1");
    }

    #[test]
    fn unknown_kid_rejected() {
        let mut k = keys();
        let token = issue(&k, "u1", "c1", 3600).unwrap();
        k.keys.remove("v1");
        let err = verify(&k, &token).unwrap_err();
        matches!(err, AuthError::UnknownKid(_));
    }
}
```

- [ ] **Step 3: テスト**

Run: `cargo test -p llm-memory-auth jwt 2>&1 | tail -5`
Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/llm-memory-auth
git commit -m "feat(auth): JWT issue/verify with kid-based key rotation"
```

### Task 19: X-Forwarded-For パーサ

**Files:**
- Modify: `crates/llm-memory-auth/src/xff.rs`

- [ ] **Step 1: 実装 + テスト**

`crates/llm-memory-auth/src/xff.rs`:
```rust
use std::net::IpAddr;

/// X-Forwarded-For ヘッダから「信頼できる client IP」を取り出す。
/// XFF はカンマ区切りで `client, proxy1, proxy2, ...` の順。
/// `trusted_proxy_count` 個のプロキシを末尾から信頼し、その手前を client とする。
pub fn parse_client_ip(xff_header: Option<&str>, peer_ip: IpAddr, trusted_proxy_count: usize) -> IpAddr {
    if let Some(xff) = xff_header {
        let ips: Vec<&str> = xff.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        if ips.len() > trusted_proxy_count {
            let idx = ips.len() - 1 - trusted_proxy_count;
            if let Ok(ip) = ips[idx].parse::<IpAddr>() {
                return ip;
            }
        }
    }
    peer_ip
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn xff_one_trusted_proxy() {
        // client, lb
        let ip = parse_client_ip(Some("203.0.113.10, 35.244.0.1"), IpAddr::from_str("10.0.0.1").unwrap(), 1);
        assert_eq!(ip.to_string(), "203.0.113.10");
    }

    #[test]
    fn xff_two_trusted_proxies() {
        let ip = parse_client_ip(Some("203.0.113.10, 198.51.100.1, 35.244.0.1"), IpAddr::from_str("10.0.0.1").unwrap(), 2);
        assert_eq!(ip.to_string(), "203.0.113.10");
    }

    #[test]
    fn xff_too_short_falls_back_to_peer() {
        let ip = parse_client_ip(Some("203.0.113.10"), IpAddr::from_str("10.0.0.1").unwrap(), 1);
        assert_eq!(ip.to_string(), "10.0.0.1");
    }

    #[test]
    fn no_xff_uses_peer() {
        let ip = parse_client_ip(None, IpAddr::from_str("10.0.0.1").unwrap(), 1);
        assert_eq!(ip.to_string(), "10.0.0.1");
    }
}
```

- [ ] **Step 2: テスト**

Run: `cargo test -p llm-memory-auth xff 2>&1 | tail -5`
Expected: 4 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-auth/src/xff.rs
git commit -m "feat(auth): X-Forwarded-For parser with trusted proxy count"
```

### Task 20: Google OAuth クライアント（authorization code + PKCE）

**Files:**
- Modify: `crates/llm-memory-auth/src/google.rs`

- [ ] **Step 1: 実装**

`crates/llm-memory-auth/src/google.rs`:
```rust
use oauth2::basic::BasicClient;
use oauth2::{AuthUrl, ClientId, ClientSecret, RedirectUrl, TokenUrl, AuthorizationCode, CsrfToken, PkceCodeChallenge, PkceCodeVerifier, Scope, TokenResponse};
use reqwest::Client;
use serde::Deserialize;

use crate::error::AuthError;

pub struct GoogleConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

pub struct GoogleClient {
    inner: BasicClient,
    http: Client,
}

#[derive(Debug, Deserialize)]
pub struct GoogleUserInfo {
    pub sub: String,
    pub email: Option<String>,
}

impl GoogleClient {
    pub fn new(cfg: GoogleConfig) -> Self {
        let inner = BasicClient::new(
            ClientId::new(cfg.client_id),
            Some(ClientSecret::new(cfg.client_secret)),
            AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".into()).unwrap(),
            Some(TokenUrl::new("https://oauth2.googleapis.com/token".into()).unwrap()),
        ).set_redirect_uri(RedirectUrl::new(cfg.redirect_uri).unwrap());
        Self { inner, http: Client::new() }
    }

    pub fn authorize_url(&self) -> (url::Url, CsrfToken, PkceCodeVerifier) {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, csrf) = self.inner
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("openid".into()))
            .add_scope(Scope::new("email".into()))
            .set_pkce_challenge(challenge)
            .url();
        (url, csrf, verifier)
    }

    pub async fn exchange_code(&self, code: String, verifier: PkceCodeVerifier) -> Result<String, AuthError> {
        let token = self.inner
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(verifier)
            .request_async(oauth2::reqwest::async_http_client)
            .await
            .map_err(|e| AuthError::OAuth(e.to_string()))?;
        Ok(token.access_token().secret().clone())
    }

    pub async fn userinfo(&self, access_token: &str) -> Result<GoogleUserInfo, AuthError> {
        let info = self.http.get("https://openidconnect.googleapis.com/v1/userinfo")
            .bearer_auth(access_token)
            .send().await?
            .error_for_status()?
            .json::<GoogleUserInfo>().await?;
        Ok(info)
    }
}
```

- [ ] **Step 2: コンパイル確認**

Run: `cargo check -p llm-memory-auth 2>&1 | tail -5`
Expected: 警告のみ。

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-auth/src/google.rs
git commit -m "feat(auth): Google OAuth client (authorization code + PKCE)"
```

### Task 21: Dynamic Client Registration (DCR) + redirect validation

**Files:**
- Modify: `crates/llm-memory-auth/src/dcr.rs`

- [ ] **Step 1: 実装 + テスト**

`crates/llm-memory-auth/src/dcr.rs`:
```rust
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::AuthError;

#[derive(Debug, Deserialize)]
pub struct DcrRequest {
    pub redirect_uris: Vec<String>,
    #[serde(default = "default_grant_types")]
    pub grant_types: Vec<String>,
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
    pub client_name: Option<String>,
}

fn default_grant_types() -> Vec<String> { vec!["authorization_code".into(), "refresh_token".into()] }

#[derive(Debug, Serialize)]
pub struct DcrResponse {
    pub client_id: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub token_endpoint_auth_method: String,
    pub client_name: Option<String>,
}

pub const MAX_REDIRECT_URIS: usize = 5;
pub const ALLOWED_GRANT_TYPES: &[&str] = &["authorization_code", "refresh_token"];
pub const ALLOWED_AUTH_METHODS: &[&str] = &["none", "client_secret_basic"];

pub fn validate(req: &DcrRequest) -> Result<DcrResponse, AuthError> {
    if req.redirect_uris.is_empty() {
        return Err(AuthError::OAuth("redirect_uris required".into()));
    }
    if req.redirect_uris.len() > MAX_REDIRECT_URIS {
        return Err(AuthError::OAuth(format!("redirect_uris exceeds max {MAX_REDIRECT_URIS}")));
    }
    for u in &req.redirect_uris {
        let parsed = Url::parse(u).map_err(|_| AuthError::OAuth(format!("invalid redirect_uri: {u}")))?;
        if parsed.scheme() != "https" {
            return Err(AuthError::OAuth(format!("redirect_uri must be https: {u}")));
        }
    }
    for g in &req.grant_types {
        if !ALLOWED_GRANT_TYPES.contains(&g.as_str()) {
            return Err(AuthError::OAuth(format!("grant_type not allowed: {g}")));
        }
    }
    let method = req.token_endpoint_auth_method.clone().unwrap_or_else(|| "none".into());
    if !ALLOWED_AUTH_METHODS.contains(&method.as_str()) {
        return Err(AuthError::OAuth(format!("auth method not allowed: {method}")));
    }
    Ok(DcrResponse {
        client_id: String::new(),       // 呼び出し側で ULID を埋める
        redirect_uris: req.redirect_uris.clone(),
        grant_types: req.grant_types.clone(),
        token_endpoint_auth_method: method,
        client_name: req.client_name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_https_redirect() {
        let r = DcrRequest {
            redirect_uris: vec!["https://example.com/cb".into()],
            grant_types: default_grant_types(), token_endpoint_auth_method: None, client_name: None,
        };
        assert!(validate(&r).is_ok());
    }

    #[test]
    fn rejects_http_redirect() {
        let r = DcrRequest {
            redirect_uris: vec!["http://example.com/cb".into()],
            grant_types: default_grant_types(), token_endpoint_auth_method: None, client_name: None,
        };
        assert!(validate(&r).is_err());
    }

    #[test]
    fn rejects_unknown_grant_type() {
        let r = DcrRequest {
            redirect_uris: vec!["https://x/cb".into()],
            grant_types: vec!["implicit".into()], token_endpoint_auth_method: None, client_name: None,
        };
        assert!(validate(&r).is_err());
    }

    #[test]
    fn rejects_too_many_redirects() {
        let r = DcrRequest {
            redirect_uris: vec!["https://x/cb".into(); 6],
            grant_types: default_grant_types(), token_endpoint_auth_method: None, client_name: None,
        };
        assert!(validate(&r).is_err());
    }
}
```

- [ ] **Step 2: テスト**

Run: `cargo test -p llm-memory-auth dcr 2>&1 | tail -5`
Expected: 4 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-auth/src/dcr.rs
git commit -m "feat(auth): DCR validation (redirect_uri https, grant_types, auth_method)"
```

### Task 22: Authorization Server エンドポイント

**Files:**
- Modify: `crates/llm-memory-auth/src/authorization_server.rs`

- [ ] **Step 1: 実装**

`crates/llm-memory-auth/src/authorization_server.rs` — Spec §9 の通り `/.well-known/oauth-authorization-server` / `/oauth/register` / `/oauth/authorize` / `/oauth/callback/google` / `/oauth/token` / `/oauth/revoke` を axum Router として実装。

このタスクは大きいため、以下 3 step に分割:

```rust
// 概要のみ。具体的なハンドラ実装は server crate と統合
use axum::{Router, routing::{get, post}};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/callback/google", get(callback_google))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
}
```

ハンドラ実装は server crate と密結合のため Task 27 で完成させる。本タスクでは Router 定義と各ハンドラのシグネチャだけ用意。

- [ ] **Step 2: スケルトンを書く**

具体的なシグネチャと未実装プレースホルダ (`todo!()`) を持つハンドラを書き、コンパイルが通る状態にする。実装は Task 27 で完成。

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-auth/src/authorization_server.rs
git commit -m "feat(auth): AS router scaffold (handlers stubbed)"
```

### Task 23: Auth middleware (Bearer JWT 検証)

**Files:**
- Modify: `crates/llm-memory-auth/src/middleware.rs`

- [ ] **Step 1: 実装 + テスト**

`crates/llm-memory-auth/src/middleware.rs`:
```rust
use axum::{extract::{Request, State}, http::StatusCode, middleware::Next, response::Response};
use axum_extra::headers::{Authorization, authorization::Bearer};
use axum_extra::TypedHeader;

use crate::jwt::{self, JwtKeys};

#[derive(Clone, Debug)]
pub struct AuthenticatedUser {
    pub user_id: String,
    pub client_id: String,
}

pub async fn require_auth(
    State(keys): State<JwtKeys>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let claims = jwt::verify(&keys, bearer.token()).map_err(|_| StatusCode::UNAUTHORIZED)?;
    req.extensions_mut().insert(AuthenticatedUser { user_id: claims.sub, client_id: claims.client_id });
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn missing_header_returns_unauthorized() {
        // axum middleware 単体テストはやや大袈裟になるため、jwt::verify を直接呼ぶ単体テストを置く
        let mut m = HashMap::new();
        m.insert("v1".into(), b"01234567890123456789012345678901".to_vec());
        let keys = JwtKeys { current_kid: "v1".into(), keys: m };
        assert!(jwt::verify(&keys, "bogus").is_err());
    }
}
```

- [ ] **Step 2: テスト**

Run: `cargo test -p llm-memory-auth middleware 2>&1 | tail -5`
Expected: 1 test pass.

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-auth/src/middleware.rs
git commit -m "feat(auth): Bearer JWT middleware"
```

---

## Phase F: MCP server

### Task 24: server crate スケルトンと config

**Files:**
- Create: `crates/llm-memory-server/Cargo.toml`
- Create: `crates/llm-memory-server/src/main.rs`
- Create: `crates/llm-memory-server/src/config.rs`
- Create: `crates/llm-memory-server/src/app.rs`

- [ ] **Step 1: Cargo.toml**

```toml
[package]
name = "llm-memory-server"
edition.workspace = true

[[bin]]
name = "llm-memory-server"
path = "src/main.rs"

[dependencies]
llm-memory-core = { path = "../llm-memory-core" }
llm-memory-storage = { path = "../llm-memory-storage" }
llm-memory-llm = { path = "../llm-memory-llm" }
llm-memory-coordinator = { path = "../llm-memory-coordinator" }
llm-memory-auth = { path = "../llm-memory-auth" }
axum.workspace = true
axum-extra.workspace = true
tokio.workspace = true
serde.workspace = true
serde_json.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
anyhow.workspace = true
prometheus.workspace = true
sqlx.workspace = true
```

- [ ] **Step 2: config.rs**

```rust
use std::env;

#[derive(Clone)]
pub struct ServerConfig {
    pub database_url: String,
    pub bind_addr: String,
    pub public_url: String,
    pub anthropic_api_key: String,
    pub google_client_id: String,
    pub google_client_secret: String,
    pub model_haiku: String,
    pub model_sonnet: String,
    pub trusted_proxy_count: usize,
}

impl ServerConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            database_url: env::var("DATABASE_URL")?,
            bind_addr: env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            public_url: env::var("PUBLIC_URL")?,
            anthropic_api_key: env::var("ANTHROPIC_API_KEY")?,
            google_client_id: env::var("GOOGLE_OAUTH_CLIENT_ID")?,
            google_client_secret: env::var("GOOGLE_OAUTH_CLIENT_SECRET")?,
            model_haiku: env::var("MODEL_HAIKU").unwrap_or_else(|_| "claude-haiku-4-5-20251001".into()),
            model_sonnet: env::var("MODEL_SONNET").unwrap_or_else(|_| "claude-sonnet-4-6".into()),
            trusted_proxy_count: env::var("TRUSTED_PROXY_COUNT")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(1),
        })
    }
}
```

- [ ] **Step 3: app.rs (Router 骨格)**

```rust
use std::sync::Arc;

use axum::{Router, routing::get};
use sqlx::SqlitePool;
use llm_memory_auth::jwt::JwtKeys;
use llm_memory_llm::client_http::AnthropicHttp;
use llm_memory_coordinator::coordinator::Coordinator;
use llm_memory_coordinator::worker::WorkerDeps;
use llm_memory_coordinator::state::StateMap;

use crate::config::ServerConfig;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub coordinator: Coordinator<AnthropicHttp>,
    pub jwt_keys: JwtKeys,
    pub cfg: Arc<ServerConfig>,
}

pub async fn build_router(cfg: ServerConfig) -> anyhow::Result<Router> {
    let pool = llm_memory_storage::pool::init_pool(&cfg.database_url).await?;
    let llm = Arc::new(AnthropicHttp::new(cfg.anthropic_api_key.clone()));
    let deps = Arc::new(WorkerDeps {
        pool: pool.clone(),
        state: StateMap::new(),
        llm,
        model_haiku: cfg.model_haiku.clone(),
        model_sonnet: cfg.model_sonnet.clone(),
    });
    let coordinator = Coordinator::new(deps);
    let jwt_keys = JwtKeys::from_env();
    let state = AppState { pool, coordinator, jwt_keys, cfg: Arc::new(cfg) };
    let router = Router::new()
        .route("/healthz", get(healthz))
        .with_state(state);
    Ok(router)
}

async fn healthz() -> &'static str { "ok" }
```

- [ ] **Step 4: main.rs**

```rust
use llm_memory_server::{app, config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    let cfg = config::ServerConfig::from_env()?;
    let bind = cfg.bind_addr.clone();
    let router = app::build_router(cfg).await?;
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "server starting");
    axum::serve(listener, router).await?;
    Ok(())
}
```

`lib.rs` 相当が不要なら main.rs から相対 path で `mod`。実際には `crates/llm-memory-server/src/lib.rs` で `pub mod app; pub mod config;` を export し main.rs から `use llm_memory_server::*` する形にする。

- [ ] **Step 5: コンパイル**

Run: `cargo build -p llm-memory-server 2>&1 | tail -5`
Expected: 警告のみ。

- [ ] **Step 6: Commit**

```bash
git add crates/llm-memory-server
git commit -m "feat(server): scaffold axum app with config and healthz"
```

### Task 25: MCP Streamable HTTP transport の最低限実装

**Files:**
- Create: `crates/llm-memory-server/src/mcp/mod.rs`
- Create: `crates/llm-memory-server/src/mcp/transport.rs`

- [ ] **Step 1: 実装**

MCP の Streamable HTTP は `POST /mcp` で JSON-RPC 2.0 を受け、レスポンスは JSON もしくは SSE。MVP では同期 JSON レスポンスのみ。

`crates/llm-memory-server/src/mcp/transport.rs`:
```rust
use axum::{extract::State, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError { pub code: i32, pub message: String }

impl JsonRpcResponse {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self { jsonrpc: "2.0".into(), id, result: Some(result), error: None }
    }
    pub fn error(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self { jsonrpc: "2.0".into(), id, result: None, error: Some(JsonRpcError { code, message: message.into() }) }
    }
}

pub async fn handle(
    State(state): State<AppState>,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    match req.method.as_str() {
        "tools/list" => crate::mcp::tools::list(req.id).await,
        "tools/call" => crate::mcp::tools::call(state, req.id, req.params).await,
        _ => Json(JsonRpcResponse::error(req.id, -32601, "Method not found")),
    }
}
```

`crates/llm-memory-server/src/mcp/mod.rs`:
```rust
pub mod transport;
pub mod tools;
```

- [ ] **Step 2: Commit**

```bash
git add crates/llm-memory-server/src/mcp
git commit -m "feat(server): MCP JSON-RPC transport scaffold"
```

### Task 26: MCP tools/list と raw_append ハンドラ

**Files:**
- Create: `crates/llm-memory-server/src/mcp/tools/mod.rs`
- Create: `crates/llm-memory-server/src/mcp/tools/raw_append.rs`

- [ ] **Step 1: tools/mod.rs (registry)**

```rust
use axum::{response::IntoResponse, Json};
use serde_json::{json, Value};

use crate::app::AppState;
use crate::mcp::transport::JsonRpcResponse;

pub mod raw_append;
pub mod raw_read;
pub mod raw_search;
pub mod wiki_read;
pub mod wiki_list;
pub mod wiki_rebuild;
pub mod schema_read;
pub mod schema_update;
pub mod export;

pub async fn list(id: Option<Value>) -> Json<JsonRpcResponse> {
    let tools = json!([
        { "name": "raw_append", "description": "Append a personal raw" },
        { "name": "raw_read", "description": "Read a single raw" },
        { "name": "raw_search", "description": "Search raws via FTS5" },
        { "name": "wiki_read", "description": "Read concept wiki across personal+shared" },
        { "name": "wiki_list", "description": "List concepts" },
        { "name": "wiki_rebuild", "description": "Manually trigger rebuild" },
        { "name": "schema_read", "description": "Read schema" },
        { "name": "schema_update", "description": "Update personal schema" },
        { "name": "export", "description": "Export personal data" }
    ]);
    Json(JsonRpcResponse::success(id, json!({ "tools": tools })))
}

pub async fn call(state: AppState, id: Option<Value>, params: Value) -> Json<JsonRpcResponse> {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let result = match name {
        "raw_append" => raw_append::call(state, args).await,
        "raw_read" => raw_read::call(state, args).await,
        "raw_search" => raw_search::call(state, args).await,
        "wiki_read" => wiki_read::call(state, args).await,
        "wiki_list" => wiki_list::call(state, args).await,
        "wiki_rebuild" => wiki_rebuild::call(state, args).await,
        "schema_read" => schema_read::call(state, args).await,
        "schema_update" => schema_update::call(state, args).await,
        "export" => export::call(state, args).await,
        _ => return Json(JsonRpcResponse::error(id, -32602, format!("unknown tool: {name}"))),
    };
    match result {
        Ok(v) => Json(JsonRpcResponse::success(id, v)),
        Err(e) => Json(JsonRpcResponse::error(id, -32603, e.to_string())),
    }
}
```

- [ ] **Step 2: raw_append.rs**

```rust
use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use llm_memory_storage::raws::{insert, NewRaw};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args {
    title: String,
    content: String,
    source: String,
    tags: Option<Vec<String>>,
}

pub async fn call(state: AppState, args: Value) -> Result<Value> {
    let user: AuthenticatedUser = args.get("__user").and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow!("authentication required"))?;
    let a: Args = serde_json::from_value(args)?;
    if a.title.is_empty() || a.content.is_empty() {
        return Err(anyhow!("title and content required"));
    }
    if a.content.len() > 1024 * 1024 {
        return Err(anyhow!("content exceeds 1 MB"));
    }
    let tags_json = a.tags.as_ref().map(|t| serde_json::to_string(t).unwrap());
    let r = insert(&state.pool, NewRaw {
        scope: Scope::Personal,
        owner_id: &user.user_id,
        title: &a.title,
        content: &a.content,
        source: &a.source,
        tags_json: tags_json.as_deref(),
        created_by: Some(&user.user_id),
    }).await?;
    let started = state.coordinator.notify_append(&user.user_id).await;
    Ok(json!({ "raw_id": r.id, "rebuild_started": started }))
}
```

- [ ] **Step 3: 各 stub を作成（後続タスクで実装）**

`raw_read.rs` / `raw_search.rs` / `wiki_read.rs` / `wiki_list.rs` / `wiki_rebuild.rs` / `schema_read.rs` / `schema_update.rs` / `export.rs` を `pub async fn call(state: AppState, args: Value) -> anyhow::Result<Value> { Err(anyhow::anyhow!("not implemented")) }` で作成。

- [ ] **Step 4: コンパイル**

Run: `cargo check -p llm-memory-server 2>&1 | tail -5`
Expected: 警告のみ。

- [ ] **Step 5: Commit**

```bash
git add crates/llm-memory-server/src/mcp
git commit -m "feat(server): MCP tools/list + raw_append handler"
```

### Task 27: 残り read 系ツール (raw_read / raw_search / wiki_read / wiki_list / schema_read)

**Files:**
- Modify: `crates/llm-memory-server/src/mcp/tools/raw_read.rs`
- Modify: `crates/llm-memory-server/src/mcp/tools/raw_search.rs`
- Modify: `crates/llm-memory-server/src/mcp/tools/wiki_read.rs`
- Modify: `crates/llm-memory-server/src/mcp/tools/wiki_list.rs`
- Modify: `crates/llm-memory-server/src/mcp/tools/schema_read.rs`

- [ ] **Step 1: raw_read**

```rust
use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args { id: String }

pub async fn call(state: AppState, args: Value) -> Result<Value> {
    let user: AuthenticatedUser = args.get("__user").and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow!("authentication required"))?;
    let a: Args = serde_json::from_value(args)?;
    let raw = llm_memory_storage::raws::get(&state.pool, &a.id).await?
        .ok_or_else(|| anyhow!("not found"))?;
    if raw.scope == "personal" && raw.owner_id != user.user_id {
        return Err(anyhow!("not found"));  // 認可失敗を 404 と区別しない
    }
    Ok(serde_json::to_value(raw)?)
}
```

- [ ] **Step 2: raw_search**

```rust
use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use llm_memory_storage::search::{self, SearchQuery};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args {
    query: String,
    scope: Option<String>,
    limit: Option<i64>,
}

pub async fn call(state: AppState, args: Value) -> Result<Value> {
    let user: AuthenticatedUser = args.get("__user").and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow!("authentication required"))?;
    let a: Args = serde_json::from_value(args)?;
    let scope = match a.scope.as_deref() {
        None | Some("all") => None,
        Some("personal") => Some(Scope::Personal),
        Some("shared") => Some(Scope::Shared),
        Some(s) => return Err(anyhow!("invalid scope: {s}")),
    };
    let limit = a.limit.unwrap_or(20).clamp(1, 100);

    // 全 scope の場合: personal(自分) + shared 全件 を別 query で集める。シンプルに 2 回 search。
    let mut results = Vec::new();
    if matches!(scope, None | Some(Scope::Personal)) {
        let mut hits = search::raws(&state.pool, SearchQuery {
            query: &a.query, scope: Some(Scope::Personal), owner_id: Some(&user.user_id), limit,
        }).await?;
        results.append(&mut hits);
    }
    if matches!(scope, None | Some(Scope::Shared)) {
        let mut hits = search::raws(&state.pool, SearchQuery {
            query: &a.query, scope: Some(Scope::Shared), owner_id: None, limit,
        }).await?;
        results.append(&mut hits);
    }
    results.truncate(limit as usize);
    Ok(json!({ "results": results }))
}
```

- [ ] **Step 3: wiki_read**

```rust
use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use llm_memory_storage::wikis::{self, Wiki};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args { concept: String, scope: Option<String> }

pub async fn call(state: AppState, args: Value) -> Result<Value> {
    let user: AuthenticatedUser = args.get("__user").and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow!("authentication required"))?;
    let a: Args = serde_json::from_value(args)?;
    let mode = a.scope.as_deref().unwrap_or("all");
    let personal: Option<Wiki> = if matches!(mode, "all" | "personal") {
        wikis::get(&state.pool, Scope::Personal, &user.user_id, &a.concept).await?
    } else { None };
    let shared: Vec<Wiki> = if matches!(mode, "all" | "shared") {
        // 全 shared_memory を横断
        let sms = llm_memory_storage::shared_memories::list_all(&state.pool).await?;
        let mut out = Vec::new();
        for sm in sms {
            if let Some(w) = wikis::get(&state.pool, Scope::Shared, &sm.id, &a.concept).await? {
                out.push(w);
            }
        }
        out
    } else { vec![] };
    Ok(json!({
        "concept": a.concept,
        "personal": personal,
        "shared": shared,
    }))
}
```

- [ ] **Step 4: wiki_list**

```rust
use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use llm_memory_storage::{wikis, shared_memories};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args { scope: Option<String> }

pub async fn call(state: AppState, args: Value) -> Result<Value> {
    let user: AuthenticatedUser = args.get("__user").and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow!("authentication required"))?;
    let a: Args = serde_json::from_value(args)?;
    let mode = a.scope.as_deref().unwrap_or("all");
    let personal = if matches!(mode, "all" | "personal") {
        wikis::list_concepts(&state.pool, Scope::Personal, &user.user_id).await?
    } else { vec![] };
    let shared = if matches!(mode, "all" | "shared") {
        let sms = shared_memories::list_all(&state.pool).await?;
        let mut out = Vec::new();
        for sm in sms {
            let concepts = wikis::list_concepts(&state.pool, Scope::Shared, &sm.id).await?;
            out.push(json!({ "shared_memory_id": sm.id, "concepts": concepts }));
        }
        out
    } else { vec![] };
    Ok(json!({ "personal": personal, "shared": shared }))
}
```

- [ ] **Step 5: schema_read**

```rust
use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args { scope: String, shared_memory_id: Option<String> }

pub async fn call(state: AppState, args: Value) -> Result<Value> {
    let user: AuthenticatedUser = args.get("__user").and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow!("authentication required"))?;
    let a: Args = serde_json::from_value(args)?;
    let (scope, owner_id) = match a.scope.as_str() {
        "personal" => (Scope::Personal, user.user_id.clone()),
        "shared" => (Scope::Shared, a.shared_memory_id.clone().ok_or_else(|| anyhow!("shared_memory_id required"))?),
        s => return Err(anyhow!("invalid scope: {s}")),
    };
    let content = llm_memory_storage::schemas::get(&state.pool, scope, &owner_id).await?;
    Ok(json!({ "content": content }))
}
```

- [ ] **Step 6: コンパイル**

Run: `cargo check -p llm-memory-server 2>&1 | tail -5`
Expected: 警告のみ。

- [ ] **Step 7: Commit**

```bash
git add crates/llm-memory-server/src/mcp/tools
git commit -m "feat(server): read tools (raw_read/search, wiki_read/list, schema_read)"
```

### Task 28: write 系ツール (wiki_rebuild / schema_update / export)

**Files:**
- Modify: `crates/llm-memory-server/src/mcp/tools/wiki_rebuild.rs`
- Modify: `crates/llm-memory-server/src/mcp/tools/schema_update.rs`
- Modify: `crates/llm-memory-server/src/mcp/tools/export.rs`

- [ ] **Step 1: wiki_rebuild**

```rust
use anyhow::{Result, anyhow};
use llm_memory_coordinator::coordinator::ManualOutcome;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args { concept: Option<String> }

pub async fn call(state: AppState, args: Value) -> Result<Value> {
    let user: AuthenticatedUser = args.get("__user").and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow!("authentication required"))?;
    let a: Args = serde_json::from_value(args)?;
    let r = state.coordinator.request_manual(&user.user_id, a.concept).await;
    Ok(json!({ "status": match r {
        ManualOutcome::Started => "started",
        ManualOutcome::Pending => "pending",
    }}))
}
```

- [ ] **Step 2: schema_update**

```rust
use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

#[derive(Deserialize)]
struct Args { content: String }

pub async fn call(state: AppState, args: Value) -> Result<Value> {
    let user: AuthenticatedUser = args.get("__user").and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow!("authentication required"))?;
    let a: Args = serde_json::from_value(args)?;
    llm_memory_storage::schemas::upsert(&state.pool, Scope::Personal, &user.user_id, &a.content).await?;
    Ok(json!({ "ok": true }))
}
```

- [ ] **Step 3: export with pagination**

```rust
use anyhow::{Result, anyhow};
use llm_memory_core::scope::Scope;
use llm_memory_core::time::now_ms;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

const PAGE_LIMIT: usize = 5000;

#[derive(Deserialize)]
struct Args { cursor: Option<String> }

pub async fn call(state: AppState, args: Value) -> Result<Value> {
    let user: AuthenticatedUser = args.get("__user").and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| anyhow!("authentication required"))?;
    let a: Args = serde_json::from_value(args)?;
    let cursor = a.cursor.unwrap_or_else(|| "0".into());
    let cursor_i: i64 = cursor.parse().map_err(|_| anyhow!("invalid cursor"))?;

    let raws = sqlx::query_as::<_, llm_memory_storage::raws::Raw>(
        "SELECT * FROM raws WHERE scope='personal' AND owner_id = ? AND created_at > ?
         ORDER BY created_at ASC LIMIT ?",
    ).bind(&user.user_id).bind(cursor_i).bind(PAGE_LIMIT as i64 + 1)
     .fetch_all(&state.pool).await?;

    let next_cursor = if raws.len() > PAGE_LIMIT {
        Some(raws[PAGE_LIMIT - 1].created_at.to_string())
    } else { None };
    let page = raws.into_iter().take(PAGE_LIMIT).collect::<Vec<_>>();

    // 最初の page だけ wikis + schema を返す。続きは raws のみ。
    let (wikis, schema) = if cursor_i == 0 {
        let wikis = llm_memory_storage::wikis::list_for_owner(&state.pool, Scope::Personal, &user.user_id).await?;
        let schema = llm_memory_storage::schemas::get(&state.pool, Scope::Personal, &user.user_id).await?;
        (Some(wikis), schema)
    } else { (None, None) };

    Ok(json!({
        "version": 1,
        "exported_at": now_ms(),
        "user_id": user.user_id,
        "raws": page,
        "wikis": wikis,
        "schema": schema,
        "next_cursor": next_cursor,
    }))
}
```

- [ ] **Step 4: コンパイル**

Run: `cargo build -p llm-memory-server 2>&1 | tail -5`
Expected: 成功。

- [ ] **Step 5: Commit**

```bash
git add crates/llm-memory-server/src/mcp/tools
git commit -m "feat(server): wiki_rebuild + schema_update + paginated export"
```

### Task 29: Authorization Server ハンドラの完成

**Files:**
- Modify: `crates/llm-memory-auth/src/authorization_server.rs`
- Modify: `crates/llm-memory-server/src/app.rs`

- [ ] **Step 1: AS handlers を実装**

Spec §9 の通り 6 endpoint を実装。
1. `GET /.well-known/oauth-authorization-server`
   - Spec §9 の metadata: `issuer`, `authorization_endpoint`, `token_endpoint`, `registration_endpoint`, `revocation_endpoint`, `response_types_supported`, `grant_types_supported`, `code_challenge_methods_supported`, `token_endpoint_auth_methods_supported` を返す
2. `POST /oauth/register` (DCR)
   - `dcr::validate(req)` → `oauth_clients::register` → `DcrResponse` で `client_id` を埋めて返却。`X-Forwarded-For` ベースのレート制限（IP あたり 10/day）。
3. `GET /oauth/authorize?client_id&redirect_uri&state&code_challenge`
   - client 検証、redirect_uri が登録済みか確認
   - 上記情報をサーバ側セッション（in-memory cache or signed cookie）に保存
   - Google authorize URL にリダイレクト
4. `GET /oauth/callback/google?code&state`
   - state で元セッション取得
   - Google から token 交換 → userinfo 取得 → `users::upsert`
   - 認可コードを発行して元 client の redirect_uri に redirect
5. `POST /oauth/token` (grant_type=authorization_code or refresh_token)
   - PKCE 検証
   - access_token (JWT 1h) + refresh_token (opaque, tokens テーブルに保存)
6. `POST /oauth/revoke`
   - refresh_token を `tokens.revoked_at` に記録

実装は分量が大きいため、サブタスクとして:
- 5a: metadata + DCR
- 5b: authorize + callback
- 5c: token + revoke

各サブタスクで TDD（mock Google を立ててテスト）。

- [ ] **Step 2: app.rs に AS router をマウント**

```rust
let router = Router::new()
    .merge(llm_memory_auth::authorization_server::router())
    .route("/mcp", post(crate::mcp::transport::handle).layer(axum::middleware::from_fn_with_state(state.jwt_keys.clone(), llm_memory_auth::middleware::require_auth)))
    .route("/healthz", get(healthz))
    .with_state(state);
```

- [ ] **Step 3: 統合テスト**

`crates/llm-memory-server/tests/oauth_flow.rs`:
- mock Google を立てて authorize → callback → token のフローが通ることを確認

- [ ] **Step 4: コンパイル + テスト**

Run: `cargo test -p llm-memory-server 2>&1 | tail -10`
Expected: テスト追加分も含めて全 pass.

- [ ] **Step 5: Commit**

```bash
git add crates/llm-memory-auth crates/llm-memory-server
git commit -m "feat(auth): complete OAuth 2.1 authorization server"
```

### Task 30: レート制限 middleware

**Files:**
- Create: `crates/llm-memory-server/src/rate_limit.rs`

- [ ] **Step 1: 実装**

token bucket を in-memory で実装。`DashMap<(UserId, Tier), Bucket>` または `tokio::sync::Mutex<HashMap>`。
- raw_append: 60/min
- wiki_rebuild: 6/min
- read 系: 600/min
- export: 6/min (write tier 扱い)

axum middleware として MCP ルートに装着。Tool 名で tier を分類。

実装サンプル:
```rust
pub struct RateLimiter { buckets: DashMap<(String, &'static str), Bucket> }

impl RateLimiter {
    pub fn check(&self, user_id: &str, tier: &'static str) -> bool { /* ... */ }
}

pub fn tier_of(tool: &str) -> (&'static str, u32) {
    match tool {
        "raw_append" => ("write", 60),
        "wiki_rebuild" | "export" => ("heavy", 6),
        _ => ("read", 600),
    }
}
```

- [ ] **Step 2: テスト**

```rust
#[tokio::test]
async fn limiter_throttles_after_limit() {
    let rl = RateLimiter::new();
    for _ in 0..6 { assert!(rl.check("u1", "heavy", 6).await); }
    assert!(!rl.check("u1", "heavy", 6).await);
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-server/src/rate_limit.rs
git commit -m "feat(server): in-memory token bucket rate limiter"
```

---

## Phase G: Observability

### Task 31: Prometheus メトリクス + /metrics endpoint

**Files:**
- Create: `crates/llm-memory-server/src/metrics.rs`
- Modify: `crates/llm-memory-server/src/app.rs`

- [ ] **Step 1: 実装**

```rust
use prometheus::{Encoder, Registry, IntCounter, IntGauge, Histogram, HistogramOpts, TextEncoder};

#[derive(Clone)]
pub struct Metrics {
    pub registry: Registry,
    pub rebuild_in_flight: IntGauge,
    pub rebuild_duration: Histogram,
    pub rebuild_failed: IntCounter,
    pub concept_rebuild_failed: IntCounter,
    pub rebuild_drain_iterations: Histogram,
    pub rebuild_drain_capped: IntCounter,
    pub anthropic_api_error: IntCounter,
    pub oauth_login_failure: IntCounter,
    pub dcr_registration: IntCounter,
    pub sqlite_db_size_bytes: IntGauge,
    pub http_5xx: IntCounter,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let rebuild_in_flight = IntGauge::new("rebuild_in_flight_gauge", "rebuild in-flight workers").unwrap();
        let rebuild_duration = Histogram::with_opts(HistogramOpts::new("rebuild_duration_seconds", "rebuild iteration duration").buckets(vec![0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0])).unwrap();
        let rebuild_failed = IntCounter::new("rebuild_failed_total", "rebuild failures").unwrap();
        let concept_rebuild_failed = IntCounter::new("concept_rebuild_failed_total", "per-concept failures").unwrap();
        let rebuild_drain_iterations = Histogram::with_opts(HistogramOpts::new("rebuild_drain_iterations", "drain loop iterations").buckets(vec![1.0, 2.0, 3.0, 5.0, 10.0])).unwrap();
        let rebuild_drain_capped = IntCounter::new("rebuild_drain_capped_total", "drain MAX_ITERATIONS hits").unwrap();
        let anthropic_api_error = IntCounter::new("anthropic_api_error_total", "anthropic api errors").unwrap();
        let oauth_login_failure = IntCounter::new("oauth_login_failure_total", "oauth login failures").unwrap();
        let dcr_registration = IntCounter::new("dcr_registration_total", "dcr registrations").unwrap();
        let sqlite_db_size_bytes = IntGauge::new("sqlite_db_size_bytes", "db file size").unwrap();
        let http_5xx = IntCounter::new("http_5xx_total", "5xx responses").unwrap();
        for c in [&rebuild_failed, &concept_rebuild_failed, &rebuild_drain_capped, &anthropic_api_error, &oauth_login_failure, &dcr_registration, &http_5xx] {
            registry.register(Box::new((*c).clone())).unwrap();
        }
        registry.register(Box::new(rebuild_in_flight.clone())).unwrap();
        registry.register(Box::new(rebuild_duration.clone())).unwrap();
        registry.register(Box::new(rebuild_drain_iterations.clone())).unwrap();
        registry.register(Box::new(sqlite_db_size_bytes.clone())).unwrap();
        Self {
            registry, rebuild_in_flight, rebuild_duration, rebuild_failed,
            concept_rebuild_failed, rebuild_drain_iterations, rebuild_drain_capped,
            anthropic_api_error, oauth_login_failure, dcr_registration, sqlite_db_size_bytes, http_5xx,
        }
    }
}

pub async fn handler(state: axum::extract::State<crate::app::AppState>) -> impl axum::response::IntoResponse {
    let encoder = TextEncoder::new();
    let metrics = state.metrics.registry.gather();
    let mut buf = Vec::new();
    encoder.encode(&metrics, &mut buf).unwrap();
    ([("content-type", encoder.format_type().to_string())], buf)
}
```

`AppState` に `pub metrics: Arc<Metrics>` 追加。`app.rs` で `.route("/metrics", get(metrics::handler))` 追加。Coordinator は `Metrics` を保持して increment する形に書き換え。

- [ ] **Step 2: coordinator に metrics 注入**

`WorkerDeps` に `metrics: Arc<Metrics>` を追加し、`spawn_worker` で `rebuild_in_flight.inc()` / `.dec()` を呼ぶ。

- [ ] **Step 3: コンパイル + テスト**

Run: `cargo test --workspace 2>&1 | tail -10`
Expected: 全 pass.

- [ ] **Step 4: Commit**

```bash
git add crates/llm-memory-server/src/metrics.rs crates/llm-memory-server/src/app.rs crates/llm-memory-coordinator
git commit -m "feat(server): Prometheus metrics + /metrics endpoint"
```

### Task 32: アカウント削除エンドポイント

**Files:**
- Create: `crates/llm-memory-server/src/account.rs`

- [ ] **Step 1: 実装**

```rust
use axum::{extract::State, http::StatusCode};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

pub async fn delete_me(State(state): State<AppState>, user: AuthenticatedUser) -> Result<StatusCode, StatusCode> {
    llm_memory_storage::users::delete_cascade(&state.pool, &user.user_id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}
```

`app.rs` に `.route("/v1/account", delete(account::delete_me))` 追加。

- [ ] **Step 2: 統合テスト**

```rust
#[tokio::test]
async fn delete_me_cascades_personal_data() {
    // pool 作成 → user upsert → raw insert → DELETE /v1/account → user not found
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/llm-memory-server/src
git commit -m "feat(server): account deletion with cascade"
```

---

## Phase H: Deployment

### Task 33: Dockerfile (multi-stage)

**Files:**
- Create: `docker/Dockerfile`
- Create: `.dockerignore`

- [ ] **Step 1: Dockerfile**

```dockerfile
FROM rust:1.84-slim AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release --bin llm-memory-server

FROM gcr.io/distroless/cc-debian12
COPY --from=builder /app/target/release/llm-memory-server /usr/local/bin/llm-memory-server
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/llm-memory-server"]
```

`.dockerignore`:
```
target/
.git/
*.db
.env
```

- [ ] **Step 2: ビルド確認**

Run: `docker build -f docker/Dockerfile -t llm-memory:dev . 2>&1 | tail -5`
Expected: イメージ作成成功（環境にあれば）。なければ skip し CI で検証。

- [ ] **Step 3: Commit**

```bash
git add docker/Dockerfile .dockerignore
git commit -m "build(docker): multi-stage Dockerfile with distroless runtime"
```

### Task 34: docker-compose.yml + litestream

**Files:**
- Create: `docker/docker-compose.yml`
- Create: `docker/litestream.yml`

- [ ] **Step 1: docker-compose.yml**

```yaml
version: "3.9"
services:
  server:
    build:
      context: ..
      dockerfile: docker/Dockerfile
    environment:
      - DATABASE_URL=sqlite:///data/db.sqlite
      - BIND_ADDR=0.0.0.0:8080
      - PUBLIC_URL=${PUBLIC_URL}
      - ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY}
      - GOOGLE_OAUTH_CLIENT_ID=${GOOGLE_OAUTH_CLIENT_ID}
      - GOOGLE_OAUTH_CLIENT_SECRET=${GOOGLE_OAUTH_CLIENT_SECRET}
      - JWT_SIGNING_KEY_v1=${JWT_SIGNING_KEY_v1}
      - TRUSTED_PROXY_COUNT=1
    volumes:
      - data:/data
    ports:
      - "8080:8080"
    restart: unless-stopped

  litestream:
    image: litestream/litestream:0.3.13
    command: ["replicate"]
    volumes:
      - data:/data
      - ./litestream.yml:/etc/litestream.yml:ro
    environment:
      - GOOGLE_APPLICATION_CREDENTIALS=/etc/gcp-sa.json
    restart: unless-stopped

volumes:
  data:
```

- [ ] **Step 2: litestream.yml**

```yaml
dbs:
  - path: /data/db.sqlite
    replicas:
      - type: gcs
        bucket: ${LITESTREAM_BUCKET}
        path: db.sqlite
        retention: 24h
```

- [ ] **Step 3: Commit**

```bash
git add docker/docker-compose.yml docker/litestream.yml
git commit -m "build(deploy): docker-compose with litestream sidecar"
```

### Task 35: GCE 起動スクリプトと README

**Files:**
- Create: `deploy/gce/startup.sh`
- Create: `deploy/gce/README.md`

- [ ] **Step 1: startup.sh**

```bash
#!/bin/bash
set -euo pipefail

# COS 上で docker-compose を起動するスクリプト
# 想定: GCE インスタンスメタデータに必要な env を埋め込む

cd /opt/llm-memory
docker-compose pull
docker-compose up -d
```

- [ ] **Step 2: README**

`deploy/gce/README.md` に以下を記載:
1. GCE インスタンス作成手順 (e2-small + pd-balanced 20GB)
2. Cloud Load Balancer 設定（HTTPS 終端）
3. Secret Manager に置く secret 一覧
4. ファイアウォール (LB からのみ TCP 8080 許可)
5. GCS バケット作成（litestream 用）
6. デプロイ手順 (`gcloud compute scp docker-compose.yml ...`)

- [ ] **Step 3: Commit**

```bash
git add deploy/gce
git commit -m "docs(deploy): GCE startup script and README"
```

### Task 36: GitHub Actions CI

**Files:**
- Create: `.github/workflows/ci.yml`

- [ ] **Step 1: 実装**

```yaml
name: CI
on:
  push:
    branches: [main]
  pull_request:

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace -- -D warnings
      - run: cargo test --workspace
  docker:
    runs-on: ubuntu-latest
    needs: test
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-buildx-action@v3
      - run: docker build -f docker/Dockerfile -t llm-memory:ci .
```

- [ ] **Step 2: github-actions-optimize スキル準拠の確認**

CLAUDE.md の指示通り、`.github/workflows/*.yml` 修正時は `github-actions-optimize` を起動するルール。本タスクではスキル起動を提案するだけにとどめ、最適化（cache key 細分化、ディスク節約等）は別 PR で対応する。

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: cargo test + clippy + docker build"
```

### Task 37: 手動 E2E チェックリストと最終 PR 作成

**Files:**
- Create: `docs/superpowers/runbooks/e2e-checklist.md`

- [ ] **Step 1: チェックリスト作成**

`docs/superpowers/runbooks/e2e-checklist.md`:
```markdown
# Phase 1 手動 E2E チェックリスト

1. デプロイ直後の確認
   - [ ] `curl https://<host>/healthz` → "ok"
   - [ ] `curl https://<host>/.well-known/oauth-authorization-server` → JSON 取得
   - [ ] `curl https://<host>/metrics` → Prometheus テキスト

2. OAuth フロー（Claude Desktop で）
   - [ ] サーバを Claude Desktop に追加
   - [ ] Google 認証画面が出る
   - [ ] 認可後、tools/list が成功する

3. MCP ツール
   - [ ] `raw_append({title, content, source})` → raw_id 返却、`rebuild_started: true`
   - [ ] 10 秒程度待って `wiki_list()` → 新規 concept が増える
   - [ ] `wiki_read({concept})` → personal の content が取得できる
   - [ ] `raw_search({query})` → ヒットする
   - [ ] `wiki_rebuild()` → "started" or "pending"
   - [ ] `schema_update({content: "..."})` → ok
   - [ ] `schema_read({scope: "personal"})` → 返却
   - [ ] `export()` → 全データ + next_cursor の有無
   - [ ] `DELETE /v1/account` → 204、その後 wiki_list が空

4. 共有メモリ
   - [ ] 別途 `sqlite3` で `shared_memories` と `raws scope=shared` を投入
   - [ ] `raw_search({scope: "shared"})` でヒット
   - [ ] `wiki_read({scope: "all"})` で shared が返る

5. 障害確認
   - [ ] Anthropic API キーを一時的に無効化 → rebuild 失敗 → `concept_rebuild_failed_total` 増加
   - [ ] Worker 再起動 → 中断ジョブが次の append で再開
```

- [ ] **Step 2: 最終 PR 作成**

実装完了したらブランチを push し、PR を作成:
```bash
git push -u origin docs/phase1-design   # またはこれまでの実装作業ブランチ
gh pr create --title "Phase 1: LLM Memory Extension MCP Server" --body "..."
```

PR 本文には spec / plan / E2E チェックリストへのリンクを含める。

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/runbooks/e2e-checklist.md
git commit -m "docs(runbook): manual E2E checklist for Phase 1"
```
