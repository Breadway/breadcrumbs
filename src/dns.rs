use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Parse a DNS server list. Empty input deliberately means unpin; any
/// malformed non-empty token rejects the whole setting so it cannot become
/// an accidental unpin or partial pin.
pub fn parse_servers(value: &str) -> Result<(Vec<Ipv4Addr>, Vec<Ipv6Addr>), String> {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for token in value
        .split([',', ' ', ';'])
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        match token.parse::<IpAddr>() {
            Ok(IpAddr::V4(addr)) => v4.push(addr),
            Ok(IpAddr::V6(addr)) => v6.push(addr),
            Err(_) => return Err(format!("invalid DNS server '{token}'")),
        }
    }
    Ok((v4, v6))
}

pub fn validate(value: &str) -> Result<(), String> {
    parse_servers(value).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_explicit_unpin_and_mixed_valid_values_parse() {
        assert_eq!(parse_servers("").unwrap(), (vec![], vec![]));
        assert_eq!(parse_servers("  \t ").unwrap(), (vec![], vec![]));
        let (v4, v6) = parse_servers("1.1.1.1, 2606:4700::1111;9.9.9.9").unwrap();
        assert_eq!(
            v4,
            vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(9, 9, 9, 9)]
        );
        assert_eq!(v6, vec!["2606:4700::1111".parse::<Ipv6Addr>().unwrap()]);
    }

    #[test]
    fn rejects_invalid_nonempty_tokens_without_partial_result() {
        assert_eq!(
            parse_servers("1.1.1.x"),
            Err("invalid DNS server '1.1.1.x'".into())
        );
        assert!(parse_servers("1.1.1.1, bogus").is_err());
    }
}
