use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::Duration;

use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};

use crate::error::Error;

const SERVICE_TYPE: &str = "_googlecast._tcp.local.";

/// Cast receiver capability bitfield advertised through DNS-SD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CastCapabilities(u64);

impl CastCapabilities {
    pub const VIDEO_OUTPUT: Self = Self(1 << 0);
    pub const VIDEO_INPUT: Self = Self(1 << 1);
    pub const AUDIO_OUTPUT: Self = Self(1 << 2);
    pub const AUDIO_INPUT: Self = Self(1 << 3);
    pub const DEVELOPMENT_MODE: Self = Self(1 << 4);
    pub const MULTIZONE_GROUP: Self = Self(1 << 5);

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn contains(self, capability: Self) -> bool {
        self.0 & capability.0 == capability.0
    }
}

/// One resolved route to a Cast receiver.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CastEndpoint {
    address: SocketAddr,
    interface_index: Option<u32>,
    interface_name: Option<String>,
}

impl CastEndpoint {
    /// Scoped socket address suitable for a Cast control connection.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// OS interface index on which mDNS discovered this route.
    pub fn interface_index(&self) -> Option<u32> {
        self.interface_index
    }

    /// OS interface name on which mDNS discovered this route.
    pub fn interface_name(&self) -> Option<&str> {
        self.interface_name.as_deref()
    }
}

/// A Cast device discovered on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastDevice {
    friendly_name: String,
    model: String,
    stable_id: String,
    service_instance: String,
    endpoints: Vec<CastEndpoint>,
    capabilities: CastCapabilities,
    protocol_version: Option<u8>,
}

impl CastDevice {
    /// Friendly receiver name, such as "Living Room TV".
    pub fn name(&self) -> &str {
        &self.friendly_name
    }

    /// Receiver model advertised in the DNS-SD TXT record.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Stable Cast device UUID. This is the preferred UI identity key.
    pub fn id(&self) -> &str {
        &self.stable_id
    }

    /// Full DNS-SD service instance name.
    pub fn service_instance(&self) -> &str {
        &self.service_instance
    }

    /// All currently resolved, explicitly ordered routes.
    pub fn endpoints(&self) -> &[CastEndpoint] {
        &self.endpoints
    }

    /// Preferred route: IPv4, then global IPv6, then scoped link-local IPv6.
    pub fn preferred_endpoint(&self) -> &CastEndpoint {
        // Resolved devices are only constructed with at least one route.
        &self.endpoints[0]
    }

    /// Backward-compatible IP accessor for the preferred route.
    pub fn addr(&self) -> IpAddr {
        self.preferred_endpoint().address.ip()
    }

    /// Cast control port for the preferred route.
    pub fn port(&self) -> u16 {
        self.preferred_endpoint().address.port()
    }

    /// Receiver capabilities advertised in the `ca` TXT property.
    pub fn capabilities(&self) -> CastCapabilities {
        self.capabilities
    }

    /// Advertised Cast protocol version, when valid.
    pub fn protocol_version(&self) -> Option<u8> {
        self.protocol_version
    }
}

/// Discover Cast receivers on all enabled mDNS interfaces.
///
/// Results are coalesced by stable Cast device ID. Every usable address is
/// retained with its discovery interface; IPv6 link-local addresses are only
/// returned when the mDNS backend supplied a nonzero scope identifier.
pub async fn discover(discovery_timeout: Duration) -> Result<Vec<CastDevice>, Error> {
    let daemon = ServiceDaemon::new()
        .map_err(|error| discovery_error(format!("create mDNS daemon: {error}")))?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .map_err(|error| discovery_error(format!("browse {SERVICE_TYPE}: {error}")))?;

    let mut devices = HashMap::<String, CastDevice>::new();
    let deadline = tokio::time::Instant::now() + discovery_timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, receiver.recv_async()).await {
            Ok(Ok(ServiceEvent::ServiceResolved(service))) => {
                if let Some(device) = device_from_service(&service) {
                    devices
                        .entry(device.stable_id.clone())
                        .and_modify(|current| merge_device(current, &device))
                        .or_insert(device);
                }
            }
            Ok(Ok(ServiceEvent::ServiceRemoved(_, service_instance))) => {
                devices.retain(|_, device| device.service_instance != service_instance);
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                let _ = daemon.stop_browse(SERVICE_TYPE);
                let _ = daemon.shutdown();
                return Err(discovery_error(format!("receive mDNS event: {error}")));
            }
            Err(_) => break,
        }
    }

    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    let mut devices: Vec<_> = devices.into_values().collect();
    devices.sort_by(|left, right| {
        left.friendly_name
            .cmp(&right.friendly_name)
            .then_with(|| left.stable_id.cmp(&right.stable_id))
    });
    Ok(devices)
}

fn device_from_service(service: &ResolvedService) -> Option<CastDevice> {
    let stable_id = service.get_property_val_str("id")?.trim();
    if stable_id.is_empty() || service.port == 0 {
        return None;
    }

    let mut endpoints = Vec::new();
    for scoped_ip in &service.addresses {
        append_endpoints(&mut endpoints, scoped_ip, service.port);
    }
    sort_and_deduplicate_endpoints(&mut endpoints);
    if endpoints.is_empty() {
        return None;
    }

    let friendly_name = service
        .get_property_val_str("fn")
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| service.get_hostname())
        .to_owned();
    let capabilities = service
        .get_property_val_str("ca")
        .and_then(|bits| bits.parse().ok())
        .map(CastCapabilities::from_bits)
        .unwrap_or_default();
    let protocol_version = service
        .get_property_val_str("ve")
        .and_then(|version| version.parse().ok());

    Some(CastDevice {
        friendly_name,
        model: service
            .get_property_val_str("md")
            .unwrap_or_default()
            .to_owned(),
        stable_id: stable_id.to_owned(),
        service_instance: service.fullname.clone(),
        endpoints,
        capabilities,
        protocol_version,
    })
}

fn append_endpoints(endpoints: &mut Vec<CastEndpoint>, scoped_ip: &ScopedIp, port: u16) {
    match scoped_ip {
        ScopedIp::V4(scoped) => {
            let address = *scoped.addr();
            if address.is_unspecified() || address.is_multicast() {
                return;
            }
            if scoped.interface_ids().is_empty() {
                endpoints.push(CastEndpoint {
                    address: SocketAddrV4::new(address, port).into(),
                    interface_index: None,
                    interface_name: None,
                });
            } else {
                endpoints.extend(scoped.interface_ids().iter().map(|interface| CastEndpoint {
                    address: SocketAddrV4::new(address, port).into(),
                    interface_index: (interface.index != 0).then_some(interface.index),
                    interface_name: (!interface.name.is_empty()).then(|| interface.name.clone()),
                }));
            }
        }
        ScopedIp::V6(scoped) => {
            let address = *scoped.addr();
            if address.is_unspecified() || address.is_multicast() {
                return;
            }
            let interface = scoped.scope_id();
            if is_ipv6_unicast_link_local(address) && interface.index == 0 {
                return;
            }
            let scope_id = if is_ipv6_unicast_link_local(address) {
                interface.index
            } else {
                0
            };
            endpoints.push(CastEndpoint {
                address: SocketAddrV6::new(address, port, 0, scope_id).into(),
                interface_index: (interface.index != 0).then_some(interface.index),
                interface_name: (!interface.name.is_empty()).then(|| interface.name.clone()),
            });
        }
        _ => {}
    }
}

fn merge_device(current: &mut CastDevice, update: &CastDevice) {
    current.friendly_name.clone_from(&update.friendly_name);
    current.model.clone_from(&update.model);
    current
        .service_instance
        .clone_from(&update.service_instance);
    current.capabilities = update.capabilities;
    current.protocol_version = update.protocol_version;
    current.endpoints.extend(update.endpoints.iter().cloned());
    sort_and_deduplicate_endpoints(&mut current.endpoints);
}

fn sort_and_deduplicate_endpoints(endpoints: &mut Vec<CastEndpoint>) {
    endpoints.sort_by(|left, right| {
        endpoint_priority(left.address)
            .cmp(&endpoint_priority(right.address))
            .then_with(|| left.address.ip().cmp(&right.address.ip()))
            .then_with(|| left.interface_index.cmp(&right.interface_index))
    });
    endpoints.dedup();
}

fn endpoint_priority(address: SocketAddr) -> u8 {
    match address.ip() {
        IpAddr::V4(_) => 0,
        IpAddr::V6(address) if !is_ipv6_unicast_link_local(address) => 1,
        IpAddr::V6(_) => 2,
    }
}

fn is_ipv6_unicast_link_local(address: std::net::Ipv6Addr) -> bool {
    address.segments()[0] & 0xffc0 == 0xfe80
}

fn discovery_error(detail: impl Into<String>) -> Error {
    Error::ConnectionFailed(detail.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(address: &str, interface_index: Option<u32>) -> CastEndpoint {
        CastEndpoint {
            address: address.parse().unwrap(),
            interface_index,
            interface_name: interface_index.map(|index| format!("if{index}")),
        }
    }

    #[test]
    fn endpoints_are_stably_ordered_by_reachability() {
        let mut endpoints = vec![
            endpoint("[fe80::1%3]:8009", Some(3)),
            endpoint("[2001:db8::1]:8009", Some(2)),
            endpoint("192.0.2.2:8009", Some(2)),
            endpoint("192.0.2.1:8009", Some(1)),
        ];
        sort_and_deduplicate_endpoints(&mut endpoints);
        let addresses: Vec<_> = endpoints.iter().map(CastEndpoint::address).collect();
        assert_eq!(addresses[0], "192.0.2.1:8009".parse().unwrap());
        assert_eq!(addresses[1], "192.0.2.2:8009".parse().unwrap());
        assert_eq!(addresses[2], "[2001:db8::1]:8009".parse().unwrap());
        assert_eq!(addresses[3], "[fe80::1%3]:8009".parse().unwrap());
    }

    #[test]
    fn capability_bits_are_preserved() {
        let capabilities = CastCapabilities::from_bits(0b1_0101);
        assert!(capabilities.contains(CastCapabilities::VIDEO_OUTPUT));
        assert!(capabilities.contains(CastCapabilities::AUDIO_OUTPUT));
        assert!(capabilities.contains(CastCapabilities::DEVELOPMENT_MODE));
        assert_eq!(capabilities.bits(), 0b1_0101);
    }
}
