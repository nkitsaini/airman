use nix::ifaddrs::getifaddrs;

use crate::domain::IpAddressInfo;

pub fn interface_addresses(interface: &str) -> Vec<IpAddressInfo> {
    let interfaces = match getifaddrs() {
        Ok(interfaces) => interfaces,
        Err(error) => {
            tracing::debug!(%error, interface, "could not read kernel interface addresses");
            return Vec::new();
        }
    };
    let mut addresses = interfaces
        .filter(|entry| entry.interface_name == interface)
        .filter_map(|entry| {
            let address = entry.address?;
            let netmask = entry.netmask?;
            if let (Some(address), Some(netmask)) =
                (address.as_sockaddr_in(), netmask.as_sockaddr_in())
            {
                let mask = netmask.ip();
                return Some(IpAddressInfo {
                    address: address.ip().to_string(),
                    prefix_len: ipv4_prefix(mask.octets()),
                    netmask: mask.to_string(),
                });
            }
            if let (Some(address), Some(netmask)) =
                (address.as_sockaddr_in6(), netmask.as_sockaddr_in6())
            {
                let mask = netmask.ip();
                return Some(IpAddressInfo {
                    address: address.ip().to_string(),
                    prefix_len: ipv6_prefix(mask.segments()),
                    netmask: mask.to_string(),
                });
            }
            None
        })
        .collect::<Vec<_>>();
    addresses.sort();
    addresses.dedup();
    addresses
}

fn ipv4_prefix(mask: [u8; 4]) -> u8 {
    mask.iter().map(|octet| octet.count_ones()).sum::<u32>() as u8
}

fn ipv6_prefix(mask: [u16; 8]) -> u8 {
    mask.iter().map(|segment| segment.count_ones()).sum::<u32>() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculates_ipv4_and_ipv6_prefix_lengths() {
        assert_eq!(ipv4_prefix([255, 255, 255, 0]), 24);
        assert_eq!(ipv6_prefix([u16::MAX; 8]), 128);
        assert_eq!(
            ipv6_prefix([u16::MAX, u16::MAX, u16::MAX, u16::MAX, 0, 0, 0, 0]),
            64
        );
    }

    #[test]
    fn unknown_interface_has_no_addresses() {
        assert!(interface_addresses("airman-interface-that-cannot-exist").is_empty());
    }
}
