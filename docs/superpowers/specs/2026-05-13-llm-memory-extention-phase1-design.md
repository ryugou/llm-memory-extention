# LLM Memory Extension — Phase 1 設計書

- 日付: 2026-05-13
- ステータス: 初回レビュー反映済み、再レビュー待ち
- 著者: Toshihiko Ryugo（ブレインストーミング: Claude）

## 1. 目的

Claude.ai / ChatGPT / Claude Code から MCP 経由で参照できる「個人メモリ」「共有メモリ」を提供する Remote MCP サーバを構築する。raw（生の記録）を蓄積し、概念ごとの wiki（合成知識）を LLM によって rebuild してキャッシュし、Claude が回答時に参照する。

Phase 2（将来）では rebuild の中身を Vegapunk（社内 GraphRAG エンジン）に差し替えられる構造を残す。

## 2. スコープ

Phase 1 の MVP として、設計の全体像を維持しつつ、ブレインストーミングで合意した以下の簡略化を行う：

- 認証プロバイダは **Google OAuth のみ**
- `org` 概念を撤廃し、**共有メモリは ID 管理された複数エンティティ** (`shared_memories(id, name)`)
- アクセス制御テーブルを持たず、共有メモリは **ログイン済みユーザ全員が強制参照**
- 共有メモリへの書き込みは MCP からは行わない（read only）。raws / wikis / schemas (`scope='shared'`) は **サーバ外** で SQLite に直接書き込まれる前提
- 共有 scope の LLM rebuild は **走らせない**
- `admin` / `role` の概念は持たない
- `raw_append` に伴う wiki rebuild は **非同期**。`(scope=personal, owner=user.id)` 単位で同時 1 本に制限。rebuild 中の append は raws 保存のみで rebuild を trigger しない。worker は完了直前に **drain loop** で「rebuild 中に追加された raws」を取り込むまでループし、その後 idle に戻る。手動 `wiki_rebuild` ツールも提供
- rebuild の進行状態は **プロセス内 in-memory state** のみで管理（DB に job 履歴テーブルを持たない）。再起動で状態は失われるが、次の append が新規 rebuild を起こすため運用上問題なし
- 失敗 concept の自動再試行は **行わない**。失敗時は manual `wiki_rebuild` 推奨
- raw の delete / update API、schema の validation / lint、Apple Sign In / Magic Link は MVP 対象外

### 2.1 受け入れ済みのトレードオフ

MVP として明示的に受容している運用リスクとその境界条件：

| トレードオフ | 受け入れ条件 | 解除条件（再評価のトリガ） |
|---|---|---|
| 共有メモリのログイン全員参照 | 1 インスタンス = 1 信頼境界（社内デプロイ等）。マルチテナント運用は想定しない | 複数の信頼境界をまたぐ運用が出てきたら `shared_memory_members(user_id, shared_memory_id, role)` を追加し、認可で参照可否を絞る |
| 共有メモリ MCP write 不可 | 投入は運用領域（直接 SQL / CLI / 別パイプライン） | エンドユーザが自分で書き込みたい運用が出たら write 系ツールを追加 |
| rebuild の eventual consistency | rebuild 中の append は次回 append まで反映されない。staleness window は最大「手動 `wiki_rebuild` まで無期限」 | UI 即時反映が要件になったら sync rebuild モードを追加 |
| シングルライタ SQLite | 同時書き込み無し、busy_timeout で待機 | 同時書き込み競合が頻発したら Phase 2 移行 |
| SLO 無し | 可用性・レイテンシのコミットなし。best effort | 業務利用で SLO が要求されたら定義し、観測性を強化 |
| キュー公平性なし（並行実行は per-owner で 1 本に制限のみ、グローバルキューなし） | 全 owner の rebuild は to do リストではなく単純に tokio task で spawn。Anthropic API のレートリミットが事実上の上限 | per-owner キュー / 重み付きスケジューラを導入 |
| 再起動で in-memory rebuild state が失われる | 進行中だった rebuild は中断。raws はすでに DB にあるので次の append が再 rebuild を起こす。途中まで更新された wikis は次回再合成で正される | DB-backed job state が必要なら復活時に再導入 |

## 3. システム全体図

```
┌──────────────────────────────────────────┐
│  Claude.ai / ChatGPT / Claude Code       │
└──────────────┬───────────────────────────┘
               │ MCP + OAuth 2.1 (Google を ID プロバイダに委譲)
               ▼
┌──────────────────────────────────────────┐
│  Remote MCP Server (Rust + axum)         │
│  - Google OAuth                          │
│  - MCP tools (personal R/W, shared RO)   │
│  - 非同期 rebuild worker (personal のみ) │
└──────────────┬───────────────────────────┘
               ▼
┌──────────────────────────────────────────┐
│  SQLite (+ FTS5)  [GCE VM 永続ディスク + litestream→GCS] │
│  - users                                 │
│  - shared_memories                       │
│  - raws  (scope: personal | shared)      │
│  - wikis (scope: personal | shared)      │
│  - schemas (scope: personal | shared)    │
│  - oauth_clients                         │
│  - tokens (refresh)                      │
│                                          │
│  (rebuild 進行状態は in-memory state のみ)│
└──────────────────────────────────────────┘
```

共有 scope のデータはサーバ外パイプライン（CLI、別ツール、直接 SQL、将来は Vegapunk export 等）で SQLite に投入される。サーバはこれを read するだけ。

## 4. データモデル

### 4.1 共通規約

- **ID 規約**: 主キー `id` は原則アプリ生成の **ULID**（26 文字、Crockford Base32、辞書順 = 生成時刻順）。
  - **例外**: `shared_memories.id` のみ運用者が安定キーを割り当てる人間可読 ID を許容する（例: `company-wide`）。形式制約: 小文字英数字とハイフン、長さ 1〜64、`^[a-z0-9][a-z0-9-]{0,63}$`。
- **タイムスタンプ**: `INTEGER` で Unix epoch ms。
- **接続設定 (PRAGMA)**: 起動時に以下を発行する。
  ```
  PRAGMA journal_mode = WAL;        -- litestream 必須
  PRAGMA busy_timeout = 5000;       -- 5 秒
  PRAGMA foreign_keys = ON;
  PRAGMA synchronous = NORMAL;      -- WAL + litestream で十分
  PRAGMA wal_autocheckpoint = 1000;
  ```
- **litestream 契約**: WAL モード必須。`replicate` 設定で GCS バケットに 24h リテンション。restore は cold start 時に実行。同時に複数プロセスから WAL に書かないこと（VM 1 台、サーバ 1 プロセスを守る）。

### 4.2 テーブル定義

```sql
CREATE TABLE users (
  id TEXT PRIMARY KEY,
  provider TEXT NOT NULL,        -- 'google' (MVP)
  subject TEXT NOT NULL,         -- Google sub
  email TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE(provider, subject)
);

CREATE TABLE shared_memories (
  id TEXT PRIMARY KEY,           -- 例: 'company-wide', 'team-frontend'
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE raws (
  id TEXT PRIMARY KEY,
  scope TEXT NOT NULL CHECK (scope IN ('personal','shared')),
  owner_id TEXT NOT NULL,         -- personal: user.id / shared: shared_memory.id
  title TEXT NOT NULL,
  content TEXT NOT NULL,
  source TEXT NOT NULL,
  tags TEXT,                       -- JSON 文字列
  created_by TEXT,                 -- personal は user.id。shared 外部投入なら NULL 可
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_raws_scope_owner ON raws(scope, owner_id);
CREATE INDEX idx_raws_created_at ON raws(created_at);

CREATE TABLE wikis (
  scope TEXT NOT NULL CHECK (scope IN ('personal','shared')),
  owner_id TEXT NOT NULL,
  concept TEXT NOT NULL,
  content TEXT NOT NULL,
  source_refs TEXT NOT NULL,       -- JSON array of raw ids
  last_rebuilt_at INTEGER NOT NULL,
  PRIMARY KEY (scope, owner_id, concept)
);

CREATE TABLE schemas (
  scope TEXT NOT NULL CHECK (scope IN ('personal','shared')),
  owner_id TEXT NOT NULL,
  content TEXT NOT NULL,           -- CLAUDE.md 相当 (MVP では保存と取り出しのみ)
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (scope, owner_id)
);

-- rebuild の進行状態は DB に持たない (in-memory state のみ)
-- §7 を参照

CREATE TABLE oauth_clients (
  id TEXT PRIMARY KEY,             -- DCR 発行 client_id (ULID)
  redirect_uris TEXT NOT NULL,     -- JSON array
  grant_types TEXT NOT NULL,       -- JSON array
  token_endpoint_auth_method TEXT NOT NULL,
  client_name TEXT,
  created_at INTEGER NOT NULL,
  last_seen_at INTEGER,
  revoked_at INTEGER
);

CREATE TABLE tokens (
  refresh_token TEXT PRIMARY KEY,  -- opaque
  user_id TEXT NOT NULL REFERENCES users(id),
  client_id TEXT NOT NULL REFERENCES oauth_clients(id),
  expires_at INTEGER NOT NULL,
  revoked_at INTEGER
);

CREATE VIRTUAL TABLE raws_fts USING fts5(
  title, content, tags,
  content='raws', content_rowid='rowid'
);

-- raws_fts の同期トリガ
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

-- wikis_fts は MVP の MCP ツールから参照されないため作成しない (YAGNI)
-- wiki の全文検索が必要になった時点で別途追加する
```

## 5. MCP ツールセット

| ツール | 種別 | 認可 | 動作 |
|---|---|---|---|
| `raw_search(query, scope='all'\|'personal'\|'shared', tags?, date_range?, limit=20)` | read | login | FTS5 BM25 検索。`scope='all'` は personal(self) ∪ 全 shared。 |
| `raw_read(id)` | read | login + (personal なら owner==self) | 単一 raw |
| `wiki_read(concept, scope='all'\|'personal'\|'shared')` | read | login | personal(self) と shared を **分離した配列** で返す（呼び出し側 LLM が両方読み合成する想定） |
| `wiki_list(scope='all'\|'personal'\|'shared')` | read | login | concept 名一覧（軽量メタのみ） |
| `schema_read(scope='personal'\|'shared')` | read | login | CLAUDE.md 相当を取得 |
| `raw_append(title, content, source, tags?)` | personal write | login | scope は **personal 固定**。レスポンス: `{ raw_id, rebuild_started: bool }` |
| `wiki_rebuild(concept?)` | personal write | login | concept 省略時は対象 owner の全 concept |
| `schema_update(content)` | personal write | login | scope は **personal 固定**。MVP では保存のみで validation は走らせない |
| `export()` | personal write | login | personal の raws + wikis + schema を 1 つの JSON で返す |

MVP に含めないもの:
- `lint(scope)` — schema 検査は MVP では走らせない
- shared への書き込み系全部
- raw の delete / update — 必要なら直接 SQLite を編集

### 5.1 入出力の JSON 形（抜粋）

```jsonc
// wiki_read の戻り
{
  "concept": "vegapunk",
  "personal": {            // null 可（personal に該当なし、または scope='shared' 指定）
    "content": "...",
    "source_refs": ["01HJ...", "01HK..."],
    "last_rebuilt_at": 1736120400000
  },
  "shared": [              // 空配列可（複数の shared_memory にまたがる）
    {
      "shared_memory_id": "company-wide",
      "content": "...",
      "source_refs": [...],
      "last_rebuilt_at": 1736120000000
    }
  ]
}

// raw_append の戻り
{ "raw_id": "01HJ...", "rebuild_started": true }   // running 中なら false

// export の戻り
{
  "version": 1,
  "exported_at": 1736120400000,
  "user_id": "01HJ...",
  "raws":    [...],
  "wikis":   [...],
  "schema":  "..."
}
```

### 5.2 制限（rate limit / body size / export）

| 対象 | 制限 |
|---|---|
| `raw_append` の `content` | 1 raw あたり 1 MB |
| `raw_append` 呼び出し頻度 | per user 60 / 分（token bucket） |
| `wiki_rebuild` 呼び出し頻度 | per user 6 / 分（高コストのため強め） |
| `export()` のレスポンス | 50 MB を超える場合は `cursor` 付きでページング（次回呼び出しに `cursor` 引数を渡す） |
| その他 read 系 | per user 600 / 分 |

レート制限超過は 429、`Retry-After` ヘッダ付き。

`export()` を write 分類にしているのは、DB 全件読みで LLM 課金は出ないが I/O が重く乱用懸念が write 系と近いため。意味上の write ではなく **コスト分類上の write tier**。

## 6. raw_append 同期フロー（即返却）

```
1. OAuth bearer 検証 → user_id 確定
2. 引数 validation (title/content 非空、tags は JSON parseable)
3. BEGIN TRANSACTION
4.   INSERT INTO raws (scope='personal', owner_id=user_id, created_by=user_id, ...)
5.   raws_fts は AFTER INSERT トリガで同期
6. COMMIT
7. RebuildCoordinator.notify_append(user_id):
     - state[user_id].running が false なら true にセットして worker を spawn
     - true なら何もしない (lazy drain)
8. レスポンス: { raw_id, rebuild_started: bool }
```

スレッドセーフな state map（例: `tokio::sync::Mutex<HashMap<OwnerKey, RebuildState>>`、または `DashMap`）でロックを取り、判定とフラグ更新を atomic に行う。

## 7. 非同期 rebuild worker

```rust
// 概念モデル (型は仮)
struct RebuildState {
    running: bool,
    manual_pending: bool,
}

// state map: HashMap<(Scope, OwnerId), RebuildState>
```

worker は tokio task として spawn され、内部で **drain loop** を回す。これは rebuild 中に追加された raws を取りこぼさないため。

```
spawn 後の主ループ:

loop {
    started_at = now_ms()  // この iteration の wiki cursor になる値

    1. 前回 rebuild 以降の raws を取得:
       - 起点 = MAX(wikis.last_rebuilt_at WHERE scope='personal' AND owner_id=?)
         該当なし (初回) なら 0
       - 終点 = started_at
       - raws WHERE scope='personal' AND owner_id=?
                AND created_at > 起点 AND created_at <= started_at

    2. new_raws が空なら break (drain 完了)

    3. 影響 concepts 特定:
       - 既存 wikis(scope='personal', owner_id) の concept 名集合を取得
       - new_raws の tags JSON で既存 concept にマッチするもの
       - Haiku (claude-haiku-4-5-20251001) で「new_raws.content + 既存 concept 一覧」
         から影響 concept + 新規追加 concept を抽出
       - 上記の和集合

    4. 影響 concept ごとに並列 (concurrency 上限 4):
       - 入力 raw 集合 = 既存 wiki.source_refs ∪ new_raws ∪ FTS5(concept) top-k
         (上限 50, BM25 で間引き)
       - Sonnet (claude-sonnet-4-6) 呼び出し → wiki content 生成
       - source_refs を validate
       - 成功: INSERT OR REPLACE INTO wikis (last_rebuilt_at = started_at)
       - 失敗: log + メトリクス、wiki 無変更（last_rebuilt_at も書かない）

    5. ループ先頭へ。次の iteration では新しい started_at で同じ手順を繰り返す。
       rebuild 中に created_at が起点 < x <= 前 iteration の started_at に挿入された
       raws はすべて拾われる。
}

// drain 完了後:
state map をロックして atomic に:
  if state[owner].manual_pending == true:
      state[owner].manual_pending = false
      // running は true のまま、ループ先頭から続行
      goto loop
  else:
      state[owner].running = false
      終了
```

### 7.1 manual `wiki_rebuild` ツールのセマンティクス

```
入力: concept? (省略時は全 concept)

state map をロックして atomic に判定:

CASE A: running == false
  → state[owner].running = true
  → worker を spawn
  → 戻り: { status: 'started' }

CASE B: running == true
  → state[owner].manual_pending = true
  → 戻り: { status: 'pending' }
     (現 rebuild 完了後、drain ループ終了時に追加 rebuild が走る)
```

manual 呼び出しが複数回 burst しても manual_pending は 1 bit なので最大 1 回しか trailing しない。

### 7.2 増分起点とリカバリ方針

- 増分起点は `MAX(wikis.last_rebuilt_at WHERE scope='personal' AND owner_id=?)` の **MAX**。
- 成功 concept の `last_rebuilt_at` は **rebuild iteration 開始時刻 (`started_at`)** で更新する（now() ではない）。これにより rebuild 中に追加された raws が次 iteration / 次回 rebuild で取りこぼされない。
- **失敗 concept は自動再試行しない**。具体的には:
  - 失敗 concept の `last_rebuilt_at` は更新されないが、MAX cursor では現れないため次回 cursor は前進する
  - 失敗 concept が再合成されるのは、(a) 別の raw が同じ concept を tag した、(b) Haiku 抽出が再びその concept を影響対象に出した、(c) ユーザが `wiki_rebuild` を明示呼び出しした、のいずれか
  - 観測メトリクス `concept_rebuild_failed_total` で運用者が異常を検知し、必要なら manual rebuild を促す

### 7.3 worker のクラッシュ・panic 対策（重要）

tokio task の panic では `running` フラグは自動で戻らない。worker spawn 時に必ず以下のいずれかでガードする:

```rust
// パターン 1: AssertUnwindSafe + catch_unwind (推奨)
tokio::spawn(async move {
    let result = std::panic::AssertUnwindSafe(rebuild_body(state.clone(), owner.clone()))
        .catch_unwind()
        .await;
    // 成功でも panic でも必ず実行
    coordinator.mark_idle(&owner);
    if let Err(panic) = result {
        tracing::error!(?owner, ?panic, "rebuild worker panicked");
        metrics::rebuild_failed_total.inc();
    }
});

// パターン 2: JoinHandle await + match
let handle = tokio::spawn(rebuild_body(state.clone(), owner.clone()));
tokio::spawn(async move {
    let result = handle.await;  // JoinError は panic も含む
    coordinator.mark_idle(&owner);
    if let Err(e) = result { /* log */ }
});
```

どちらでも良いが、Unit テストで「rebuild_body 内で panic させて running=false に戻ること」を検証する。

### 7.4 再起動時の挙動

- in-memory state は失われる
- 中断された rebuild の途中まで反映済みの wikis は残る
- 次の append が新規 rebuild を起こす → `MAX(wikis.last_rebuilt_at)` 起点で漏れた raws が拾われる → 自然に整合
- ただし「中断時点で全 concept 失敗だった rebuild」のあと、当該 owner に append が来ないと、未反映の raws が放置される（lazy drain の本質）。SLO 無しの前提下で許容

## 8. エラー処理

| 失敗 | 振る舞い |
|---|---|
| Haiku (claude-haiku-4-5) 概念抽出失敗 | rebuild abort、wikis は無変更、log + メトリクス、state.running=false に戻す |
| 個別 concept の Sonnet (claude-sonnet-4-6) 呼び出し失敗 | その concept のみ wiki 更新せず続行。log + メトリクス。`wikis.last_rebuilt_at` が進まないため次回自動再試行 |
| `source_refs` に実在しない raw id | その id を除外して保存 |
| worker タスクが panic / 中断 | spawn 側で `catch_unwind` または JoinHandle await により state.running=false を必ず復元（§7.3 参照）。次回 append で復活 |
| Sonnet レートリミット | exponential backoff + 最大 3 回リトライ、ダメならその concept だけ失敗扱い |
| 入力 validation 失敗（raw_append） | 400 で即返却、INSERT もしない |
| 認証失敗 | 401 |
| 認可失敗（他人の personal raw を read 等） | 404 として扱う（存在情報の漏洩を防ぐ） |
| レート制限超過 | 429 + `Retry-After` ヘッダ |
| `export()` レスポンスサイズ超過 | `cursor` を返却、クライアントが続きを要求 |

### 8.1 SLO とフレッシュネス

- **可用性 / レイテンシ SLO は提供しない**（MVP）。best effort。
- wiki のフレッシュネス: rebuild が走り終わるまでの間、最新 raw は wiki に未反映。staleness window は通常「次の append のあと数秒〜数十秒」、最大「ユーザが手動 `wiki_rebuild` を呼ぶまで無期限」。
- 反映タイミングが要件になる場合は `wiki_rebuild` を明示的に呼ぶ運用とする。

## 9. 認証 / 認可

MCP OAuth 2.1 サーバとして振る舞い、ID は Google に委譲する二段 OAuth。

```
[Claude.ai] ──(MCP OAuth 2.1)──> [Our Server] ──(OAuth)──> [Google]
```

実装する endpoint:
- `GET /.well-known/oauth-authorization-server` （RFC 8414）
- `POST /oauth/register` （RFC 7591 DCR）
- `GET /oauth/authorize` （Google にリダイレクト）
- `GET /oauth/callback/google` （Google から code 受領、`users` UPSERT）
- `POST /oauth/token` （こちらの access_token を発行）
- `POST /oauth/revoke`

トークン:
- access_token = HS256 JWT、TTL 1h、claims に `user_id`, `client_id`, `iat`, `exp`, `kid`
- refresh_token = 30 日 opaque、`tokens` テーブルに保存（`revoked_at` で失効可）
- PKCE 必須

#### 9.1 JWT 鍵ローテーション

- 署名鍵は Secret Manager に複数バージョン格納（`JWT_SIGNING_KEY_v1`, `v2` ...）
- 現行 active key の `kid` を JWT header に入れる
- 検証は「active + 1 つ前」の `kid` を受け入れる dual-key window（1h = access TTL の同等値）
- ローテーション手順: (1) 新 key を v(N+1) に追加 → (2) 1h 以上の dual-window → (3) v(N-1) を削除
- refresh_token は opaque なので鍵ローテに影響されない
- ユーザ強制ログアウト要件発生時は `tokens.revoked_at` を一括設定 + `kid` 一括強制ローテで両方を無効化

#### 9.2 DCR（動的クライアント登録）ポリシー

| 項目 | ポリシー |
|---|---|
| `redirect_uris` | https のみ受理（localhost と `127.0.0.1` 除く）。各 URI は事前正規化 |
| `redirect_uris` 数上限 | 1 client あたり 5 |
| `grant_types` | `authorization_code` と `refresh_token` のみ受理 |
| `token_endpoint_auth_method` | `none`（public client、PKCE 必須）または `client_secret_basic` |
| 登録レート制限 | IP あたり 10 registrations/day。IP の取り方は §9.3 |
| 登録 client 永続化 | `oauth_clients` テーブル（client_id, redirect_uris, created_at, last_seen_at） |
| revoke | `oauth_clients.revoked_at` を設定。発行済み JWT は TTL 1h で自然失効、refresh_token はテーブル側で revoke |

#### 9.3 クライアント IP の特定（レート制限の信頼境界）

`X-Forwarded-For` ヘッダは攻撃者が偽造できるため、無条件で信用してはならない。本サーバの方針:

- **GCP Cloud Load Balancer 経由でのみ公開する**前提とする（Compute Engine への直アクセスはファイアウォール `iap-forwarding` 等で遮断）
- `X-Forwarded-For` のうち、**末尾から信頼プロキシ数（=1）を引いた位置の IP** をクライアント IP とする。GCP LB が末尾に LB 自身の IP を付与する形式なので、末尾から 2 番目を取る
- ヘッダが無い / フォーマット異常の場合は TCP の peer address にフォールバック（直接到達を遮断していれば原則ここに来ない）
- アプリ起動時に「信頼プロキシ数」を環境変数 `TRUSTED_PROXY_COUNT` (default=1) で設定可能にして、LB 構成変更にも追従できるようにする

使用 crate: `oauth2`, `jsonwebtoken`, `axum`, `axum-extra`。

LLM クライアント:
- 概念抽出: `claude-haiku-4-5-20251001`
- wiki 合成: `claude-sonnet-4-6`
- いずれも prompt caching を有効化（システムプロンプト + 既存 wikis を cache 対象に）
- モデル ID は環境変数 `MODEL_HAIKU` / `MODEL_SONNET` で上書き可能

認可ルール:

| ツール種別 | 認可 |
|---|---|
| read 系 (personal) | `raws.owner_id == authenticated user_id` |
| read 系 (shared) | login されていれば誰でも |
| write 系 (personal) | `authenticated user_id` を `owner_id` に固定 |
| write 系 (shared) | 存在しない |

## 10. shared scope の運用契約

- MCP は read のみ。書き込みは存在しない。
- raws / wikis / schemas (`scope='shared'`) は **サーバの責務外** で SQLite に投入される。
- 投入手段は MVP の外（直接 SQL、litestream restore、別 Rust CLI、将来 Vegapunk export など、運用者の選択）。
- サーバはこれらを読むだけで、LLM rebuild を一切走らせない。
- `last_rebuilt_at` は shared では「外部投入時のタイムスタンプ」と読み替える。

## 11. Phase 2（Vegapunk）への接続点

差し替え対象は **rebuild worker の中身のみ**。

| Phase 1 | Phase 2 |
|---|---|
| raws テーブル | そのまま Vegapunk への ingest 元 |
| wikis テーブル | Vegapunk からの compile 結果キャッシュ |
| MCP ツール定義 | 変更なし |
| rebuild worker の Sonnet 呼び出し | Vegapunk API 呼び出しに置換 |
| FTS5 top-k 抽出 | Vegapunk のグラフ近傍 / RAG に置換 |
| shared への ingest | Vegapunk が直接担当する形に拡張可能 |

別リポジトリに分ける場合は、Phase 1 を「SQLite + MCP I/F のみ」に絞り、Vegapunk クライアントを feature flag で差し込める構造にする。

## 12. デプロイ

| 項目 | 値 |
|---|---|
| プラットフォーム | GCP Compute Engine VM |
| インスタンス | e2-micro（無料枠が使えるならこちら）または e2-small |
| OS | Container-Optimized OS または Ubuntu LTS |
| ランタイム | Docker Compose（MCP サーバ + litestream sidecar） |
| ストレージ | Persistent Disk (pd-balanced, 20 GB) に `/var/data/db.sqlite` |
| バックアップ | litestream → GCS バケット（WAL レプリケーション） |
| シークレット | Google Secret Manager: `ANTHROPIC_API_KEY`, `GOOGLE_OAUTH_CLIENT_ID`, `GOOGLE_OAUTH_CLIENT_SECRET`, `JWT_SIGNING_KEY` |
| HTTPS | Cloud Load Balancer + マネージド証明書、または Caddy リバプロでサーバ側終端 |
| ロギング | Cloud Logging（journald → fluentd-gcp） |
| メトリクス | Cloud Monitoring（ops-agent） |

### 12.1 観測すべきメトリクスとアラート閾値（初期値）

| メトリクス | 推奨アラート閾値 |
|---|---|
| `rebuild_in_flight_gauge` (in-memory state の running 数) | > 50（active owners 数の異常上昇） |
| `rebuild_duration_seconds` p99 | > 300（5 分）|
| `rebuild_failed_total` 増分 | 5 分で > 5 |
| `concept_rebuild_failed_total` 増分（concept 単位の失敗） | 5 分で > 20 |
| `anthropic_api_error_total` 増分 | 1 分で > 10 |
| `oauth_login_failure_total` 増分 | 1 分で > 20（攻撃兆候） |
| `dcr_registration_total` per IP | 1 日で > 10（rate limit ヒット） |
| `sqlite_db_size_bytes` | PD 容量の 70% |
| `http_5xx_total` 増分 | 1 分で > 5 |

### 12.2 データライフサイクル

- **アカウント削除**: `users.id` 指定で以下を一括削除（CASCADE 相当をアプリで実行）:
  - `raws WHERE scope='personal' AND owner_id=?`
  - `wikis WHERE scope='personal' AND owner_id=?`
  - `schemas WHERE scope='personal' AND owner_id=?`
  - `tokens WHERE user_id=?`
  - 最後に `users WHERE id=?`
- **データ保持**: ユーザ自身が削除するまで無期限保持。rebuild は in-memory state のみで履歴なし、reaper も不要。
- **export 後の自動削除**: なし。ユーザが明示的に削除リクエストを出した場合のみ実施。

スケール上限：SQLite シングルライタ前提で「数百ユーザ / 数十 GB」が現実的天井。それを超えたら Phase 2 移行の判断ポイント。

## 13. テスト戦略

**Unit テスト**
- 認可ロジック（scope × owner × user_id 全組み合わせ）
- rebuild 入力構築（FTS top-k ∪ source_refs ∪ 新規 raws、上限 50）
- `source_refs` validation
- `RebuildCoordinator` の state 遷移（running / manual_pending の各組み合わせで二重起動しないこと）
- worker の **drain loop**: rebuild 中に新規 raw が挿入されたとき、追加 iteration で取り込むこと
- worker の `last_rebuilt_at = started_at` 規律（now() を書かないこと）
- worker panic 時に `state.running` が必ず false に戻ること（rebuild_body 内で意図的に panic させる）
- ULID 形式 / `shared_memories.id` の正規表現バリデーション
- `X-Forwarded-For` パースの境界（信頼プロキシ数 N で末尾から N+1 番目を取る、ヘッダ偽造を弾く）

**Integration テスト**
- SQLite in-memory で `raw_append → worker → wiki_read` を end-to-end
- LLM クライアントは trait 化し mock 差し込み
- OAuth フロー: `axum::test::TestServer` + mock Google authorize endpoint

**手動 E2E**
- Claude Desktop / Claude.ai に接続し、`/.well-known/oauth-authorization-server` 取得 → DCR → authorize → tool 呼び出しが通ることを確認

**TDD 規律**
- 全 MCP ツールは「失敗テスト → 実装」順。実装フェーズでは `superpowers:test-driven-development` を起動。

## 14. デフォルト前提（レビューで上書き可）

- raws の delete / update API: 載せない（直接 SQL 推奨）
- schema validation / lint: 載せない（保存のみ）
- export 形式: JSON 1 ファイル（personal の raws + wikis + schema）
- Apple Sign In / Magic Link: MVP 外、将来拡張
- 概念粒度の暴走対策:
  - 1 owner あたり concept 数の上限を **200** とする（運用値、env で上書き可）
  - 上限到達時、Haiku は「新規 concept を作らず既存に統合する」プロンプトに切り替え
  - 概念名の最小長 2 文字 / 最大長 64 文字、英数日本語のみ（記号は escape）
- モデル設定:
  - 環境変数 `MODEL_HAIKU` / `MODEL_SONNET` で差し替え可（default: `claude-haiku-4-5-20251001` / `claude-sonnet-4-6`）
  - `ANTHROPIC_API_KEY` は Secret Manager 経由
- ID 規約: ULID（アプリ生成、Crockford Base32 26 文字）を全主キーに適用。ただし `shared_memories.id` のみ人間可読 ID を許容（§4.1 例外）

## 15. リポジトリ構成（想定）

```
crates/
  llm-memory-server/        # axum サーバ + OAuth + MCP ハンドラ
  llm-memory-core/          # ドメインロジック (auth/authorization, rebuild input 構築 等)
  llm-memory-storage/       # SQLite アクセス層 (sqlx)
  llm-memory-llm/           # Anthropic クライアント trait + 実装
  llm-memory-worker/        # rebuild worker (バイナリは server と同居でも別でも可)
migrations/                 # sqlx-migrate
docker/
  Dockerfile
  docker-compose.yml        # litestream sidecar 含む
docs/superpowers/specs/     # 本ファイルなど
.github/workflows/          # CI（github-actions-optimize スキル準拠）
```
