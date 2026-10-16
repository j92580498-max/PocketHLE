//! Bluetooth Classic RFCOMM bridge. Host operations never run on the guest CPU.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

pub const WOULD_BLOCK: u32 = 10035;
pub const NOT_READY: u32 = 10091;
pub const CANCELLED: u32 = 995;
pub type BtResult<T> = Result<T, u32>;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Device {
    pub address: u64,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct PortParams {
    pub server: bool,
    pub address: u64,
    pub channel: u32,
    pub uuid: [u8; 16],
    pub flags: u32,
}
impl PortParams {
    /// UUID bytes use GUID memory order, as PORTEMUPortParams does on ARM.
    pub fn service_uuid(&self) -> [u8; 16] {
        if self.uuid.iter().any(|b| *b != 0) {
            return self.uuid;
        }
        // Stable PocketHLE service per guest channel; Windows and Android agree.
        let mut id = [
            0x50, 0x48, 0x4c, 0x45, 0x42, 0x54, 0x41, 0x42, 0x91, 0x21, 0x47, 0x49, 0x5a, 0x00,
            0x00, 0x00,
        ];
        id[15] = self.channel as u8;
        id
    }
}

pub trait Stream: Send {
    /// Return WOULD_BLOCK while connecting or when no bytes are available.
    /// Zero is reserved for a real peer EOF.
    fn read(&mut self, bytes: &mut [u8]) -> BtResult<usize>;
    fn write(&mut self, bytes: &[u8]) -> BtResult<usize>;
}
/// Neutral socket address. Guest ARM padding never crosses into host FFI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SocketAddress {
    pub address: u64,
    pub uuid: [u8; 16],
    pub port: u32,
}
pub trait RfcommSocket: Send {
    fn bind(&mut self, addr: &SocketAddress) -> BtResult<()>;
    fn listen(&mut self, backlog: i32) -> BtResult<()>;
    fn connect(&mut self, addr: &SocketAddress) -> BtResult<()>;
    fn accept(&mut self) -> BtResult<(Box<dyn RfcommSocket>, SocketAddress)>;
    fn read(&mut self, bytes: &mut [u8], flags: i32) -> BtResult<usize>;
    fn write(&mut self, bytes: &[u8], flags: i32) -> BtResult<usize>;
    fn readiness(&mut self) -> BtResult<(bool, bool, bool)>;
    fn available(&mut self) -> BtResult<u32>;
    fn set_option(&mut self, level: i32, name: i32, value: &[u8]) -> BtResult<()>;
    fn get_option(&mut self, level: i32, name: i32, size: usize) -> BtResult<Vec<u8>>;
    fn address(&mut self, peer: bool) -> BtResult<SocketAddress>;
    fn shutdown(&mut self, how: i32) -> BtResult<()>;
}
pub struct GuestSocket {
    pub native: Box<dyn RfcommSocket>,
    pub nonblocking: bool,
    pub connecting: bool,
    pub recv_timeout_ms: u32,
    pub send_timeout_ms: u32,
}
pub trait Backend: Send + Sync {
    fn ready(&self) -> BtResult<()> {
        self.hostname().map(|_| ())
    }
    fn socket(&self) -> BtResult<Box<dyn RfcommSocket>> {
        Err(10045)
    }
    fn hostname(&self) -> BtResult<String>;
    fn scan(&self) -> BtResult<Vec<Device>>;
    /// Return promptly. Connection/accept and I/O progress in Stream methods.
    fn open(&self, params: &PortParams) -> BtResult<Box<dyn Stream>>;
}

static HOST: OnceLock<RwLock<Option<Arc<dyn Backend>>>> = OnceLock::new();
pub fn install_host(backend: Arc<dyn Backend>) {
    *HOST.get_or_init(|| RwLock::new(None)).write().unwrap() = Some(backend);
}
fn host() -> Option<Arc<dyn Backend>> {
    let slot = HOST.get_or_init(|| RwLock::new(None));
    #[cfg(windows)]
    {
        let mut h = slot.write().unwrap();
        if h.is_none() {
            *h = Some(Arc::new(windows::WindowsBackend));
        }
    }
    slot.read().unwrap().clone()
}

pub struct Port {
    stream: Mutex<Option<Box<dyn Stream>>>,
    rx: Mutex<VecDeque<u8>>,
    pub mask: std::sync::atomic::AtomicU32,
}
impl Port {
    pub fn new(stream: Box<dyn Stream>) -> Self {
        Self {
            stream: Mutex::new(Some(stream)),
            rx: Mutex::new(VecDeque::new()),
            mask: std::sync::atomic::AtomicU32::new(0),
        }
    }
    pub fn close(&self) {
        self.stream.lock().unwrap().take();
    }
    pub fn read(&self, bytes: &mut [u8]) -> BtResult<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut stream = self.stream.lock().unwrap();
        let stream = stream.as_mut().ok_or(CANCELLED)?;
        let mut rx = self.rx.lock().unwrap();
        if !rx.is_empty() {
            let n = bytes.len().min(rx.len());
            for byte in &mut bytes[..n] {
                *byte = rx.pop_front().unwrap();
            }
            return Ok(n);
        }
        stream.read(bytes)
    }
    pub fn write(&self, bytes: &[u8]) -> BtResult<usize> {
        self.stream
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(CANCELLED)?
            .write(bytes)
    }
    pub fn readable(&self) -> BtResult<bool> {
        let mut stream = self.stream.lock().unwrap();
        let stream = stream.as_mut().ok_or(CANCELLED)?;
        let mut rx = self.rx.lock().unwrap();
        if !rx.is_empty() {
            return Ok(true);
        }
        let mut b = [0; 4096];
        match stream.read(&mut b) {
            Ok(0) => Err(10054),
            Ok(n) => {
                rx.extend(&b[..n]);
                Ok(true)
            }
            Err(WOULD_BLOCK) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[derive(Clone)]
pub struct Service(Arc<Mutex<ServiceInner>>);
struct ServiceInner {
    backend: Option<Arc<dyn Backend>>,
    allowed: bool,
    enabled: bool,
    ports: HashMap<u32, (u32, Arc<Port>)>,
    next: u32,
    sockets: Vec<std::sync::Weak<Mutex<Option<Box<dyn RfcommSocket>>>>>,
}
impl Default for Service {
    fn default() -> Self {
        Self::with_optional_backend(host())
    }
}
impl Service {
    pub fn offline() -> Self {
        Self::with_optional_backend(None)
    }
    fn with_optional_backend(backend: Option<Arc<dyn Backend>>) -> Self {
        Self(Arc::new(Mutex::new(ServiceInner {
            backend,
            allowed: false,
            enabled: false,
            ports: HashMap::new(),
            next: 0xb7000000,
            sockets: Vec::new(),
        })))
    }
    pub fn with_backend(backend: Arc<dyn Backend>) -> Self {
        Self::with_optional_backend(Some(backend))
    }
    pub fn is_enabled(&self) -> bool {
        self.0.lock().unwrap().enabled
    }
    pub fn set_allowed(&self, allowed: bool) {
        self.0.lock().unwrap().allowed = allowed;
        if !allowed {
            self.enable(false);
        }
    }
    pub fn enable(&self, enabled: bool) {
        let mut s = self.0.lock().unwrap();
        s.enabled = enabled && s.allowed;
        if !s.enabled {
            for (_, port) in s.ports.values() {
                port.close();
            }
            s.ports.clear();
            for socket in s.sockets.drain(..).filter_map(|weak| weak.upgrade()) {
                socket.lock().unwrap().take();
            }
        }
    }
    /// Chicane starts the CE BTH1 driver before opening Winsock sockets.
    /// The guest cannot override the user's hardware permission gate.
    pub fn start_driver(&self) -> BtResult<()> {
        let backend = {
            let s = self.0.lock().unwrap();
            if !s.allowed {
                return Err(5);
            }
            s.backend.clone().ok_or(NOT_READY)?
        };
        backend.ready()?;
        let mut s = self.0.lock().unwrap();
        if !s.allowed {
            return Err(5);
        }
        s.enabled = true;
        Ok(())
    }
    pub fn backend(&self) -> BtResult<Arc<dyn Backend>> {
        self.0.lock().unwrap().backend.clone().ok_or(NOT_READY)
    }
    pub fn active_backend(&self) -> BtResult<Arc<dyn Backend>> {
        let s = self.0.lock().unwrap();
        if !s.enabled {
            return Err(NOT_READY);
        }
        s.backend.clone().ok_or(NOT_READY)
    }
    pub fn socket(&self) -> BtResult<Box<dyn RfcommSocket>> {
        let native = self.active_backend()?.socket()?;
        self.manage_socket(native)
    }
    fn manage_socket(&self, native: Box<dyn RfcommSocket>) -> BtResult<Box<dyn RfcommSocket>> {
        let mut service = self.0.lock().unwrap();
        if !service.enabled {
            return Err(NOT_READY);
        }
        let inner = Arc::new(Mutex::new(Some(native)));
        service.sockets.retain(|weak| weak.strong_count() != 0);
        service.sockets.push(Arc::downgrade(&inner));
        Ok(Box::new(ManagedSocket {
            inner,
            service: self.clone(),
        }))
    }
    pub fn register(&self, index: u32, params: &PortParams) -> BtResult<u32> {
        if !(1..=9).contains(&index) || params.channel > 30 || params.flags & !15 != 0 {
            return Err(87);
        }
        let backend = self.active_backend()?;
        let mut s = self.0.lock().unwrap();
        if s.ports.contains_key(&index) {
            return Err(183);
        }
        let port = Arc::new(Port::new(backend.open(params)?));
        let handle = s.next;
        s.next = s.next.checked_add(1).ok_or(8u32)?;
        s.ports.insert(index, (handle, port));
        Ok(handle)
    }
    pub fn port(&self, index: u32) -> Option<Arc<Port>> {
        self.0
            .lock()
            .unwrap()
            .ports
            .get(&index)
            .map(|(_, p)| p.clone())
    }
    pub fn registered_port(&self, index: u32) -> Option<(u32, Arc<Port>)> {
        self.0.lock().unwrap().ports.get(&index).cloned()
    }
    pub fn deregister(&self, handle: u32) -> bool {
        let mut s = self.0.lock().unwrap();
        let index = s
            .ports
            .iter()
            .find(|(_, (h, _))| *h == handle)
            .map(|(i, _)| *i);
        if let Some(index) = index {
            if let Some((_, p)) = s.ports.remove(&index) {
                p.close();
            }
            true
        } else {
            false
        }
    }
}

/// Weak registration lets disabling the shared Bluetooth service close every
/// native socket, including sockets in other guest processes, without cycles.
struct ManagedSocket {
    inner: Arc<Mutex<Option<Box<dyn RfcommSocket>>>>,
    service: Service,
}
impl ManagedSocket {
    fn with<T>(&mut self, f: impl FnOnce(&mut dyn RfcommSocket) -> BtResult<T>) -> BtResult<T> {
        let mut guard = self.inner.lock().unwrap();
        f(guard.as_mut().ok_or(10050u32)?.as_mut())
    }
}
impl RfcommSocket for ManagedSocket {
    fn bind(&mut self, a: &SocketAddress) -> BtResult<()> {
        self.with(|s| s.bind(a))
    }
    fn listen(&mut self, n: i32) -> BtResult<()> {
        self.with(|s| s.listen(n))
    }
    fn connect(&mut self, a: &SocketAddress) -> BtResult<()> {
        self.with(|s| s.connect(a))
    }
    fn accept(&mut self) -> BtResult<(Box<dyn RfcommSocket>, SocketAddress)> {
        let (native, addr) = self.with(|s| s.accept())?;
        Ok((self.service.manage_socket(native)?, addr))
    }
    fn read(&mut self, b: &mut [u8], flags: i32) -> BtResult<usize> {
        self.with(|s| s.read(b, flags))
    }
    fn write(&mut self, b: &[u8], flags: i32) -> BtResult<usize> {
        self.with(|s| s.write(b, flags))
    }
    fn readiness(&mut self) -> BtResult<(bool, bool, bool)> {
        self.with(|s| s.readiness())
    }
    fn available(&mut self) -> BtResult<u32> {
        self.with(|s| s.available())
    }
    fn set_option(&mut self, l: i32, n: i32, b: &[u8]) -> BtResult<()> {
        self.with(|s| s.set_option(l, n, b))
    }
    fn get_option(&mut self, l: i32, n: i32, size: usize) -> BtResult<Vec<u8>> {
        self.with(|s| s.get_option(l, n, size))
    }
    fn address(&mut self, peer: bool) -> BtResult<SocketAddress> {
        self.with(|s| s.address(peer))
    }
    fn shutdown(&mut self, how: i32) -> BtResult<()> {
        self.with(|s| s.shutdown(how))
    }
}

pub struct Lookup {
    pub result: Arc<Mutex<Option<BtResult<Vec<Device>>>>>,
    pub index: usize,
}
pub struct PendingWrite {
    pub handle: u32,
    pub pointer: u32,
    pub bytes: Vec<u8>,
    pub offset: usize,
}
#[derive(Default)]
pub struct State {
    pub service: Service,
    pub startups: u32,
    pub lookups: HashMap<u32, Lookup>,
    pub next_lookup: u32,
    pub writes: HashMap<(usize, u32, u32), PendingWrite>,
    pub sockets: HashMap<u32, GuestSocket>,
    pub next_socket: u32,
    pub socket_deadlines: HashMap<(usize, u32, u32), std::time::Instant>,
}
impl State {
    pub fn insert_socket(
        &mut self,
        native: Box<dyn RfcommSocket>,
        nonblocking: bool,
    ) -> BtResult<u32> {
        let id = self.next_socket.max(0xb7200000);
        self.next_socket = id.checked_add(1).ok_or(10055u32)?;
        self.sockets.insert(
            id,
            GuestSocket {
                native,
                nonblocking,
                connecting: false,
                recv_timeout_ms: 0,
                send_timeout_ms: 0,
            },
        );
        Ok(id)
    }
    pub fn begin_lookup(&mut self) -> BtResult<u32> {
        let backend = self.service.active_backend()?;
        let result = Arc::new(Mutex::new(None));
        let output = result.clone();
        std::thread::Builder::new()
            .name("pockethle-bt-discovery".into())
            .spawn(move || {
                *output.lock().unwrap() = Some(backend.scan());
            })
            .map_err(|_| 8u32)?;
        let id = self.next_lookup.max(0xb7100000);
        self.next_lookup = id.checked_add(1).ok_or(8u32)?;
        self.lookups.insert(id, Lookup { result, index: 0 });
        Ok(id)
    }
}

#[cfg(windows)]
mod windows;
