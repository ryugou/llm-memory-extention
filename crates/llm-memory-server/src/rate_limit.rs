use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct Tier {
    pub name: &'static str,
    pub per_minute: u32,
}

pub const READ_TIER: Tier = Tier { name: "read", per_minute: 600 };
pub const WRITE_TIER: Tier = Tier { name: "write", per_minute: 60 };
pub const HEAVY_TIER: Tier = Tier { name: "heavy", per_minute: 6 };

/// Choose the tier based on the tool name.
pub fn tier_of(tool: &str) -> Tier {
    match tool {
        "raw_append" => WRITE_TIER,
        "schema_update" => WRITE_TIER,
        "wiki_rebuild" => HEAVY_TIER,
        "export" => HEAVY_TIER,
        _ => READ_TIER,
    }
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

pub struct RateLimiter {
    #[allow(dead_code)]
    capacities: HashMap<(String, &'static str), Mutex<Bucket>>,
    overall: Mutex<HashMap<(String, &'static str), Bucket>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            capacities: HashMap::new(),
            overall: Mutex::new(HashMap::new()),
        }
    }

    /// Returns true if the request is allowed (consumes 1 token).
    /// Returns false if the user has exceeded the tier's per-minute budget.
    pub fn check(&self, user_id: &str, tier: Tier) -> bool {
        let mut buckets = self.overall.lock().unwrap();
        let key = (user_id.to_string(), tier.name);
        let now = Instant::now();
        let entry = buckets.entry(key).or_insert(Bucket {
            tokens: tier.per_minute as f64,
            last_refill: now,
        });

        // Refill at per_minute / 60 tokens per second.
        let elapsed = now.duration_since(entry.last_refill);
        let refill = elapsed.as_secs_f64() * (tier.per_minute as f64 / 60.0);
        entry.tokens = (entry.tokens + refill).min(tier.per_minute as f64);
        entry.last_refill = now;

        if entry.tokens >= 1.0 {
            entry.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

impl Default for RateLimiter {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn tier_assignment() {
        assert_eq!(tier_of("raw_append").name, "write");
        assert_eq!(tier_of("schema_update").name, "write");
        assert_eq!(tier_of("wiki_rebuild").name, "heavy");
        assert_eq!(tier_of("export").name, "heavy");
        assert_eq!(tier_of("raw_search").name, "read");
        assert_eq!(tier_of("wiki_read").name, "read");
        assert_eq!(tier_of("unknown").name, "read");
    }

    #[test]
    fn heavy_tier_throttles_after_limit() {
        let rl = RateLimiter::new();
        for _ in 0..6 {
            assert!(rl.check("u1", HEAVY_TIER), "first 6 should pass");
        }
        assert!(!rl.check("u1", HEAVY_TIER), "7th should be throttled");
    }

    #[test]
    fn refills_over_time() {
        let rl = RateLimiter::new();
        // Drain heavy tier completely.
        for _ in 0..6 { rl.check("u-r", HEAVY_TIER); }
        assert!(!rl.check("u-r", HEAVY_TIER));
        // Wait for a refill (>10s gives ~1 token at 6/min).
        sleep(Duration::from_millis(11_000));
        assert!(rl.check("u-r", HEAVY_TIER), "should have refilled at least 1 token");
    }

    #[test]
    fn different_users_isolated() {
        let rl = RateLimiter::new();
        for _ in 0..6 { rl.check("u1", HEAVY_TIER); }
        assert!(!rl.check("u1", HEAVY_TIER));
        assert!(rl.check("u2", HEAVY_TIER), "u2 should be unaffected");
    }
}
