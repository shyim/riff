use std::net::IpAddr;

#[derive(Debug, Clone)]
pub struct NoProxyPattern {
    entries: Vec<NoProxyEntry>,
}

#[derive(Debug, Clone)]
struct NoProxyEntry {
    host: HostPattern,
    port: Option<u16>,
}

#[derive(Debug, Clone)]
enum HostPattern {
    Any,
    Domain(String),
    Address(IpAddr),
    Network(IpAddr, u8),
}

impl NoProxyPattern {
    pub fn new(pattern: &str) -> Self {
        Self {
            entries: pattern.split(',').filter_map(parse_entry).collect(),
        }
    }

    pub fn matches(&self, url: &url::Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let port = url.port_or_known_default();
        let address = parse_address(host);

        self.entries.iter().any(|entry| {
            if entry.port.is_some() && entry.port != port {
                return false;
            }
            match &entry.host {
                HostPattern::Any => true,
                HostPattern::Domain(domain) => {
                    host.eq_ignore_ascii_case(domain)
                        || host
                            .strip_suffix(domain)
                            .is_some_and(|prefix| prefix.ends_with('.'))
                }
                HostPattern::Address(expected) => address == Some(*expected),
                HostPattern::Network(network, prefix) => {
                    address.is_some_and(|address| network_contains(*network, *prefix, address))
                }
            }
        })
    }
}

fn parse_entry(raw: &str) -> Option<NoProxyEntry> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw == "*" {
        return Some(NoProxyEntry {
            host: HostPattern::Any,
            port: None,
        });
    }

    let (host, port) = split_host_port(raw);
    let host = host.trim_matches(['[', ']']);
    let host_pattern = if let Some((network, prefix)) = host.split_once('/') {
        let network = parse_address(network)?;
        let prefix = prefix.parse::<u8>().ok()?;
        let valid_prefix = match network {
            IpAddr::V4(_) => prefix <= 32,
            IpAddr::V6(_) => prefix <= 128,
        };
        valid_prefix.then_some(HostPattern::Network(network, prefix))?
    } else if let Some(address) = parse_address(host) {
        HostPattern::Address(address)
    } else {
        HostPattern::Domain(host.trim_start_matches('.').to_ascii_lowercase())
    };

    Some(NoProxyEntry {
        host: host_pattern,
        port,
    })
}

fn split_host_port(raw: &str) -> (&str, Option<u16>) {
    if let Some(bracket_end) = raw.find(']') {
        let host = &raw[..=bracket_end];
        let port = raw
            .get(bracket_end + 1..)
            .and_then(|suffix| suffix.strip_prefix(':'))
            .and_then(|port| port.parse().ok());
        return (host, port);
    }
    if raw.bytes().filter(|byte| *byte == b':').count() == 1 {
        if let Some((host, port)) = raw.rsplit_once(':') {
            if let Ok(port) = port.parse() {
                return (host, Some(port));
            }
        }
    }
    (raw, None)
}

fn parse_address(host: &str) -> Option<IpAddr> {
    let address = host.trim_matches(['[', ']']).parse::<IpAddr>().ok()?;
    Some(match address {
        IpAddr::V6(address) if address.to_ipv4_mapped().is_some() => {
            IpAddr::V4(address.to_ipv4_mapped().unwrap())
        }
        address => address,
    })
}

fn network_contains(network: IpAddr, prefix: u8, address: IpAddr) -> bool {
    match (network, address) {
        (IpAddr::V4(network), IpAddr::V4(address)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            u32::from(network) & mask == u32::from(address) & mask
        }
        (IpAddr::V6(network), IpAddr::V6(address)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            u128::from(network) & mask == u128::from(address) & mask
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, host: &str) -> bool {
        let url = if host.starts_with('[') || !host.contains(':') {
            format!("http://{host}")
        } else if host.ends_with(":443") {
            format!("https://{host}")
        } else {
            format!("http://{host}")
        };
        NoProxyPattern::new(pattern).matches(&url::Url::parse(&url).unwrap())
    }

    // Ported from Composer\Test\Util\NoProxyPatternTest::testHostName.
    #[test]
    fn composer_no_proxy_matches_domains_on_label_boundaries() {
        let pattern = "foobar.com, .barbaz.net";
        for (host, expected) in [
            ("foobar.com", true),
            ("www.foobar.com", true),
            ("foofoobar.com", false),
            ("barbaz.net", true),
            ("www.barbaz.net", true),
            ("barbarbaz.net", false),
            ("barbaz.com", false),
            ("foobar.com.", false),
        ] {
            assert_eq!(matches(pattern, host), expected, "host {host}");
        }
    }

    // Ported from Composer\Test\Util\NoProxyPatternTest::testIpAddress.
    #[test]
    fn composer_no_proxy_matches_ipv4_ipv6_and_mapped_addresses() {
        let pattern = "192.168.1.1, 2001:db8::52:0:1";
        for (host, expected) in [
            ("192.168.1.1", true),
            ("192.168.1.4", false),
            ("[2001:db8:0:0:0:52:0:1]", true),
            ("[2001:db8:0:0:0:52:0:2]", false),
            ("[::FFFF:C0A8:0101]", true),
            ("[::FFFF:C0A8:0104]", false),
        ] {
            assert_eq!(matches(pattern, host), expected, "host {host}");
        }
    }

    // Ported from Composer\Test\Util\NoProxyPatternTest::testIpRange.
    #[test]
    fn composer_no_proxy_matches_ipv4_and_ipv6_cidr_ranges() {
        let pattern = "10.0.0.0/30, 2002:db8:a::45/121";
        for (host, expected) in [
            ("10.0.0.2", true),
            ("10.0.0.4", false),
            ("[2002:db8:a:0:0:0:0:7f]", true),
            ("[2002:db8:a:0:0:0:0:ff]", false),
            ("[::FFFF:0A00:0002]", true),
            ("[::FFFF:0A00:0004]", false),
        ] {
            assert_eq!(matches(pattern, host), expected, "host {host}");
        }
    }

    // Ported from Composer\Test\Util\NoProxyPatternTest::testPort.
    #[test]
    fn composer_no_proxy_honors_explicit_ports() {
        let pattern =
            "192.168.1.2:81, 192.168.1.3:80, [2001:db8::52:0:2]:443, [2001:db8::52:0:3]:80";
        for (host, expected) in [
            ("192.168.1.3", true),
            ("192.168.1.2", false),
            ("[2001:db8::52:0:3]", true),
            ("[2001:db8::52:0:2]", false),
        ] {
            assert_eq!(matches(pattern, host), expected, "host {host}");
        }
    }
}
