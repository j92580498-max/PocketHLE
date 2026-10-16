//! Native WinHTTP, asynchronous request handles with callback-owned buffers.
//! Cancel wakes the worker; only that worker closes the async request, after
//! API initiation returns. Context survives until HANDLE_CLOSING notification.
use super::*;
use std::{
    cell::UnsafeCell,
    ffi::c_void,
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};
use windows_sys::Win32::{Foundation::GetLastError, Networking::WinHttp::*};
pub struct WindowsBackend;
struct Native(usize);
impl Drop for Native {
    fn drop(&mut self) {
        unsafe {
            WinHttpCloseHandle(self.0 as *mut c_void);
        }
    }
}
struct WindowsClient(Arc<Native>);
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn native(p: *mut c_void) -> Result<Arc<Native>> {
    if p.is_null() {
        Err(unsafe { GetLastError() })
    } else {
        Ok(Arc::new(Native(p as usize)))
    }
}
fn ok(b: i32) -> Result<()> {
    if b == 0 {
        Err(unsafe { GetLastError() })
    } else {
        Ok(())
    }
}
impl Backend for WindowsBackend {
    fn open(&self, spec: &SessionSpec) -> Result<Arc<dyn Client>> {
        unsafe {
            let agent = wide(&spec.agent);
            let proxy = wide(&spec.proxy);
            let bypass = wide(&spec.bypass);
            let access = match spec.access {
                0 => WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                1 => WINHTTP_ACCESS_TYPE_NO_PROXY,
                3 => WINHTTP_ACCESS_TYPE_NAMED_PROXY,
                _ => return Err(87),
            };
            let h = native(WinHttpOpen(
                agent.as_ptr(),
                access,
                if spec.proxy.is_empty() {
                    std::ptr::null()
                } else {
                    proxy.as_ptr()
                },
                if spec.bypass.is_empty() {
                    std::ptr::null()
                } else {
                    bypass.as_ptr()
                },
                WINHTTP_FLAG_ASYNC,
            ))?;
            ok(WinHttpSetTimeouts(h.0 as _, 30000, 30000, 30000, 30000))?;
            Ok(Arc::new(WindowsClient(h)))
        }
    }
}
impl Client for WindowsClient {
    fn start(&self, spec: RequestSpec, body: Vec<u8>) -> Result<Box<dyn Transfer>> {
        let channel = Channel::default();
        let output = channel.clone();
        let client = self.0.clone();
        std::thread::Builder::new()
            .name("pockethle-http".into())
            .spawn(move || {
                let result = unsafe { request(client, spec, body, &output) };
                output.finish(result);
            })
            .map_err(|_| 8u32)?;
        Ok(channel.transfer(|| {}))
    }
}
enum Event {
    Sent,
    Headers,
    Read(u32),
    Error(u32),
}
struct Context {
    events: mpsc::Sender<Event>,
    buffer: UnsafeCell<[u8; 8192]>,
    body: Vec<u8>,
    _client: Arc<Native>,
    _connection: Arc<Native>,
}
// Exactly one ReadData is outstanding. Native writes finish before READ_COMPLETE
// is sent over mpsc; the worker reads only after receiving that completion.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}
unsafe extern "system" fn callback(
    _: *mut c_void,
    ctx: usize,
    status: u32,
    info: *mut c_void,
    length: u32,
) {
    if ctx == 0 {
        return;
    }
    let ptr = ctx as *const Context;
    if status == WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING {
        drop(Arc::from_raw(ptr));
        return;
    }
    let c = &*ptr;
    let event = match status {
        WINHTTP_CALLBACK_STATUS_SENDREQUEST_COMPLETE => Event::Sent,
        WINHTTP_CALLBACK_STATUS_HEADERS_AVAILABLE => Event::Headers,
        WINHTTP_CALLBACK_STATUS_READ_COMPLETE => Event::Read(length),
        WINHTTP_CALLBACK_STATUS_REQUEST_ERROR
            if !info.is_null()
                && length as usize >= std::mem::size_of::<WINHTTP_ASYNC_RESULT>() =>
        {
            Event::Error((*(info as *const WINHTTP_ASYNC_RESULT)).dwError)
        }
        _ => return,
    };
    let _ = c.events.send(event);
}
struct RequestHandle(usize);
impl Drop for RequestHandle {
    fn drop(&mut self) {
        unsafe {
            WinHttpCloseHandle(self.0 as _);
        }
    }
}
fn wait(rx: &mpsc::Receiver<Event>, channel: &Channel, expected: u8) -> Result<u32> {
    let until = Instant::now() + Duration::from_secs(35);
    loop {
        if channel.cancelled() {
            return Err(12017);
        }
        if Instant::now() >= until {
            return Err(12002);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Error(e)) => {
                return Err(match e {
                    12175 => 12045,
                    995 => 12017,
                    _ => e,
                })
            }
            Ok(Event::Sent) if expected == 0 => return Ok(0),
            Ok(Event::Headers) if expected == 1 => return Ok(0),
            Ok(Event::Read(n)) if expected == 2 => return Ok(n),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return Err(12030),
        }
    }
}
unsafe fn request(
    client: Arc<Native>,
    spec: RequestSpec,
    body: Vec<u8>,
    channel: &Channel,
) -> Result<()> {
    if channel.cancelled() {
        return Err(12017);
    }
    let server = wide(&spec.server);
    let connection = native(WinHttpConnect(client.0 as _, server.as_ptr(), spec.port, 0))?;
    let method = wide(&spec.method);
    let path = wide(&spec.path);
    let version = wide(&spec.version);
    let handle = WinHttpOpenRequest(
        connection.0 as _,
        method.as_ptr(),
        path.as_ptr(),
        version.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        if spec.secure { WINHTTP_FLAG_SECURE } else { 0 },
    );
    if handle.is_null() {
        return Err(GetLastError());
    }
    let handle = RequestHandle(handle as usize);
    let raw = handle.0 as *mut c_void;
    let disable = (if spec.flags & 0x00200000 != 0 {
        WINHTTP_DISABLE_REDIRECTS
    } else {
        0
    }) | (if spec.flags & 0x00080000 != 0 {
        WINHTTP_DISABLE_COOKIES
    } else {
        0
    }) | (if spec.flags & 0x00040000 != 0 {
        WINHTTP_DISABLE_AUTHENTICATION
    } else {
        0
    });
    if disable != 0 {
        ok(WinHttpSetOption(
            raw,
            WINHTTP_OPTION_DISABLE_FEATURE,
            &disable as *const _ as _,
            4,
        ))?;
    }
    if !spec.user.is_empty() && spec.flags & 0x00040000 == 0 {
        let user = wide(&spec.user);
        let password = wide(&spec.password);
        ok(WinHttpSetCredentials(
            raw,
            WINHTTP_AUTH_TARGET_SERVER,
            WINHTTP_AUTH_SCHEME_BASIC,
            user.as_ptr(),
            password.as_ptr(),
            std::ptr::null_mut(),
        ))?;
    }
    let headers = wide(
        &spec
            .headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}\r\n"))
            .collect::<String>(),
    );
    let (tx, rx) = mpsc::channel();
    let context = Arc::new(Context {
        events: tx,
        buffer: UnsafeCell::new([0; 8192]),
        body,
        _client: client,
        _connection: connection,
    });
    let flags = WINHTTP_CALLBACK_FLAG_SENDREQUEST_COMPLETE
        | WINHTTP_CALLBACK_FLAG_HEADERS_AVAILABLE
        | WINHTTP_CALLBACK_FLAG_READ_COMPLETE
        | WINHTTP_CALLBACK_FLAG_REQUEST_ERROR
        | WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING;
    let previous = WinHttpSetStatusCallback(raw, Some(callback), flags, 0);
    if previous.map(|f| f as usize) == Some(usize::MAX) {
        return Err(GetLastError());
    }
    let owned = Arc::into_raw(context.clone()) as usize;
    if WinHttpSetOption(
        raw,
        WINHTTP_OPTION_CONTEXT_VALUE,
        &owned as *const _ as _,
        std::mem::size_of::<usize>() as u32,
    ) == 0
    {
        drop(Arc::from_raw(owned as *const Context));
        return Err(GetLastError());
    }
    // HANDLE_CLOSING now releases the callback's Arc. Its buffers stay alive
    // even if the worker exits while a native asynchronous operation is pending.
    ok(WinHttpSendRequest(
        raw,
        headers.as_ptr(),
        (headers.len() - 1) as u32,
        if context.body.is_empty() {
            std::ptr::null()
        } else {
            context.body.as_ptr().cast()
        },
        context.body.len() as u32,
        context.body.len() as u32,
        owned,
    ))?;
    wait(&rx, channel, 0)?;
    ok(WinHttpReceiveResponse(raw, std::ptr::null_mut()))?;
    wait(&rx, channel, 1)?;
    let mut size = 0u32;
    WinHttpQueryHeaders(
        raw,
        WINHTTP_QUERY_RAW_HEADERS_CRLF,
        std::ptr::null(),
        std::ptr::null_mut(),
        &mut size,
        std::ptr::null_mut(),
    );
    if GetLastError() != 122 {
        return Err(GetLastError());
    }
    if size > 131072 || size % 2 != 0 {
        return Err(12150);
    }
    let mut text = vec![0u16; size as usize / 2];
    ok(WinHttpQueryHeaders(
        raw,
        WINHTTP_QUERY_RAW_HEADERS_CRLF,
        std::ptr::null(),
        text.as_mut_ptr().cast(),
        &mut size,
        std::ptr::null_mut(),
    ))?;
    let text = String::from_utf16_lossy(&text[..size as usize / 2]);
    let mut lines = text.trim_end_matches('\0').split("\r\n");
    let first = lines.next().ok_or(12150u32)?;
    let mut parts = first.splitn(3, ' ');
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();
    let status = parts
        .next()
        .ok_or(12150u32)?
        .parse::<u32>()
        .map_err(|_| 12150u32)?;
    let reason = parts.next().unwrap_or("").to_string();
    let headers = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        })
        .collect();
    channel.publish(ResponseHead {
        status,
        version,
        reason,
        headers,
    });
    loop {
        if channel.cancelled() {
            return Err(12017);
        }
        ok(WinHttpReadData(
            raw,
            (*context.buffer.get()).as_mut_ptr().cast(),
            8192,
            std::ptr::null_mut(),
        ))?;
        let n = wait(&rx, channel, 2)?;
        if n == 0 {
            break;
        }
        if n > 8192 {
            return Err(12150);
        }
        channel.push(&(&*context.buffer.get())[..n as usize])?;
    }
    Ok(())
}
