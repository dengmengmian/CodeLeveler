//! Destination identity and validation shared by process and host networking.
//! A validated resource is a pinned DNS snapshot, not permission to resolve again.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkScope {
    None,
    Loopback,
    LocalLan,
    ConfiguredRemote(Vec<NetworkResource>),
    Internet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkTransport {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredRemoteIdentity {
    pub repository_identity: String,
    pub remote_name: String,
    pub canonical_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkResource {
    pub host: String,
    pub port: u16,
    pub transport: NetworkTransport,
    pub resolved_addresses: Vec<SocketAddr>,
    pub configured_remote: Option<ConfiguredRemoteIdentity>,
}

#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("invalid network destination: {0}")]
    InvalidDestination(String),
    #[error("network scope {0} denies destination")]
    ScopeDenied(&'static str),
    #[error("network DNS resolution failed: {0}")]
    Resolution(#[from] std::io::Error),
}

impl NetworkResource {
    pub fn from_resolved(
        host: impl Into<String>,
        port: u16,
        transport: NetworkTransport,
        resolved_addresses: Vec<SocketAddr>,
    ) -> Result<Self, NetworkError> {
        let mut resource = Self {
            host: host.into().to_ascii_lowercase(),
            port,
            transport,
            resolved_addresses,
            configured_remote: None,
        };
        resource.validate_destinations()?;
        resource.resolved_addresses = normalized_addresses(&resource.resolved_addresses);
        Ok(resource)
    }

    /// Recheck at the execution boundary, including deserialized or mutated records.
    pub fn validate_destinations(&self) -> Result<(), NetworkError> {
        if self.host.is_empty()
            || self.host.chars().any(char::is_whitespace)
            || self.port == 0
            || self.resolved_addresses.is_empty()
        {
            return Err(NetworkError::InvalidDestination(
                "missing host, port, or DNS addresses".into(),
            ));
        }
        for address in &self.resolved_addresses {
            if address.port() != self.port || !is_ordinary_destination(address.ip()) {
                return Err(NetworkError::InvalidDestination(address.to_string()));
            }
        }
        Ok(())
    }
}

impl NetworkScope {
    pub fn validate_resource(&self, resource: &NetworkResource) -> Result<(), NetworkError> {
        resource.validate_destinations()?;
        let allowed = match self {
            Self::None => false,
            Self::Loopback => resource
                .resolved_addresses
                .iter()
                .all(|a| is_loopback(a.ip())),
            Self::LocalLan => resource
                .resolved_addresses
                .iter()
                .all(|a| is_local_lan(a.ip())),
            Self::Internet => true,
            Self::ConfiguredRemote(resources) => {
                resource.configured_remote.as_ref().is_some_and(|identity| {
                    !identity.repository_identity.is_empty()
                        && !identity.remote_name.is_empty()
                        && !identity.canonical_url.is_empty()
                        && resources.iter().any(|allowed| {
                            allowed.validate_destinations().is_ok()
                                && allowed.configured_remote.as_ref() == Some(identity)
                                && allowed.host.eq_ignore_ascii_case(&resource.host)
                                && allowed.port == resource.port
                                && allowed.transport == resource.transport
                                && normalized_addresses(&allowed.resolved_addresses)
                                    == normalized_addresses(&resource.resolved_addresses)
                        })
                })
            }
        };
        if allowed {
            Ok(())
        } else {
            Err(NetworkError::ScopeDenied(self.label()))
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Loopback => "loopback",
            Self::LocalLan => "local_lan",
            Self::ConfiguredRemote(_) => "configured_remote",
            Self::Internet => "internet",
        }
    }
}

/// IPv4-mapped IPv6 must receive the underlying IPv4 policy.
pub fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

pub fn is_loopback(ip: IpAddr) -> bool {
    normalize_ip(ip).is_loopback()
}

/// Loopback, RFC1918, IPv4 link-local, IPv6 ULA and IPv6 link-local.
pub fn is_local_lan(ip: IpAddr) -> bool {
    match normalize_ip(ip) {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.segments()[0] & 0xfe00 == 0xfc00
                || ip.segments()[0] & 0xffc0 == 0xfe80
        }
    }
}

/// Reject wildcard, multicast, broadcast and reserved IPv4 destinations.
pub fn is_ordinary_destination(ip: IpAddr) -> bool {
    match normalize_ip(ip) {
        IpAddr::V4(ip) => {
            !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && ip.octets()[0] != 0
                && ip.octets()[0] < 240
        }
        IpAddr::V6(ip) => !ip.is_unspecified() && !ip.is_multicast(),
    }
}

fn normalized_addresses(addresses: &[SocketAddr]) -> Vec<SocketAddr> {
    let mut result: Vec<_> = addresses
        .iter()
        .map(|a| match (a, normalize_ip(a.ip())) {
            (SocketAddr::V6(_), IpAddr::V4(ip)) => SocketAddr::new(IpAddr::V4(ip), a.port()),
            _ => *a,
        })
        .collect();
    result.sort_unstable();
    result.dedup();
    result
}

/// Resolve once, validate every answer, and return the socket destinations to pin.
/// Consumers must disable proxy environment discovery and revalidate redirects.
pub async fn resolve_network_resource(
    host: &str,
    port: u16,
    transport: NetworkTransport,
) -> Result<NetworkResource, NetworkError> {
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::net::lookup_host((host, port)).await?.collect()
    };
    NetworkResource::from_resolved(host, port, transport, addresses)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(addresses: &[&str]) -> NetworkResource {
        NetworkResource::from_resolved(
            "localhost",
            3000,
            NetworkTransport::Tcp,
            addresses.iter().map(|s| s.parse().unwrap()).collect(),
        )
        .unwrap()
    }

    #[test]
    fn loopback_checks_every_resolved_address_and_mapped_ipv4() {
        assert!(
            NetworkScope::Loopback
                .validate_resource(&resource(&[
                    "127.0.0.2:3000",
                    "[::1]:3000",
                    "[::ffff:127.0.0.1]:3000"
                ]))
                .is_ok()
        );
        assert!(
            NetworkScope::Loopback
                .validate_resource(&resource(&["127.0.0.1:3000", "8.8.8.8:3000"]))
                .is_err()
        );
        assert!(
            NetworkScope::Loopback
                .validate_resource(&resource(&["[::ffff:192.168.1.1]:3000"]))
                .is_err()
        );
    }

    #[test]
    fn lan_ranges_and_ordinary_destination_boundaries() {
        for ip in [
            "10.1.2.3:3000",
            "172.16.0.1:3000",
            "192.168.1.1:3000",
            "169.254.1.1:3000",
            "[fc00::1]:3000",
            "[fe80::1]:3000",
            "[::1]:3000",
        ] {
            assert!(
                NetworkScope::LocalLan
                    .validate_resource(&resource(&[ip]))
                    .is_ok(),
                "{ip}"
            );
        }
        assert!(
            NetworkScope::LocalLan
                .validate_resource(&resource(&["8.8.8.8:3000"]))
                .is_err()
        );
        for ip in [
            "0.0.0.0:3000",
            "224.0.0.1:3000",
            "255.255.255.255:3000",
            "[::]:3000",
            "[ff02::1]:3000",
        ] {
            assert!(
                NetworkResource::from_resolved(
                    "host",
                    3000,
                    NetworkTransport::Tcp,
                    vec![ip.parse().unwrap()]
                )
                .is_err()
            );
        }
        assert!(
            NetworkResource::from_resolved("host", 3000, NetworkTransport::Tcp, vec![]).is_err()
        );
    }

    #[test]
    fn configured_remote_binds_identity_transport_and_dns_snapshot() {
        let mut allowed = resource(&["127.0.0.1:3000"]);
        allowed.configured_remote = Some(ConfiguredRemoteIdentity {
            repository_identity: "repo-a".into(),
            remote_name: "origin".into(),
            canonical_url: "https://example.test/a.git".into(),
        });
        let scope = NetworkScope::ConfiguredRemote(vec![allowed.clone()]);
        assert!(scope.validate_resource(&allowed).is_ok());
        let mut changed = allowed.clone();
        changed.configured_remote.as_mut().unwrap().canonical_url =
            "https://example.test/b.git".into();
        assert!(scope.validate_resource(&changed).is_err());
        changed = allowed.clone();
        changed.transport = NetworkTransport::Udp;
        assert!(scope.validate_resource(&changed).is_err());
        changed = allowed.clone();
        changed.resolved_addresses = vec!["127.0.0.2:3000".parse().unwrap()];
        assert!(scope.validate_resource(&changed).is_err());
        assert!(
            NetworkScope::ConfiguredRemote(vec![resource(&["127.0.0.1:3000"])])
                .validate_resource(&resource(&["127.0.0.1:3000"]))
                .is_err()
        );
    }

    #[test]
    fn none_denies_and_internet_requires_valid_destination() {
        assert!(
            NetworkScope::None
                .validate_resource(&resource(&["127.0.0.1:3000"]))
                .is_err()
        );
        assert!(
            NetworkScope::Internet
                .validate_resource(&resource(&["8.8.8.8:3000"]))
                .is_ok()
        );
        let mut invalid = resource(&["8.8.8.8:3000"]);
        invalid.resolved_addresses[0] = "0.0.0.0:3000".parse().unwrap();
        assert!(NetworkScope::Internet.validate_resource(&invalid).is_err());
    }

    #[test]
    fn resources_reject_unknown_hosts_and_port_mismatch() {
        for host in ["", " ", "host\nname"] {
            assert!(
                NetworkResource::from_resolved(
                    host,
                    3000,
                    NetworkTransport::Tcp,
                    vec!["127.0.0.1:3000".parse().unwrap()]
                )
                .is_err()
            );
        }
        assert!(
            NetworkResource::from_resolved(
                "localhost",
                3000,
                NetworkTransport::Tcp,
                vec!["127.0.0.1:3001".parse().unwrap()]
            )
            .is_err()
        );
        assert!(
            NetworkResource::from_resolved(
                "localhost",
                0,
                NetworkTransport::Tcp,
                vec!["127.0.0.1:0".parse().unwrap()]
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn resolver_pins_literal_addresses_and_localhost_dns() {
        let mapped = resolve_network_resource("[::ffff:127.0.0.1]", 3000, NetworkTransport::Tcp)
            .await
            .unwrap();
        assert_eq!(
            mapped.resolved_addresses,
            vec!["127.0.0.1:3000".parse::<SocketAddr>().unwrap()]
        );
        let localhost = resolve_network_resource("localhost", 3000, NetworkTransport::Tcp)
            .await
            .unwrap();
        assert!(NetworkScope::Loopback.validate_resource(&localhost).is_ok());
        assert!(
            resolve_network_resource("0.0.0.0", 3000, NetworkTransport::Tcp)
                .await
                .is_err()
        );
    }
}
