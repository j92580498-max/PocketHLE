//! Windows Bluetooth Classic using the Microsoft Bluetooth/Winsock stack.
use super::*;
use std::mem::size_of;
use std::time::{Duration, Instant};
use windows_sys::core::GUID;
use windows_sys::Win32::{Devices::Bluetooth::*, Foundation::GetLastError, Networking::WinSock::*};

pub struct WindowsBackend;
fn error() -> u32 {
    unsafe { WSAGetLastError() as u32 }
}
fn init() -> BtResult<()> {
    static INIT: OnceLock<BtResult<()>> = OnceLock::new();
    *INIT.get_or_init(|| unsafe {
        let mut data = WSADATA::default();
        let code = WSAStartup(0x0202, &mut data);
        if code == 0 {
            Ok(())
        } else {
            Err(code as u32)
        }
    })
}
fn uuid(params: &PortParams) -> GUID {
    let b = params.service_uuid();
    GUID {
        data1: u32::from_le_bytes(b[..4].try_into().unwrap()),
        data2: u16::from_le_bytes(b[4..6].try_into().unwrap()),
        data3: u16::from_le_bytes(b[6..8].try_into().unwrap()),
        data4: b[8..].try_into().unwrap(),
    }
}
fn nonblocking(s: SOCKET) -> BtResult<()> {
    let mut yes = 1u32;
    if unsafe { ioctlsocket(s, FIONBIO, &mut yes) } == 0 {
        Ok(())
    } else {
        Err(error())
    }
}
struct Socket(SOCKET);
impl Drop for Socket {
    fn drop(&mut self) {
        unsafe {
            closesocket(self.0);
        }
    }
}
impl Backend for WindowsBackend {
    fn ready(&self) -> BtResult<()> {
        init()?;
        radio_for(0).map(|_| ())
    }
    fn socket(&self) -> BtResult<Box<dyn RfcommSocket>> {
        init()?;
        let raw = unsafe { socket(AF_BTH as i32, SOCK_STREAM, BTHPROTO_RFCOMM as i32) };
        if raw == INVALID_SOCKET {
            return Err(error());
        }
        let owned = Socket(raw);
        nonblocking(raw)?;
        let yes = 1i32;
        status(unsafe {
            setsockopt(
                raw,
                SOL_RFCOMM as i32,
                SO_BTH_AUTHENTICATE as i32,
                (&yes as *const i32).cast(),
                4,
            )
        })?;
        Ok(Box::new(NativeSocket::new(owned)))
    }
    fn hostname(&self) -> BtResult<String> {
        init()?;
        Ok(std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PocketHLE".into()))
    }
    fn scan(&self) -> BtResult<Vec<Device>> {
        init()?;
        unsafe {
            let mut radio = std::ptr::null_mut();
            let p = BLUETOOTH_FIND_RADIO_PARAMS {
                dwSize: size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
            };
            let find_radio = BluetoothFindFirstRadio(&p, &mut radio);
            if find_radio.is_null() {
                return Err(NOT_READY);
            }
            BluetoothFindRadioClose(find_radio);
            let search = BLUETOOTH_DEVICE_SEARCH_PARAMS {
                dwSize: size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
                fReturnAuthenticated: 1,
                fReturnRemembered: 1,
                fReturnUnknown: 1,
                fReturnConnected: 1,
                fIssueInquiry: 1,
                cTimeoutMultiplier: 4,
                hRadio: radio,
            };
            let mut info = BLUETOOTH_DEVICE_INFO {
                dwSize: size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
                ..Default::default()
            };
            let find = BluetoothFindFirstDevice(&search, &mut info);
            if find.is_null() {
                let e = GetLastError();
                windows_sys::Win32::Foundation::CloseHandle(radio);
                return if e == 259 { Ok(Vec::new()) } else { Err(e) };
            }
            let mut devices = Vec::new();
            loop {
                let n = info
                    .szName
                    .iter()
                    .position(|b| *b == 0)
                    .unwrap_or(info.szName.len());
                devices.push(Device {
                    address: info.Address.Anonymous.ullLong,
                    name: String::from_utf16_lossy(&info.szName[..n]),
                });
                info.dwSize = size_of::<BLUETOOTH_DEVICE_INFO>() as u32;
                if BluetoothFindNextDevice(find, &mut info) == 0 {
                    break;
                }
            }
            BluetoothFindDeviceClose(find);
            windows_sys::Win32::Foundation::CloseHandle(radio);
            devices.sort_by_key(|d| d.address);
            devices.dedup_by_key(|d| d.address);
            Ok(devices)
        }
    }
    fn open(&self, params: &PortParams) -> BtResult<Box<dyn Stream>> {
        init()?;
        let socket = unsafe { socket(AF_BTH as i32, SOCK_STREAM, BTHPROTO_RFCOMM as i32) };
        if socket == INVALID_SOCKET {
            return Err(error());
        }
        let owned = Socket(socket);
        nonblocking(socket)?;
        let yes = 1i32;
        if unsafe {
            setsockopt(
                socket,
                SOL_RFCOMM as i32,
                SO_BTH_AUTHENTICATE as i32,
                (&yes as *const i32).cast(),
                4,
            )
        } != 0
        {
            return Err(error());
        }
        if params.flags & 8 != 0
            && unsafe {
                setsockopt(
                    socket,
                    SOL_RFCOMM as i32,
                    SO_BTH_ENCRYPT as i32,
                    (&yes as *const i32).cast(),
                    4,
                )
            } != 0
        {
            return Err(error());
        }
        let mut addr = SOCKADDR_BTH {
            addressFamily: AF_BTH,
            btAddr: if params.server { 0 } else { params.address },
            serviceClassId: uuid(params),
            port: if params.server { u32::MAX } else { 0 },
        };
        let mut stream = WindowsStream {
            socket: owned,
            peer: None,
            server: params.server,
            connected: false,
            deadline: Instant::now() + Duration::from_secs(30),
            advertisement: None,
            visibility: None,
        };
        if params.server {
            if unsafe {
                bind(
                    socket,
                    (&addr as *const SOCKADDR_BTH).cast(),
                    size_of::<SOCKADDR_BTH>() as i32,
                )
            } != 0
            {
                return Err(error());
            }
            if unsafe { listen(socket, 1) } != 0 {
                return Err(error());
            }
            let mut len = size_of::<SOCKADDR_BTH>() as i32;
            if unsafe { getsockname(socket, (&mut addr as *mut SOCKADDR_BTH).cast(), &mut len) }
                != 0
            {
                return Err(error());
            }
            let visibility = Visibility::acquire(addr.btAddr)?;
            advertise(&mut addr, &mut uuid(params), RNRSERVICE_REGISTER)?;
            stream.visibility = Some(visibility);
            stream.advertisement = Some((addr, uuid(params)));
        } else {
            let status = unsafe {
                connect(
                    socket,
                    (&addr as *const SOCKADDR_BTH).cast(),
                    size_of::<SOCKADDR_BTH>() as i32,
                )
            };
            if status == 0 {
                stream.connected = true;
            } else {
                let e = error();
                if e != WSAEWOULDBLOCK as u32 && e != WSAEINPROGRESS as u32 {
                    return Err(e);
                }
            }
        }
        Ok(Box::new(stream))
    }
}
fn advertise(
    addr: &mut SOCKADDR_BTH,
    guid: &mut GUID,
    operation: WSAESETSERVICEOP,
) -> BtResult<()> {
    let mut name: Vec<u16> = "PocketHLE RFCOMM\0".encode_utf16().collect();
    let mut cs = CSADDR_INFO {
        LocalAddr: SOCKET_ADDRESS {
            lpSockaddr: (addr as *mut SOCKADDR_BTH).cast(),
            iSockaddrLength: size_of::<SOCKADDR_BTH>() as i32,
        },
        RemoteAddr: SOCKET_ADDRESS::default(),
        iSocketType: SOCK_STREAM,
        iProtocol: BTHPROTO_RFCOMM as i32,
    };
    let query = WSAQUERYSETW {
        dwSize: size_of::<WSAQUERYSETW>() as u32,
        lpszServiceInstanceName: name.as_mut_ptr(),
        lpServiceClassId: guid,
        dwNameSpace: NS_BTH,
        dwNumberOfCsAddrs: 1,
        lpcsaBuffer: &mut cs,
        ..Default::default()
    };
    if unsafe { WSASetServiceW(&query, operation, 0) } == 0 {
        Ok(())
    } else {
        Err(error())
    }
}
struct WindowsStream {
    socket: Socket,
    peer: Option<Socket>,
    server: bool,
    connected: bool,
    deadline: Instant,
    advertisement: Option<(SOCKADDR_BTH, GUID)>,
    visibility: Option<Arc<Visibility>>,
}
impl WindowsStream {
    fn ready(&mut self) -> BtResult<SOCKET> {
        if self.server {
            if self.peer.is_none() {
                let peer =
                    unsafe { accept(self.socket.0, std::ptr::null_mut(), std::ptr::null_mut()) };
                if peer == INVALID_SOCKET {
                    return Err(error());
                }
                let owned = Socket(peer);
                nonblocking(peer)?;
                self.peer = Some(owned);
            }
            return Ok(self.peer.as_ref().unwrap().0);
        }
        if !self.connected {
            if Instant::now() >= self.deadline {
                return Err(10060);
            }
            let mut write = FD_SET::default();
            write.fd_count = 1;
            write.fd_array[0] = self.socket.0;
            let mut fail = write;
            let timeout = TIMEVAL::default();
            let count = unsafe { select(0, std::ptr::null_mut(), &mut write, &mut fail, &timeout) };
            if count == SOCKET_ERROR {
                return Err(error());
            }
            if count == 0 {
                return Err(WOULD_BLOCK);
            }
            let mut code = 0i32;
            let mut len = 4;
            if unsafe {
                getsockopt(
                    self.socket.0,
                    SOL_SOCKET,
                    SO_ERROR,
                    (&mut code as *mut i32).cast(),
                    &mut len,
                )
            } != 0
            {
                return Err(error());
            }
            if code != 0 {
                return Err(code as u32);
            }
            self.connected = true;
        }
        Ok(self.socket.0)
    }
}
impl Stream for WindowsStream {
    fn read(&mut self, bytes: &mut [u8]) -> BtResult<usize> {
        let socket = self.ready()?;
        let n = unsafe {
            recv(
                socket,
                bytes.as_mut_ptr(),
                bytes.len().min(i32::MAX as usize) as i32,
                0,
            )
        };
        if n == SOCKET_ERROR {
            Err(error())
        } else {
            Ok(n as usize)
        }
    }
    fn write(&mut self, bytes: &[u8]) -> BtResult<usize> {
        let socket = self.ready()?;
        let n = unsafe {
            send(
                socket,
                bytes.as_ptr(),
                bytes.len().min(i32::MAX as usize) as i32,
                0,
            )
        };
        if n == SOCKET_ERROR {
            Err(error())
        } else {
            Ok(n as usize)
        }
    }
}
impl Drop for WindowsStream {
    fn drop(&mut self) {
        if let Some((addr, guid)) = &mut self.advertisement {
            let _ = advertise(addr, guid, RNRSERVICE_DELETE);
        }
    }
}

struct NativeSocket {
    socket: Socket,
    bound: Option<SocketAddress>,
    advertisement: Option<(SOCKADDR_BTH, GUID)>,
    visibility: Option<Arc<Visibility>>,
}
impl NativeSocket {
    fn new(socket: Socket) -> Self {
        Self {
            socket,
            bound: None,
            advertisement: None,
            visibility: None,
        }
    }
}
impl Drop for NativeSocket {
    fn drop(&mut self) {
        if let Some((addr, guid)) = &mut self.advertisement {
            let _ = advertise(addr, guid, RNRSERVICE_DELETE);
        }
    }
}
fn socket_uuid(a: &SocketAddress) -> GUID {
    uuid(&PortParams {
        server: false,
        address: a.address,
        channel: if a.port == 0 || a.port == u32::MAX {
            1
        } else {
            a.port
        },
        uuid: a.uuid,
        flags: 0,
    })
}
// A shared lease restores the radio's prior state after the last host closes.
// Handles are stored as usize so no raw pointer needs an unsafe Send impl.
struct Radio(usize);
impl Drop for Radio {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0 as _);
        }
    }
}
fn radio_for(address: u64) -> BtResult<(Radio, u64)> {
    unsafe {
        let params = BLUETOOTH_FIND_RADIO_PARAMS {
            dwSize: size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
        };
        let mut raw = std::ptr::null_mut();
        let find = BluetoothFindFirstRadio(&params, &mut raw);
        if find.is_null() {
            return Err(NOT_READY);
        }
        loop {
            let radio = Radio(raw as usize);
            let mut info = BLUETOOTH_RADIO_INFO {
                dwSize: size_of::<BLUETOOTH_RADIO_INFO>() as u32,
                ..Default::default()
            };
            if BluetoothGetRadioInfo(raw, &mut info) == 0 {
                let found = info.address.Anonymous.ullLong;
                if address == 0 || address == found {
                    BluetoothFindRadioClose(find);
                    return Ok((radio, found));
                }
            }
            drop(radio);
            if BluetoothFindNextRadio(find, &mut raw) == 0 {
                break;
            }
        }
        BluetoothFindRadioClose(find);
        Err(NOT_READY)
    }
}
struct Visibility {
    address: u64,
}
struct RadioState {
    radio: Radio,
    discoverable: bool,
    connectable: bool,
    users: usize,
}
fn visibility_states() -> &'static Mutex<HashMap<u64, RadioState>> {
    static STATES: OnceLock<Mutex<HashMap<u64, RadioState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}
impl Visibility {
    fn acquire(address: u64) -> BtResult<Arc<Self>> {
        let (radio, address) = radio_for(address)?;
        let mut states = visibility_states().lock().unwrap();
        if let Some(state) = states.get_mut(&address) {
            state.users += 1;
            return Ok(Arc::new(Self { address }));
        }
        let h = radio.0 as _;
        let state = RadioState {
            radio,
            users: 1,
            discoverable: unsafe { BluetoothIsDiscoverable(h) } != 0,
            connectable: unsafe { BluetoothIsConnectable(h) } != 0,
        };
        if !state.connectable && unsafe { BluetoothEnableIncomingConnections(h, 1) } == 0 {
            return Err(NOT_READY);
        }
        if !state.discoverable && unsafe { BluetoothEnableDiscovery(h, 1) } == 0 {
            return Err(NOT_READY);
        }
        states.insert(address, state);
        Ok(Arc::new(Self { address }))
    }
}
impl Drop for Visibility {
    fn drop(&mut self) {
        let mut states = visibility_states().lock().unwrap();
        if let Some(state) = states.get_mut(&self.address) {
            state.users -= 1;
            if state.users == 0 {
                states.remove(&self.address);
            }
        }
    }
}
impl Drop for RadioState {
    fn drop(&mut self) {
        let h = self.radio.0 as _;
        unsafe {
            if !self.discoverable {
                BluetoothEnableDiscovery(h, 0);
            }
            if !self.connectable {
                BluetoothEnableIncomingConnections(h, 0);
            }
        }
    }
}
fn native_address(a: &SocketAddress) -> SOCKADDR_BTH {
    let b = a.uuid;
    SOCKADDR_BTH {
        addressFamily: AF_BTH,
        btAddr: a.address,
        port: a.port,
        serviceClassId: GUID {
            data1: u32::from_le_bytes(b[..4].try_into().unwrap()),
            data2: u16::from_le_bytes(b[4..6].try_into().unwrap()),
            data3: u16::from_le_bytes(b[6..8].try_into().unwrap()),
            data4: b[8..].try_into().unwrap(),
        },
    }
}
fn guest_address(a: SOCKADDR_BTH) -> SocketAddress {
    let guid = a.serviceClassId;
    let mut uuid = [0; 16];
    uuid[..4].copy_from_slice(&guid.data1.to_le_bytes());
    uuid[4..6].copy_from_slice(&guid.data2.to_le_bytes());
    uuid[6..8].copy_from_slice(&guid.data3.to_le_bytes());
    uuid[8..].copy_from_slice(&guid.data4);
    SocketAddress {
        address: a.btAddr,
        uuid,
        port: a.port,
    }
}
fn status(result: i32) -> BtResult<()> {
    if result == SOCKET_ERROR {
        Err(error())
    } else {
        Ok(())
    }
}
impl RfcommSocket for NativeSocket {
    fn bind(&mut self, addr: &SocketAddress) -> BtResult<()> {
        let mut a = native_address(addr);
        a.serviceClassId = GUID::default();
        status(unsafe {
            bind(
                self.socket.0,
                (&a as *const SOCKADDR_BTH).cast(),
                size_of::<SOCKADDR_BTH>() as i32,
            )
        })?;
        self.bound = Some(addr.clone());
        Ok(())
    }
    fn listen(&mut self, backlog: i32) -> BtResult<()> {
        status(unsafe { listen(self.socket.0, backlog) })?;
        if self.advertisement.is_some() {
            return Ok(());
        }
        let mut a = SOCKADDR_BTH::default();
        let mut len = size_of::<SOCKADDR_BTH>() as i32;
        status(unsafe {
            getsockname(
                self.socket.0,
                (&mut a as *mut SOCKADDR_BTH).cast(),
                &mut len,
            )
        })?;
        let visibility = Visibility::acquire(a.btAddr)?;
        // bind/listen alone do not publish SDP. Android resolves a logical
        // channel through this UUID rather than assuming a physical port.
        let logical = self.bound.as_ref().ok_or(10022u32)?;
        let mut guid = socket_uuid(logical);
        advertise(&mut a, &mut guid, RNRSERVICE_REGISTER)?;
        self.advertisement = Some((a, guid));
        self.visibility = Some(visibility);
        Ok(())
    }
    fn connect(&mut self, addr: &SocketAddress) -> BtResult<()> {
        let mut a = native_address(addr);
        a.serviceClassId = socket_uuid(addr);
        a.port = 0; // SDP resolution supports Android's allocated physical channel.
        status(unsafe {
            connect(
                self.socket.0,
                (&a as *const SOCKADDR_BTH).cast(),
                size_of::<SOCKADDR_BTH>() as i32,
            )
        })
    }
    fn accept(&mut self) -> BtResult<(Box<dyn RfcommSocket>, SocketAddress)> {
        let mut a = SOCKADDR_BTH::default();
        let mut len = size_of::<SOCKADDR_BTH>() as i32;
        let raw = unsafe {
            accept(
                self.socket.0,
                (&mut a as *mut SOCKADDR_BTH).cast(),
                &mut len,
            )
        };
        if raw == INVALID_SOCKET {
            return Err(error());
        }
        let owned = Socket(raw);
        nonblocking(raw)?;
        Ok((Box::new(NativeSocket::new(owned)), guest_address(a)))
    }
    fn read(&mut self, bytes: &mut [u8], flags: i32) -> BtResult<usize> {
        let n = unsafe { recv(self.socket.0, bytes.as_mut_ptr(), bytes.len() as i32, flags) };
        if n == SOCKET_ERROR {
            Err(error())
        } else {
            Ok(n as usize)
        }
    }
    fn write(&mut self, bytes: &[u8], flags: i32) -> BtResult<usize> {
        let n = unsafe { send(self.socket.0, bytes.as_ptr(), bytes.len() as i32, flags) };
        if n == SOCKET_ERROR {
            Err(error())
        } else {
            Ok(n as usize)
        }
    }
    fn readiness(&mut self) -> BtResult<(bool, bool, bool)> {
        let mut r = FD_SET::default();
        r.fd_count = 1;
        r.fd_array[0] = self.socket.0;
        let mut w = r;
        let mut e = r;
        let timeout = TIMEVAL::default();
        let n = unsafe { select(0, &mut r, &mut w, &mut e, &timeout) };
        if n == SOCKET_ERROR {
            return Err(error());
        }
        Ok((r.fd_count != 0, w.fd_count != 0, e.fd_count != 0))
    }
    fn available(&mut self) -> BtResult<u32> {
        let mut n = 0;
        status(unsafe { ioctlsocket(self.socket.0, FIONREAD, &mut n) })?;
        Ok(n)
    }
    fn set_option(&mut self, level: i32, name: i32, value: &[u8]) -> BtResult<()> {
        status(unsafe {
            setsockopt(
                self.socket.0,
                level,
                name,
                value.as_ptr(),
                value.len() as i32,
            )
        })
    }
    fn get_option(&mut self, level: i32, name: i32, size: usize) -> BtResult<Vec<u8>> {
        let mut bytes = vec![0; size];
        let mut len = size as i32;
        status(unsafe { getsockopt(self.socket.0, level, name, bytes.as_mut_ptr(), &mut len) })?;
        if len < 0 || len as usize > size {
            return Err(10014);
        }
        bytes.truncate(len as usize);
        Ok(bytes)
    }
    fn address(&mut self, peer: bool) -> BtResult<SocketAddress> {
        let mut a = SOCKADDR_BTH::default();
        let mut len = size_of::<SOCKADDR_BTH>() as i32;
        status(unsafe {
            if peer {
                getpeername(
                    self.socket.0,
                    (&mut a as *mut SOCKADDR_BTH).cast(),
                    &mut len,
                )
            } else {
                getsockname(
                    self.socket.0,
                    (&mut a as *mut SOCKADDR_BTH).cast(),
                    &mut len,
                )
            }
        })?;
        Ok(guest_address(a))
    }
    fn shutdown(&mut self, how: i32) -> BtResult<()> {
        status(unsafe { shutdown(self.socket.0, how) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_channels_publish_the_same_service_as_android_and_sdk_com() {
        for port in [1, 2, 3, 30, 0, u32::MAX] {
            let a = SocketAddress {
                port,
                ..Default::default()
            };
            let guid = socket_uuid(&a);
            let logical = if port == 0 || port == u32::MAX {
                1
            } else {
                port
            };
            assert_eq!(guid.data1, 0x454c4850);
            assert_eq!(guid.data2, 0x5442);
            assert_eq!(guid.data3, 0x4241);
            assert_eq!(
                guid.data4,
                [0x91, 0x21, 0x47, 0x49, 0x5a, 0, 0, logical as u8]
            );
        }
        let custom = SocketAddress {
            uuid: [0x12; 16],
            port: 2,
            ..Default::default()
        };
        assert_eq!(socket_uuid(&custom).data1, 0x12121212);
    }
    #[test]
    fn host_address_layout_roundtrips_guest_guid_bytes() {
        let a = SocketAddress {
            address: 0x123456789abc,
            port: 2,
            uuid: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        };
        assert_eq!(guest_address(native_address(&a)), a);
    }
}
