//! Which address a request came from.
//!
//! Used only by the login and setup lockout. Ingest never reads, logs or
//! stores a client address: an error report is about the app, not about
//! who sent it.
//!
//! Three policies:
//!
//! * `Peer` (default) — the TCP peer. Right when nothing sits in front.
//! * `ForwardedFor` — the first `X-Forwarded-For` hop, for a reverse proxy
//!   that strips and re-emits that header (`ERREX_TRUST_PROXY_HEADERS`).
//! * `Cloudflare` — `CF-Connecting-IP`, believed only when the TCP peer is
//!   in the trusted set: Cloudflare's published ranges, plus the local
//!   reverse proxy when one sits between Cloudflare and the daemon (and
//!   itself admits only Cloudflare). From any other peer the header is
//!   ignored, so a client that reaches the daemon directly cannot choose
//!   its own lockout bucket.

use std::net::{IpAddr, SocketAddr};

use axum::http::HeaderMap;

/// <https://www.cloudflare.com/ips-v4> and <https://www.cloudflare.com/ips-v6>.
/// `deploy/wp-vps/cloudflare-ips.sh` refreshes the nginx copy from the same
/// URLs; when Cloudflare publishes a change, update this list too.
pub const CLOUDFLARE_RANGES: &[&str] = &[
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
    "2400:cb00::/32",
    "2606:4700::/32",
    "2803:f800::/32",
    "2405:b500::/32",
    "2405:8100::/32",
    "2a06:98c0::/29",
    "2c0f:f248::/32",
];

/// An address range, `addr/prefix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    net: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let net: IpAddr = addr.parse().map_err(|_| format!("bad address in {s:?}"))?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .parse::<u8>()
                .map_err(|_| format!("bad prefix in {s:?}"))?,
            None => max,
        };
        if prefix > max {
            return Err(format!("prefix too long in {s:?}"));
        }
        Ok(Self { net, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.net, canonical(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                prefix_eq(&net.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                prefix_eq(&net.octets(), &ip.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

/// An IPv4 peer on a dual-stack socket arrives as `::ffff:a.b.c.d`.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

fn prefix_eq(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let full = usize::from(prefix / 8);
    if a[..full] != b[..full] {
        return false;
    }
    let rest = prefix % 8;
    if rest == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rest);
    (a[full] & mask) == (b[full] & mask)
}

/// Parse a trusted-peer list. The word `cloudflare` stands for
/// [`CLOUDFLARE_RANGES`].
pub fn parse_trusted(list: &[String]) -> Result<Vec<Cidr>, String> {
    let mut out = Vec::new();
    for item in list.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if item.eq_ignore_ascii_case("cloudflare") {
            for r in CLOUDFLARE_RANGES {
                out.push(Cidr::parse(r)?);
            }
        } else {
            out.push(Cidr::parse(item)?);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientIpPolicy {
    Peer,
    ForwardedFor,
    Cloudflare { trusted: Vec<Cidr> },
}

impl ClientIpPolicy {
    /// The client address under this policy, or `"unknown"` without a peer.
    pub fn resolve(&self, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
        match self {
            ClientIpPolicy::Peer => {}
            ClientIpPolicy::ForwardedFor => {
                let first = headers
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|xff| xff.split(',').next())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                if let Some(first) = first {
                    return first.to_string();
                }
            }
            ClientIpPolicy::Cloudflare { trusted } => {
                let from_edge = peer.is_some_and(|p| trusted.iter().any(|c| c.contains(p.ip())));
                let claimed = headers
                    .get("cf-connecting-ip")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<IpAddr>().ok());
                if let (true, Some(ip)) = (from_edge, claimed) {
                    return canonical(ip).to_string();
                }
            }
        }
        peer.map(|s| canonical(s.ip()).to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn cf_policy(extra: &[&str]) -> ClientIpPolicy {
        let mut list = vec!["cloudflare".to_string()];
        list.extend(extra.iter().map(|s| s.to_string()));
        ClientIpPolicy::Cloudflare {
            trusted: parse_trusted(&list).unwrap(),
        }
    }

    fn cf_headers(ip: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("cf-connecting-ip", HeaderValue::from_static(ip));
        h
    }

    // ----- Cidr -----

    #[test]
    fn cidr_matches_inside_and_rejects_outside() {
        let c = Cidr::parse("2a06:98c0::/29").unwrap();
        assert!(c.contains("2a06:98c7:ffff::1".parse().unwrap()));
        assert!(!c.contains("2a06:98c8::1".parse().unwrap()));
        let v4 = Cidr::parse("162.158.0.0/15").unwrap();
        assert!(v4.contains("162.159.255.255".parse().unwrap()));
        assert!(!v4.contains("162.160.0.0".parse().unwrap()));
    }

    #[test]
    fn cidr_matches_a_mapped_ipv4_peer() {
        let c = Cidr::parse("173.245.48.0/20").unwrap();
        assert!(c.contains("::ffff:173.245.48.1".parse().unwrap()));
    }

    #[test]
    fn cidr_without_prefix_is_a_single_host() {
        let c = Cidr::parse("::1").unwrap();
        assert!(c.contains("::1".parse().unwrap()));
        assert!(!c.contains("::2".parse().unwrap()));
    }

    #[test]
    fn cidr_rejects_garbage() {
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("nope/8").is_err());
        assert!(Cidr::parse("10.0.0.0/x").is_err());
    }

    #[test]
    fn cloudflare_keyword_expands_to_every_published_range() {
        let list = parse_trusted(&["cloudflare".into()]).unwrap();
        assert_eq!(list.len(), CLOUDFLARE_RANGES.len());
    }

    // ----- Cloudflare policy -----

    #[test]
    fn cloudflare_header_is_believed_from_a_cloudflare_peer() {
        let peer: SocketAddr = "[2606:4700::1]:443".parse().unwrap();
        let ip = cf_policy(&[]).resolve(&cf_headers("198.51.100.7"), Some(peer));
        assert_eq!(ip, "198.51.100.7");
    }

    #[test]
    fn cloudflare_header_is_ignored_from_any_other_peer() {
        let peer: SocketAddr = "[2001:db8::5]:4000".parse().unwrap();
        let ip = cf_policy(&[]).resolve(&cf_headers("198.51.100.7"), Some(peer));
        assert_eq!(ip, "2001:db8::5");
    }

    #[test]
    fn a_trusted_local_proxy_passes_the_header_through() {
        let peer: SocketAddr = "[::1]:50000".parse().unwrap();
        let ip = cf_policy(&["::1/128"]).resolve(&cf_headers("198.51.100.7"), Some(peer));
        assert_eq!(ip, "198.51.100.7");
        let untrusted = cf_policy(&[]).resolve(&cf_headers("198.51.100.7"), Some(peer));
        assert_eq!(untrusted, "::1");
    }

    #[test]
    fn cloudflare_policy_ignores_x_forwarded_for_and_junk_headers() {
        let peer: SocketAddr = "[2606:4700::1]:443".parse().unwrap();
        let mut h = cf_headers("not-an-ip");
        h.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        assert_eq!(cf_policy(&[]).resolve(&h, Some(peer)), "2606:4700::1");
    }

    // ----- Peer / ForwardedFor -----
    //
    // The lockout-bypass scenario: an attacker rotates `X-Forwarded-For`
    // per request to look like a fresh "IP" each time. Only the policy an
    // operator opted into may read it.

    #[test]
    fn forwarded_for_uses_the_first_hop() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.2.3.4, 5.6.7.8"),
        );
        let peer: SocketAddr = "9.9.9.9:1234".parse().unwrap();
        assert_eq!(
            ClientIpPolicy::ForwardedFor.resolve(&h, Some(peer)),
            "1.2.3.4"
        );
        assert_eq!(ClientIpPolicy::ForwardedFor.resolve(&h, None), "1.2.3.4");
    }

    #[test]
    fn peer_policy_ignores_every_header() {
        let mut h = cf_headers("1.2.3.4");
        h.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        let peer: SocketAddr = "9.9.9.9:1234".parse().unwrap();
        assert_eq!(ClientIpPolicy::Peer.resolve(&h, Some(peer)), "9.9.9.9");
    }

    #[test]
    fn falls_back_to_peer_then_unknown() {
        let h = HeaderMap::new();
        let peer: SocketAddr = "9.9.9.9:1234".parse().unwrap();
        for p in [
            ClientIpPolicy::Peer,
            ClientIpPolicy::ForwardedFor,
            cf_policy(&[]),
        ] {
            assert_eq!(p.resolve(&h, Some(peer)), "9.9.9.9");
            assert_eq!(p.resolve(&h, None), "unknown");
        }
    }
}
