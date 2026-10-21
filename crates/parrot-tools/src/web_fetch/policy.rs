use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Host and resolved-address checks are both applied before a connection is pinned.
pub trait AddressPolicy: Send + Sync {
    fn allows_host(&self, host: &str) -> bool;
    fn allows(&self, address: IpAddr) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PublicAddressPolicy;

impl AddressPolicy for PublicAddressPolicy {
    fn allows_host(&self, host: &str) -> bool {
        host != "localhost" && !host.ends_with(".localhost")
    }

    fn allows(&self, address: IpAddr) -> bool {
        match canonical_address(address) {
            IpAddr::V4(address) => allows_public_v4(address),
            IpAddr::V6(address) => allows_public_v6(address),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PrivateAddressPolicy;

impl AddressPolicy for PrivateAddressPolicy {
    fn allows_host(&self, _host: &str) -> bool {
        true
    }

    fn allows(&self, address: IpAddr) -> bool {
        match address {
            IpAddr::V4(address) => address != Ipv4Addr::UNSPECIFIED && !address.is_multicast(),
            IpAddr::V6(address) => address != Ipv6Addr::UNSPECIFIED && !address.is_multicast(),
        }
    }
}

pub(crate) fn canonical_address(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        address => address,
    }
}

pub(crate) fn canonical_host(host: &str) -> String {
    host.trim_end_matches('.').to_lowercase()
}

fn allows_public_v4(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    first != 0
        && first != 10
        && first != 127
        && !(first == 100 && (64..=127).contains(&second))
        && !(first == 169 && second == 254)
        && !(first == 172 && (16..=31).contains(&second))
        && !(first == 192 && second == 0 && third == 0)
        && !(first == 192 && second == 0 && third == 2)
        && !(first == 192 && second == 168)
        && !(first == 198 && matches!(second, 18 | 19))
        && !(first == 198 && second == 51 && third == 100)
        && !(first == 203 && second == 0 && third == 113)
        && first < 224
}

fn allows_public_v6(address: Ipv6Addr) -> bool {
    let bytes = address.octets();
    let site_local = bytes[0] == 0xfe && bytes[1] & 0xc0 == 0xc0;
    let link_local = bytes[0] == 0xfe && bytes[1] & 0xc0 == 0x80;
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || site_local
        || link_local
        || bytes[0] & 0xfe == 0xfc
    {
        return false;
    }
    ![
        &[0x00, 0x64, 0xff, 0x9b, 0x00, 0x01][..],
        &[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00][..],
        &[0x20, 0x01, 0x00, 0x02, 0x00, 0x00][..],
        &[0x20, 0x01, 0x0d, 0xb8][..],
    ]
    .iter()
    .any(|prefix| bytes.starts_with(prefix))
}
