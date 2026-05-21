//! Worker から外部メトリクス層へ統計を送るための抽象。
//! 実装は server crate 側 (`Metrics` への impl) で提供する。
//! coordinator crate は server crate に依存できない (循環依存) ため、
//! trait オブジェクト経由で配線する。

/// 運用者が異常検知に用いる counter / gauge を増減させる。
pub trait MetricsSink: Send + Sync {
    /// 1 セッション全体が失敗 (もしくは worker が panic) したとき。
    fn inc_rebuild_failed(&self);
    /// 個別 concept の synthesize が失敗したとき。
    fn inc_concept_rebuild_failed(&self);
    /// drain ループが `MAX_ITERATIONS` に達したとき。
    fn inc_rebuild_drain_capped(&self);
    /// 1 セッションで実行された drain iteration 数。
    fn observe_drain_iterations(&self, n: u64);
    /// worker タスク開始時に +1。
    fn rebuild_in_flight_inc(&self);
    /// worker タスク終了時に -1。
    fn rebuild_in_flight_dec(&self);
    /// LLM provider 関連の失敗 (`LlmError::Api`) が起きたときに +1。
    /// 含まれるもの: HTTP non-2xx (5xx / quota / 認可エラー) と、ADC token
    /// 初期化・取得失敗 (status=503 で wrap)。
    /// 含まれないもの: safety filter / response parse 異常 (`LlmError::Parse`)、
    /// reqwest 通信エラー (`LlmError::Reqwest`)、JSON 異常 (`LlmError::Json`)。
    fn inc_llm_api_error(&self);
}

/// テスト/メトリクス無効時用の no-op 実装。
pub struct NoopMetricsSink;

impl MetricsSink for NoopMetricsSink {
    fn inc_rebuild_failed(&self) {}
    fn inc_concept_rebuild_failed(&self) {}
    fn inc_rebuild_drain_capped(&self) {}
    fn observe_drain_iterations(&self, _n: u64) {}
    fn rebuild_in_flight_inc(&self) {}
    fn rebuild_in_flight_dec(&self) {}
    fn inc_llm_api_error(&self) {}
}
