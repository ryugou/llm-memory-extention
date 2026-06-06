//! Per-schema ingest 直列化 (= mutex pool)。
//!
//! **Scope 制限**: 本実装は **単一 process (= 単一 wrapper replica)** 前提で
//! 動く in-process mutex pool。複数 replica で wrapper を horizontal scale-out
//! した場合、replica を跨いだ並列 ingest は直列化されない (= 各 replica が
//! 別個の HashMap を持つため)。Phase 1 の運用形態 (= 1 VM 1 replica) 前提で
//! 設計しており、scale-out が必要になったら distributed lock (Redis 等) か
//! sticky routing で同 schema を同 replica に固定する設計が必要になる。
//!
//! PR #21 の dedup pre-check は `collect_dedup_catalogue` で
//! `query_nodes` を撃ち、`{user.vegapunk_schema}:` prefix の entity 一覧を
//! 取得する。同じ schema へ並列に `ingest` / `ingest_raw` が来ると、両者が
//! それぞれ古い catalogue を取って自分の rewrite を進めてしまい、互いに
//! 相手の entity を見えないまま vegapunk へ流す = dedup が race で空振り。
//! PR #24 の sync-wait は **直前** の ingest の extraction 完了を待つが、
//! 並列に進む 2 つを互いに待たせる仕組みは持っていない。
//!
//! 本モジュールは `vegapunk_schema` をキーにした **per-schema mutex** を
//! 提供する。`ingest` / `ingest_raw` handler は処理開始時に該当 schema の
//! lock を取り、関数を抜けるまで保持する。これにより 1 schema に対する
//! ingest が wrapper 単位で逐次化される。
//!
//! cross-schema (= 異なる user) は別 lock なので並列実行可能。
//! shared_schema_name は personal とは別 entry を持ち、書き込み禁止の
//! ガードは PR #21/handlers 側で行うため本モジュールは関知しない。
//!
//! 設計メモ:
//! - 外側 `tokio::sync::Mutex<HashMap<...>>` は HashMap の get-or-insert
//!   の間だけ保持し、すぐに drop する。per-schema lock は `Arc::clone` で
//!   呼び出し側へ渡し、その上で `lock().await` する。
//! - HashMap は schema 名で grow し続けるが、tenant 数は実運用で限定的
//!   (= 自社員数規模) なので GC 戦略は持たない。将来必要になったら LRU
//!   に置き換える。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

/// `IngestSerializer` は `vegapunk_schema` → `Arc<Mutex<()>>` の map を
/// 保持する。`lock_for(schema)` で呼び出し側に lock 本体を返し、caller が
/// `lock().await` でガードを取る形にする。
#[derive(Debug, Default)]
pub struct IngestSerializer {
    schemas: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl IngestSerializer {
    pub fn new() -> Self {
        Self::default()
    }

    /// `schema` 用の per-schema lock を取り出す。初回呼び出しなら HashMap
    /// に新規エントリを作る。返した `Arc<Mutex<()>>` は caller が
    /// `.lock().await` で guard を得て、ingest 処理が終わるまで保持する。
    pub async fn lock_for(&self, schema: &str) -> Arc<Mutex<()>> {
        let mut map = self.schemas.lock().await;
        if let Some(existing) = map.get(schema) {
            return existing.clone();
        }
        let fresh = Arc::new(Mutex::new(()));
        map.insert(schema.to_string(), fresh.clone());
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn same_schema_serializes_concurrent_callers() {
        // 同じ schema に対して 2 つの task が `lock_for("alice")` を取り、
        // 各 task が critical-section 内で 50ms sleep する。直列化保証は
        // **critical-section 内の同時在席数の peak が 1** であることで
        // 確認する (= wall-clock time 比較は CI の jitter で flaky になる
        // ため、in-section カウンタの peak 観測に振っている)。
        //
        // 2 つの task の lock 取得を **タイミング sleep に頼らず** 同時に
        // 競争させるため `tokio::sync::Barrier` を使う (= Copilot review:
        // 5ms sleep だと spawn 直後の race が決定的でなく、片方が先に取り
        // 終えてから片方が始まる "事実上 sequential" になり得て検知力が
        // 弱まる)。両 task は barrier wait を抜けた瞬間に同時に lock を狙う。
        use tokio::sync::Barrier;

        let ser = Arc::new(IngestSerializer::new());
        let in_section = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));

        let lock_a = ser.lock_for("alice").await;
        let lock_b = ser.lock_for("alice").await;
        assert!(Arc::ptr_eq(&lock_a, &lock_b));

        let spawn_critical = |lock: Arc<Mutex<()>>, barrier: Arc<Barrier>| {
            let inside = in_section.clone();
            let peak = max_seen.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                let _g = lock.lock().await;
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                inside.fetch_sub(1, Ordering::SeqCst);
            })
        };

        let h1 = spawn_critical(lock_a.clone(), barrier.clone());
        let h2 = spawn_critical(lock_b.clone(), barrier.clone());
        let (r1, r2) = tokio::join!(h1, h2);
        r1.unwrap();
        r2.unwrap();

        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "concurrent ingest with same schema must serialize (peak in-section count must be 1)"
        );
    }

    #[tokio::test]
    async fn distinct_schemas_get_distinct_locks() {
        let ser = IngestSerializer::new();
        let lock_alice = ser.lock_for("alice").await;
        let lock_bob = ser.lock_for("bob").await;
        assert!(!Arc::ptr_eq(&lock_alice, &lock_bob));

        // 別 schema の lock は同時に取れる (= 並列性が保たれる)。
        let _g1 = lock_alice.lock().await;
        let _g2 = lock_bob.lock().await;
    }

    #[tokio::test]
    async fn reentrant_get_returns_same_lock() {
        // 同じ schema を 100 回 lock_for しても同一の Arc を返すこと
        // (= 同名 schema の serializer が hash key で同定される)。
        let ser = IngestSerializer::new();
        let first = ser.lock_for("alice").await;
        for _ in 0..100 {
            let again = ser.lock_for("alice").await;
            assert!(Arc::ptr_eq(&first, &again));
        }
    }
}
