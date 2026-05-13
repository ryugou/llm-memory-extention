use std::net::IpAddr;

/// Parse X-Forwarded-For with a trust boundary.
///
/// XFF is comma-separated `client, proxy1, proxy2, ...`. With N trusted proxies
/// at the tail, the client IP is the (N+1)-th from the end. Anything beyond is
/// untrusted and we fall back to the peer address.
///
/// MVP assumes deployment behind GCP Cloud Load Balancer (`TRUSTED_PROXY_COUNT=1`).
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

    fn peer() -> IpAddr { IpAddr::from_str("10.0.0.1").unwrap() }

    #[test]
    fn xff_one_trusted_proxy() {
        let ip = parse_client_ip(Some("203.0.113.10, 35.244.0.1"), peer(), 1);
        assert_eq!(ip.to_string(), "203.0.113.10");
    }

    #[test]
    fn xff_two_trusted_proxies() {
        let ip = parse_client_ip(Some("203.0.113.10, 198.51.100.1, 35.244.0.1"), peer(), 2);
        assert_eq!(ip.to_string(), "203.0.113.10");
    }

    #[test]
    fn xff_too_short_falls_back_to_peer() {
        let ip = parse_client_ip(Some("203.0.113.10"), peer(), 1);
        assert_eq!(ip.to_string(), "10.0.0.1");
    }

    #[test]
    fn no_xff_uses_peer() {
        let ip = parse_client_ip(None, peer(), 1);
        assert_eq!(ip.to_string(), "10.0.0.1");
    }

    #[test]
    fn malformed_xff_entry_falls_back_to_peer() {
        let ip = parse_client_ip(Some("not-an-ip, 35.244.0.1"), peer(), 1);
        // The "client" slot at index 0 fails to parse, so we fall back to peer.
        assert_eq!(ip.to_string(), "10.0.0.1");
    }

    #[test]
    fn empty_xff_uses_peer() {
        let ip = parse_client_ip(Some(""), peer(), 1);
        assert_eq!(ip.to_string(), "10.0.0.1");
    }
}
