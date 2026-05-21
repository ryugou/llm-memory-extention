# Phase 1 手動 E2E チェックリスト

デプロイ後に以下を全て確認してから本番リリースする。

## 1. デプロイ直後のスモークテスト

- [ ] `curl https://<host>/healthz` → `ok`
- [ ] `curl https://<host>/.well-known/oauth-authorization-server` → JSON (issuer / endpoints)
- [ ] `curl https://<host>/metrics` → Prometheus テキスト (`rebuild_failed_total` 等が含まれる)
- [ ] サーバーログに ERROR / FATAL がない

## 2. OAuth フロー（Claude Desktop or curl）

### Claude Desktop の場合
- [ ] Claude Desktop の MCP 設定で `https://<host>` を追加
- [ ] ブラウザで Google 認証画面が出る
- [ ] 認可後、Claude Desktop に戻り「Connected」表示
- [ ] `tools/list` 相当が表示される

### curl の場合
- [ ] DCR で `client_id` を取得:
      ```bash
      curl -X POST https://<host>/oauth/register \
        -H 'content-type: application/json' \
        -d '{"redirect_uris":["https://example.com/cb"]}'
      ```
- [ ] `client_id` が ULID (26 文字) で返る

## 3. MCP ツール

認可済みのアクセストークン `$TOKEN` を取得した状態で:

- [ ] `raw_append` で raw を 1 件追加 → `{"raw_id":"01H...","rebuild_started":true}` が返る
- [ ] 10〜30 秒待ってから `wiki_list` を呼ぶ → 新しい concept が増える
- [ ] `wiki_read({concept: "<新規 concept>"})` → personal の content が取得できる
- [ ] `raw_search({query:"<キーワード>"})` → ヒットする
- [ ] `wiki_rebuild()` → `{"status":"started" | "pending"}`
- [ ] `schema_update({content:"...."})` → `{"ok":true}`
- [ ] `schema_read({scope:"personal"})` → 直前にセットした content が取れる
- [ ] `export()` → `raws` / `wikis` / `schema` を含む JSON
- [ ] `export({cursor:"<created_at>"})` → ページング動作確認 (raws のみ、wikis/schema は null)

## 4. アカウント削除

- [ ] `DELETE /v1/account` (Bearer 付き) → 204
- [ ] 直後の `wiki_list` → personal の concepts が空
- [ ] `raw_search` → personal の hits が空

## 5. 共有メモリ（外部投入経由）

- [ ] サーバの SQLite に `sqlite3` で直接 shared_memories と raws (scope='shared') を投入
- [ ] `raw_search({scope:"shared"})` → ヒット
- [ ] `wiki_read({scope:"all"})` → shared 配列に該当 wiki が入る
- [ ] 別ユーザでも同じ shared が読める

## 6. レート制限

- [ ] `wiki_rebuild` を 7 回連続で叩く → 7 回目以降 `rate_limited: heavy tier` エラー
- [ ] `raw_search` を 700 回連続で叩く → 600 回前後でレート制限発動

## 7. 障害確認

- [ ] Vertex AI ADC を一時的に無効化 (VM の SA から `roles/aiplatform.user` を外す) → `raw_append` 後 rebuild が失敗。Haiku 抽出が 401/403 で run_session が早期 Err になるので `rebuild_failed_total` と `llm_api_error_total` が増加 (concept 単位の `concept_rebuild_failed_total` は extract で session 全体が失敗するため増えない)
- [ ] サーバ再起動 → 中断ジョブが次の append で再開
- [ ] DB ファイル肥大 → `sqlite_db_size_bytes` メトリクスで観測

## 8. バックアップ

- [ ] litestream のログに replicate 成功メッセージ
- [ ] GCS バケットに `db.sqlite` 関連オブジェクトが存在
- [ ] `restore -if-replica-exists` で復元できる

---

すべてチェック後、PR をマージ可。
