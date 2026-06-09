-- tool_ownership: vegapunk が返す search_id / msg_id を **どの user が生成
-- したか** を記録する。`feedback` と `get_job_status` の handler は受け取った
-- id を本テーブルで照合し、user_id が一致しなければ 403 を返す。これで
-- 他 tenant の id を試行で叩けないようにする (= cross-tenant guard、PR #15
-- 以降の schema 強制注入では捕まらない経路を塞ぐ)。
--
-- 設計上の注意:
-- - `kind` で名前空間を分ける ('search' = SearchResponse.search_id、'msg' =
--   IngestRawResponse.msg_ids[i])。foreign_id の衝突 (= search/msg で同じ
--   string) を避けるため (kind, foreign_id) を複合 PK にする。
-- - `user_id` は users.id FK で NOT NULL。`PRAGMA foreign_keys=ON` 環境
--   では、ownership 行が残っている user を DELETE しようとすると FK
--   violation で失敗する。現状 user 削除フロー自体存在しないため
--   ON DELETE 動作は未指定 (= 削除を試みた時に明示的に拒否される、
--   safe-default)。将来 user 削除を実装する際は本 FK の cascade ポリシー
--   (= CASCADE / SET NULL / 明示的な ownership 行先削除) をその PR で決める。
-- - 個別の id 単発の SELECT が主用途なので、PK 経由の lookup で十分。
--   user 単位の管理画面 (= 「自分の search 履歴」など) 用に user_id 単独
--   index も足しておく。
CREATE TABLE tool_ownership (
  kind TEXT NOT NULL,
  foreign_id TEXT NOT NULL,
  user_id TEXT NOT NULL REFERENCES users(id),
  created_at INTEGER NOT NULL,
  PRIMARY KEY (kind, foreign_id)
);

CREATE INDEX idx_tool_ownership_user ON tool_ownership (user_id);
