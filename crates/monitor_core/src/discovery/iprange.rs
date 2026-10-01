use std::net::Ipv4Addr;

use crate::error::{CoreError, Result};

/// Hard limit of addresses expanded from a single spec, protecting the UI and
/// the network from a typo such as `0.0.0.0/0`.
pub const MAX_TARGETS: usize = 4096;

/// Expands an address specification into a list of IPv4 addresses.
///
/// Accepted forms:
///
/// * `192.168.1.64`             – a single host.
/// * `192.168.1.1-254`          – a range sharing the first three octets.
/// * `192.168.1.10-192.168.1.60`– a fully qualified range.
/// * `192.168.1.0/24`           – CIDR notation (network and broadcast of
///   prefixes shorter than /31 are skipped).
pub fn parse_targets(spec: &str) -> Result<Vec<Ipv4Addr>> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(CoreError::network("empty address range"));
    }

    let targets = if let Some((base, prefix)) = spec.split_once('/') {
        let prefix: u8 = prefix
            .trim()
            .parse()
            .map_err(|_| CoreError::network(format!("invalid CIDR prefix: {spec}")))?;
        expand_cidr(base.trim(), prefix)?
    } else if let Some((start, end)) = spec.split_once('-') {
        expand_range(start.trim(), end.trim())?
    } else {
        vec![parse_ipv4(spec)?]
    };

    if targets.is_empty() {
        return Err(CoreError::network(format!("range {spec} expands to no address")));
    }
    if targets.len() > MAX_TARGETS {
        return Err(CoreError::network(format!(
            "range {spec} expands to {} addresses (limit {MAX_TARGETS})",
            targets.len()
        )));
    }
    Ok(targets)
}

/// Expands several specifications, de-duplicating the result.
pub fn parse_targets_multi(specs: &[String]) -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for spec in specs {
        match parse_targets(spec) {
            Ok(addresses) => {
                for address in addresses {
                    if !out.contains(&address) {
                        out.push(address);
                    }
                }
            }
            Err(err) => tracing::warn!(target: "xgview::discovery", spec, %err, "skipping invalid range"),
        }
    }
    out
}

fn parse_ipv4(text: &str) -> Result<Ipv4Addr> {
    text.parse::<Ipv4Addr>()
        .map_err(|_| CoreError::network(format!("invalid IPv4 address: {text}")))
}

fn expand_range(start: &str, end: &str) -> Result<Vec<Ipv4Addr>> {
    let start = parse_ipv4(start)?;
    let end = if end.contains('.') {
        parse_ipv4(end)?
    } else {
        let last: u8 = end
            .parse()
            .map_err(|_| CoreError::network(format!("invalid range end: {end}")))?;
        Ipv4Addr::new(start.octets()[0], start.octets()[1], start.octets()[2], last)
    };

    let start_value = u32::from(start);
    let end_value = u32::from(end);
    if end_value < start_value {
        return Err(CoreError::network(format!("reversed range: {start}-{end}")));
    }
    if (end_value - start_value) as usize + 1 > MAX_TARGETS {
        return Err(CoreError::network(format!(
            "range {start}-{end} is too large (limit {MAX_TARGETS})"
        )));
    }
    Ok((start_value..=end_value).map(Ipv4Addr::from).collect())
}

fn expand_cidr(base: &str, prefix: u8) -> Result<Vec<Ipv4Addr>> {
    if prefix > 32 {
        return Err(CoreError::network(format!("invalid CIDR prefix: /{prefix}")));
    }
    let base = parse_ipv4(base)?;
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    let network = u32::from(base) & mask;
    let broadcast = network | !mask;

    // Usable host range: skip network and broadcast addresses.
    let (first, last) = if prefix >= 31 {
        (network, broadcast)
    } else {
        (network + 1, broadcast - 1)
    };
    if (last - first) as usize + 1 > MAX_TARGETS {
        return Err(CoreError::network(format!(
            "{base}/{prefix} is too large (limit {MAX_TARGETS})"
        )));
    }
    Ok((first..=last).map(Ipv4Addr::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_address() {
        let targets = parse_targets("192.168.1.64").unwrap();
        assert_eq!(targets, vec![Ipv4Addr::new(192, 168, 1, 64)]);
    }

    #[test]
    fn parses_short_range() {
        let targets = parse_targets("192.168.10.1-4").unwrap();
        assert_eq!(targets.len(), 4);
        assert_eq!(targets[3], Ipv4Addr::new(192, 168, 10, 4));
    }

    #[test]
    fn parses_full_range() {
        let targets = parse_targets("10.0.0.1-10.0.0.3").unwrap();
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn parses_cidr_without_network_and_broadcast() {
        let targets = parse_targets("192.168.1.0/24").unwrap();
        assert_eq!(targets.len(), 254);
        assert_eq!(targets[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(targets[253], Ipv4Addr::new(192, 168, 1, 254));
    }

    #[test]
    fn rejects_reversed_range() {
        assert!(parse_targets("192.168.1.10-5").is_err());
    }

    #[test]
    fn deduplicates_multiple_specs() {
        let specs = vec!["10.0.0.1".to_string(), "10.0.0.1-2".to_string()];
        assert_eq!(parse_targets_multi(&specs).len(), 2);
    }
}
