//! IP allowlisting: `TRAVERSE_ALLOWED_IPS` parsed into CIDR ranges, checked against peer addresses.
//! An empty allowlist accepts everything (the default).

use std::net::IpAddr;

/// Parsed CIDR range (e.g. `203.0.113.0/24` or `::1/128`).
#[derive(Debug, Clone)]
struct CidrRange {
    addr: IpAddr,
    prefix_len: u8,
}

impl CidrRange {
    fn contains(&self, ip: &IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(test)) => {
                if self.prefix_len == 0 {
                    return true;
                }
                if self.prefix_len >= 32 {
                    return net == *test;
                }
                let mask = u32::MAX << (32 - self.prefix_len);
                (u32::from(net) & mask) == (u32::from(*test) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(test)) => {
                if self.prefix_len == 0 {
                    return true;
                }
                if self.prefix_len >= 128 {
                    return net == *test;
                }
                let mask = u128::MAX << (128 - self.prefix_len);
                (u128::from(net) & mask) == (u128::from(*test) & mask)
            }
            _ => false, // v4 vs v6 mismatch
        }
    }
}

/// IP allowlist; empty accepts everything.
#[derive(Debug, Clone)]
pub struct IpFilter {
    ranges: Vec<CidrRange>,
}

impl IpFilter {
    /// Parses a `TRAVERSE_ALLOWED_IPS` value: comma-separated bare IPs (`192.168.1.1`) or CIDR
    /// ranges (`10.0.0.0/8`). Empty input gives an empty filter.
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        if input.is_empty() {
            return Ok(Self { ranges: Vec::new() });
        }

        let mut ranges = Vec::new();
        for entry in input.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }

            if let Some((addr_str, prefix_str)) = entry.split_once('/') {
                let addr: IpAddr = addr_str
                    .trim()
                    .parse()
                    .map_err(|e| format!("invalid IP in '{entry}': {e}"))?;
                let prefix_len: u8 = prefix_str
                    .trim()
                    .parse()
                    .map_err(|e| format!("invalid prefix in '{entry}': {e}"))?;
                let max_prefix = if addr.is_ipv4() { 32 } else { 128 };
                if prefix_len > max_prefix {
                    return Err(format!(
                        "prefix /{prefix_len} too large for {entry} (max /{max_prefix})"
                    ));
                }
                ranges.push(CidrRange { addr, prefix_len });
            } else {
                let addr: IpAddr = entry
                    .parse()
                    .map_err(|e| format!("invalid IP '{entry}': {e}"))?;
                let prefix_len = if addr.is_ipv4() { 32 } else { 128 };
                ranges.push(CidrRange { addr, prefix_len });
            }
        }

        Ok(Self { ranges })
    }

    /// Empty filter (accepts everything).
    pub fn allow_all() -> Self {
        Self { ranges: Vec::new() }
    }

    /// Is the IP allowed (or the filter empty)?
    pub fn is_allowed(&self, ip: &IpAddr) -> bool {
        if self.ranges.is_empty() {
            return true;
        }
        self.ranges.iter().any(|r| r.contains(ip))
    }

    /// Empty (accepts everything)?
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Number of ranges.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allows_all() {
        let f = IpFilter::parse("").unwrap();
        assert!(f.is_allowed(&"1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn single_ip() {
        let f = IpFilter::parse("192.168.1.1").unwrap();
        assert!(f.is_allowed(&"192.168.1.1".parse().unwrap()));
        assert!(!f.is_allowed(&"192.168.1.2".parse().unwrap()));
    }

    #[test]
    fn cidr_range() {
        let f = IpFilter::parse("10.0.0.0/8").unwrap();
        assert!(f.is_allowed(&"10.0.0.1".parse().unwrap()));
        assert!(f.is_allowed(&"10.255.255.255".parse().unwrap()));
        assert!(!f.is_allowed(&"11.0.0.1".parse().unwrap()));
    }

    #[test]
    fn multiple_entries() {
        let f = IpFilter::parse("10.0.0.0/8, 192.168.1.0/24, 203.0.113.42").unwrap();
        assert!(f.is_allowed(&"10.1.2.3".parse().unwrap()));
        assert!(f.is_allowed(&"192.168.1.100".parse().unwrap()));
        assert!(f.is_allowed(&"203.0.113.42".parse().unwrap()));
        assert!(!f.is_allowed(&"8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn ipv6() {
        let f = IpFilter::parse("::1, fe80::/10").unwrap();
        assert!(f.is_allowed(&"::1".parse().unwrap()));
        assert!(f.is_allowed(&"fe80::1".parse().unwrap()));
        assert!(!f.is_allowed(&"2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn invalid_input() {
        assert!(IpFilter::parse("not-an-ip").is_err());
        assert!(IpFilter::parse("10.0.0.0/33").is_err());
    }
}
