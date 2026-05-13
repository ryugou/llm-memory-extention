use prometheus::{Encoder, Histogram, HistogramOpts, IntCounter, IntGauge, Registry, TextEncoder};

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
        let rebuild_duration = Histogram::with_opts(
            HistogramOpts::new("rebuild_duration_seconds", "rebuild iteration duration")
                .buckets(vec![0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0]),
        ).unwrap();
        let rebuild_failed = IntCounter::new("rebuild_failed_total", "rebuild failures").unwrap();
        let concept_rebuild_failed = IntCounter::new("concept_rebuild_failed_total", "per-concept failures").unwrap();
        let rebuild_drain_iterations = Histogram::with_opts(
            HistogramOpts::new("rebuild_drain_iterations", "drain loop iterations")
                .buckets(vec![1.0, 2.0, 3.0, 5.0, 10.0]),
        ).unwrap();
        let rebuild_drain_capped = IntCounter::new("rebuild_drain_capped_total", "drain MAX_ITERATIONS hits").unwrap();
        let anthropic_api_error = IntCounter::new("anthropic_api_error_total", "anthropic api errors").unwrap();
        let oauth_login_failure = IntCounter::new("oauth_login_failure_total", "oauth login failures").unwrap();
        let dcr_registration = IntCounter::new("dcr_registration_total", "dcr registrations").unwrap();
        let sqlite_db_size_bytes = IntGauge::new("sqlite_db_size_bytes", "db file size").unwrap();
        let http_5xx = IntCounter::new("http_5xx_total", "5xx responses").unwrap();

        for c in [&rebuild_failed, &concept_rebuild_failed, &rebuild_drain_capped,
                  &anthropic_api_error, &oauth_login_failure, &dcr_registration, &http_5xx] {
            registry.register(Box::new((*c).clone())).unwrap();
        }
        registry.register(Box::new(rebuild_in_flight.clone())).unwrap();
        registry.register(Box::new(rebuild_duration.clone())).unwrap();
        registry.register(Box::new(rebuild_drain_iterations.clone())).unwrap();
        registry.register(Box::new(sqlite_db_size_bytes.clone())).unwrap();

        Self {
            registry, rebuild_in_flight, rebuild_duration, rebuild_failed,
            concept_rebuild_failed, rebuild_drain_iterations, rebuild_drain_capped,
            anthropic_api_error, oauth_login_failure, dcr_registration,
            sqlite_db_size_bytes, http_5xx,
        }
    }

    pub fn encode_text(&self) -> Vec<u8> {
        let encoder = TextEncoder::new();
        let mut buf = Vec::new();
        encoder.encode(&self.registry.gather(), &mut buf).unwrap();
        buf
    }
}

impl Default for Metrics {
    fn default() -> Self { Self::new() }
}

pub async fn handler(axum::extract::State(state): axum::extract::State<crate::app::AppState>) -> impl axum::response::IntoResponse {
    let body = state.metrics.encode_text();
    ([("content-type", "text/plain; version=0.0.4")], body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_encode_returns_text() {
        let m = Metrics::new();
        m.rebuild_failed.inc();
        let text = String::from_utf8(m.encode_text()).unwrap();
        assert!(text.contains("rebuild_failed_total"));
    }

    #[test]
    fn metrics_clone_shares_registry() {
        let m1 = Metrics::new();
        let m2 = m1.clone();
        m1.rebuild_failed.inc();
        // Cloning Metrics also clones the underlying Arc-backed counters, so m2 sees the same value.
        assert_eq!(m1.rebuild_failed.get(), m2.rebuild_failed.get());
    }
}
