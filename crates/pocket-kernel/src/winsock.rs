// Quake3CE imports WS2 for IPv4 UDP; this model stays inside the guest and never opens host sockets.
use std::collections::{HashMap, VecDeque};
use std::net::Ipv4Addr;
use std::str::FromStr;

use crate::Heap;

pub const AF_INET: u32 = 2;
pub const SOCK_DGRAM: u32 = 2;
pub const IPPROTO_UDP: u32 = 17;
pub const SOL_SOCKET: u32 = 0xffff;
pub const SO_REUSEADDR: u32 = 0x0004;
pub const SO_BROADCAST: u32 = 0x0020;
pub const INADDR_ANY: u32 = 0;
pub const INADDR_BROADCAST: u32 = u32::MAX;
pub const IPV4_LOOPBACK: u32 = u32::from_le_bytes([127, 0, 0, 1]);
pub const WSADATA_SIZE: usize = 400;
pub const HOSTENT_SIZE: usize = 128;
pub const MAX_DATAGRAM_BYTES: usize = 65_507;
pub const FIONBIO: u32 = 0x8004_667e;
pub const FIONREAD: u32 = 0x4004_667f;

const SOCKET_HANDLE_BASE: u32 = 0xd000_0000;
const MAX_SOCKETS: usize = 128;
const MAX_QUEUED_DATAGRAMS: usize = 256;
const EPHEMERAL_PORT_START: u16 = 49_152;
const EPHEMERAL_PORT_END: u16 = 65_535;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SockAddrV4 {
    pub address: u32,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datagram {
    pub data: Vec<u8>,
    pub source: SockAddrV4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketError {
    NotInitialized,
    UnsupportedVersion,
    InvalidSocket,
    AddressFamilyNotSupported,
    SocketTypeNotSupported,
    ProtocolNotSupported,
    AddressInUse,
    AddressNotAvailable,
    WouldBlock,
    MessageTooLarge,
    NetworkUnreachable,
    DestinationRequired,
    NotConnected,
    InvalidArgument,
    NoBufferSpace,
    HostNotFound,
    Fault,
    AccessDenied,
    NoProtocolOption,
}

impl SocketError {
    pub const fn code(self) -> u32 {
        match self {
            Self::NotInitialized => 10_093,
            Self::UnsupportedVersion => 10_092,
            Self::InvalidSocket => 10_038,
            Self::AddressFamilyNotSupported => 10_047,
            Self::SocketTypeNotSupported => 10_044,
            Self::ProtocolNotSupported => 10_043,
            Self::AddressInUse => 10_048,
            Self::AddressNotAvailable => 10_049,
            Self::WouldBlock => 10_035,
            Self::MessageTooLarge => 10_040,
            Self::NetworkUnreachable => 10_051,
            Self::DestinationRequired => 10_039,
            Self::NotConnected => 10_057,
            Self::InvalidArgument => 10_022,
            Self::NoBufferSpace => 10_055,
            Self::HostNotFound => 11_001,
            Self::Fault => 10_014,
            Self::AccessDenied => 10_013,
            Self::NoProtocolOption => 10_042,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Socket {
    bound: Option<SockAddrV4>,
    peer: Option<SockAddrV4>,
    reuse_address: bool,
    broadcast: bool,
    received: VecDeque<Datagram>,
}

#[derive(Debug)]
pub struct WinsockState {
    startup_references: u32,
    next_handle: u32,
    next_ephemeral_port: u16,
    sockets: HashMap<u32, Socket>,
    last_errors: HashMap<usize, u32>,
    hostent_buffer: Option<u32>,
    hostname: String,
}

impl Default for WinsockState {
    fn default() -> Self {
        Self {
            startup_references: 0,
            next_handle: SOCKET_HANDLE_BASE,
            next_ephemeral_port: EPHEMERAL_PORT_START,
            sockets: HashMap::new(),
            last_errors: HashMap::new(),
            hostent_buffer: None,
            hostname: "pockethle".to_string(),
        }
    }
}

impl WinsockState {
    pub fn startup(&mut self, requested_version: u16) -> Result<(), SocketError> {
        let major = requested_version as u8;
        let minor = (requested_version >> 8) as u8;
        let supported = matches!((major, minor), (1, 0 | 1) | (2, 0..=2));
        if !supported {
            return Err(SocketError::UnsupportedVersion);
        }
        self.startup_references = self.startup_references.saturating_add(1);
        Ok(())
    }

    pub fn cleanup(&mut self) -> Result<(), SocketError> {
        if self.startup_references == 0 {
            return Err(SocketError::NotInitialized);
        }
        self.startup_references -= 1;
        if self.startup_references == 0 {
            self.sockets.clear();
        }
        Ok(())
    }

    pub fn initialized(&self) -> bool {
        self.startup_references > 0
    }

    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    pub fn resolve_ipv4(&self, name: &str) -> Result<u32, SocketError> {
        if name.eq_ignore_ascii_case("localhost") || name.eq_ignore_ascii_case(&self.hostname) {
            return Ok(IPV4_LOOPBACK);
        }
        Ipv4Addr::from_str(name)
            .map(|address| u32::from_le_bytes(address.octets()))
            .map_err(|_| SocketError::HostNotFound)
    }

    pub fn hostent_buffer(&mut self, heap: &mut Heap) -> Result<u32, SocketError> {
        if let Some(address) = self.hostent_buffer {
            return Ok(address);
        }
        let address = heap
            .alloc(HOSTENT_SIZE as u32)
            .ok_or(SocketError::NoBufferSpace)?;
        self.hostent_buffer = Some(address);
        Ok(address)
    }

    pub fn last_error_for_thread(&self, thread: usize) -> u32 {
        self.last_errors.get(&thread).copied().unwrap_or_default()
    }

    pub fn set_last_error_for_thread(&mut self, thread: usize, error: u32) {
        self.last_errors.insert(thread, error);
    }

    pub fn create_socket(
        &mut self,
        address_family: u32,
        socket_type: u32,
        protocol: u32,
    ) -> Result<u32, SocketError> {
        self.ensure_initialized()?;
        if address_family != AF_INET {
            return Err(SocketError::AddressFamilyNotSupported);
        }
        if socket_type != SOCK_DGRAM {
            return Err(SocketError::SocketTypeNotSupported);
        }
        if protocol != 0 && protocol != IPPROTO_UDP {
            return Err(SocketError::ProtocolNotSupported);
        }
        if self.sockets.len() >= MAX_SOCKETS {
            return Err(SocketError::NoBufferSpace);
        }
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1).max(SOCKET_HANDLE_BASE);
        while self.sockets.contains_key(&self.next_handle) {
            self.next_handle = self.next_handle.wrapping_add(1).max(SOCKET_HANDLE_BASE);
        }
        self.sockets.insert(handle, Socket::default());
        Ok(handle)
    }

    pub fn bind(&mut self, handle: u32, mut address: SockAddrV4) -> Result<(), SocketError> {
        self.ensure_initialized()?;
        let socket = self
            .sockets
            .get(&handle)
            .ok_or(SocketError::InvalidSocket)?;
        if socket.bound.is_some() {
            return Err(SocketError::InvalidArgument);
        }
        let reuse_address = socket.reuse_address;
        if address.address != INADDR_ANY && !is_loopback(address.address) {
            return Err(SocketError::AddressNotAvailable);
        }
        if address.port == 0 {
            address.port = self.allocate_ephemeral_port()?;
        }
        let conflict = self.sockets.iter().any(|(other_handle, other)| {
            if *other_handle == handle {
                return false;
            }
            let Some(existing) = other.bound else {
                return false;
            };
            existing.port == address.port
                && addresses_overlap(existing.address, address.address)
                && !(reuse_address && other.reuse_address)
        });
        if conflict {
            return Err(SocketError::AddressInUse);
        }
        self.sockets
            .get_mut(&handle)
            .ok_or(SocketError::InvalidSocket)?
            .bound = Some(address);
        Ok(())
    }

    pub fn connect(&mut self, handle: u32, peer: SockAddrV4) -> Result<(), SocketError> {
        self.ensure_initialized()?;
        if peer.address == INADDR_ANY && peer.port == 0 {
            let socket = self
                .sockets
                .get_mut(&handle)
                .ok_or(SocketError::InvalidSocket)?;
            socket.peer = None;
            return Ok(());
        }
        if peer.port == 0 {
            return Err(SocketError::InvalidArgument);
        }
        if peer.address != INADDR_BROADCAST && !is_loopback(peer.address) {
            return Err(SocketError::NetworkUnreachable);
        }
        self.ensure_bound(handle)?;
        self.sockets
            .get_mut(&handle)
            .ok_or(SocketError::InvalidSocket)?
            .peer = Some(peer);
        Ok(())
    }

    pub fn send(&mut self, handle: u32, data: &[u8]) -> Result<usize, SocketError> {
        self.ensure_initialized()?;
        let peer = self
            .sockets
            .get(&handle)
            .ok_or(SocketError::InvalidSocket)?
            .peer
            .ok_or(SocketError::DestinationRequired)?;
        self.send_to(handle, data, peer)
    }

    pub fn send_to(
        &mut self,
        handle: u32,
        data: &[u8],
        mut destination: SockAddrV4,
    ) -> Result<usize, SocketError> {
        self.ensure_initialized()?;
        if data.len() > MAX_DATAGRAM_BYTES {
            return Err(SocketError::MessageTooLarge);
        }
        if destination.port == 0 {
            return Err(SocketError::DestinationRequired);
        }
        if destination.address == INADDR_ANY {
            destination.address = IPV4_LOOPBACK;
        }
        if destination.address == INADDR_BROADCAST
            && !self
                .sockets
                .get(&handle)
                .ok_or(SocketError::InvalidSocket)?
                .broadcast
        {
            return Err(SocketError::AccessDenied);
        }
        if destination.address != INADDR_BROADCAST && !is_loopback(destination.address) {
            return Err(SocketError::NetworkUnreachable);
        }
        let bound = self.ensure_bound(handle)?;
        let source = SockAddrV4 {
            address: if bound.address == INADDR_ANY {
                IPV4_LOOPBACK
            } else {
                bound.address
            },
            port: bound.port,
        };
        let recipients: Vec<u32> = self
            .sockets
            .iter()
            .filter_map(|(recipient_handle, socket)| {
                let local = socket.bound?;
                let ip_matches = destination.address == INADDR_BROADCAST
                    || local.address == INADDR_ANY
                    || local.address == destination.address;
                (local.port == destination.port && ip_matches).then_some(*recipient_handle)
            })
            .collect();
        for recipient in recipients {
            let socket = self
                .sockets
                .get_mut(&recipient)
                .ok_or(SocketError::InvalidSocket)?;
            if socket.received.len() == MAX_QUEUED_DATAGRAMS {
                socket.received.pop_front();
            }
            socket.received.push_back(Datagram {
                data: data.to_vec(),
                source,
            });
        }
        Ok(data.len())
    }

    pub fn receive(&mut self, handle: u32) -> Result<Datagram, SocketError> {
        self.ensure_initialized()?;
        if self
            .sockets
            .get(&handle)
            .ok_or(SocketError::InvalidSocket)?
            .peer
            .is_none()
        {
            return Err(SocketError::NotConnected);
        }
        self.receive_from(handle)
    }

    pub fn receive_from(&mut self, handle: u32) -> Result<Datagram, SocketError> {
        self.ensure_initialized()?;
        let socket = self
            .sockets
            .get_mut(&handle)
            .ok_or(SocketError::InvalidSocket)?;
        let peer = socket.peer;
        while let Some(datagram) = socket.received.pop_front() {
            if peer.is_none_or(|peer| peer == datagram.source) {
                return Ok(datagram);
            }
        }
        Err(SocketError::WouldBlock)
    }

    pub fn close(&mut self, handle: u32) -> Result<(), SocketError> {
        self.ensure_initialized()?;
        self.sockets
            .remove(&handle)
            .map(|_| ())
            .ok_or(SocketError::InvalidSocket)
    }

    pub fn set_nonblocking(&mut self, handle: u32, _enabled: bool) -> Result<(), SocketError> {
        self.ensure_initialized()?;
        self.sockets
            .get(&handle)
            .map(|_| ())
            .ok_or(SocketError::InvalidSocket)
    }

    pub fn set_option(
        &mut self,
        handle: u32,
        level: u32,
        option: u32,
        value: u32,
    ) -> Result<(), SocketError> {
        self.ensure_initialized()?;
        let socket = self
            .sockets
            .get_mut(&handle)
            .ok_or(SocketError::InvalidSocket)?;
        if level == SOL_SOCKET {
            match option {
                SO_REUSEADDR => socket.reuse_address = value != 0,
                SO_BROADCAST => socket.broadcast = value != 0,
                0x1001 | 0x1002 | 0x1005 | 0x1006 => {}
                _ => return Err(SocketError::NoProtocolOption),
            }
        } else if level != 0 && level != IPPROTO_UDP {
            return Err(SocketError::NoProtocolOption);
        }
        Ok(())
    }

    pub fn pending_bytes(&self, handle: u32) -> Result<u32, SocketError> {
        self.ensure_initialized()?;
        let socket = self
            .sockets
            .get(&handle)
            .ok_or(SocketError::InvalidSocket)?;
        Ok(socket
            .received
            .front()
            .map_or(0, |datagram| datagram.data.len() as u32))
    }

    fn ensure_initialized(&self) -> Result<(), SocketError> {
        if self.initialized() {
            Ok(())
        } else {
            Err(SocketError::NotInitialized)
        }
    }

    fn ensure_bound(&mut self, handle: u32) -> Result<SockAddrV4, SocketError> {
        let bound = self
            .sockets
            .get(&handle)
            .ok_or(SocketError::InvalidSocket)?
            .bound;
        if let Some(bound) = bound {
            return Ok(bound);
        }
        self.bind(
            handle,
            SockAddrV4 {
                address: INADDR_ANY,
                port: 0,
            },
        )?;
        self.sockets
            .get(&handle)
            .and_then(|socket| socket.bound)
            .ok_or(SocketError::InvalidSocket)
    }

    fn allocate_ephemeral_port(&mut self) -> Result<u16, SocketError> {
        let range = u32::from(EPHEMERAL_PORT_END) - u32::from(EPHEMERAL_PORT_START) + 1;
        for _ in 0..range {
            let candidate = self.next_ephemeral_port;
            self.next_ephemeral_port = if candidate == EPHEMERAL_PORT_END {
                EPHEMERAL_PORT_START
            } else {
                candidate + 1
            };
            if !self
                .sockets
                .values()
                .any(|socket| socket.bound.is_some_and(|bound| bound.port == candidate))
            {
                return Ok(candidate);
            }
        }
        Err(SocketError::NoBufferSpace)
    }
}

fn is_loopback(address: u32) -> bool {
    address & 0xff == 127
}

fn addresses_overlap(a: u32, b: u32) -> bool {
    a == INADDR_ANY || b == INADDR_ANY || a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_udp_exchanges_datagrams_between_connected_sockets() {
        let mut network = WinsockState::default();
        network.startup(0x0101).unwrap();
        let server = network
            .create_socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
            .unwrap();
        let client = network
            .create_socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
            .unwrap();
        network
            .bind(
                server,
                SockAddrV4 {
                    address: INADDR_ANY,
                    port: 27_960,
                },
            )
            .unwrap();
        network
            .connect(
                client,
                SockAddrV4 {
                    address: IPV4_LOOPBACK,
                    port: 27_960,
                },
            )
            .unwrap();

        assert_eq!(network.send(client, b"connect").unwrap(), 7);
        let request = network.receive_from(server).unwrap();
        assert_eq!(request.data, b"connect");
        assert_eq!(request.source.address, IPV4_LOOPBACK);
        assert!((EPHEMERAL_PORT_START..=EPHEMERAL_PORT_END).contains(&request.source.port));

        network.send_to(server, b"accept", request.source).unwrap();
        assert_eq!(network.receive(client).unwrap().data, b"accept");
        assert_eq!(network.receive(client), Err(SocketError::WouldBlock));
    }

    #[test]
    fn external_addresses_are_not_forwarded_from_the_loopback_stack() {
        let mut network = WinsockState::default();
        network.startup(0x0101).unwrap();
        let socket = network
            .create_socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
            .unwrap();
        assert_eq!(
            network.send_to(
                socket,
                b"no external network",
                SockAddrV4 {
                    address: u32::from_le_bytes([192, 0, 2, 1]),
                    port: 27_960,
                },
            ),
            Err(SocketError::NetworkUnreachable)
        );
    }

    #[test]
    fn last_error_is_isolated_between_guest_threads() {
        let mut network = WinsockState::default();
        network.set_last_error_for_thread(0, 10_035);
        network.set_last_error_for_thread(1, 10_049);
        assert_eq!(network.last_error_for_thread(0), 10_035);
        assert_eq!(network.last_error_for_thread(1), 10_049);
        assert_eq!(network.last_error_for_thread(2), 0);
    }

    #[test]
    fn startup_negotiates_supported_versions_and_reference_counts_cleanup() {
        let mut network = WinsockState::default();
        assert_eq!(
            network.create_socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP),
            Err(SocketError::NotInitialized)
        );
        network.startup(0x0101).unwrap();
        network.startup(0x0202).unwrap();
        assert!(network.initialized());
        network.cleanup().unwrap();
        assert!(network.initialized());
        network.cleanup().unwrap();
        assert!(!network.initialized());
        assert_eq!(
            network.startup(0x0300),
            Err(SocketError::UnsupportedVersion)
        );
    }
}
