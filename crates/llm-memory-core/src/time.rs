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
