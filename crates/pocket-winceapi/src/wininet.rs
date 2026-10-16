//! Windows CE WinINet HTTP/HTTPS ABI. Native work never blocks the guest CPU.
use crate::{CallCtx, WinCeDispatcher};
use pocket_cpu::{regs::ArmReg, Prot};
use pocket_kernel::{
    internet::{self as net, Connection, Handle, Request, RequestSpec, SessionSpec},
    DispatchOutcome, KernelError,
};
const MAX_TEXT: u32 = 32768;
const MAX_BODY: u32 = 8 * 1024 * 1024;
pub fn register(d: &mut WinCeDispatcher) {
    for (name, f) in [
        ("InternetOpenW", open as crate::Handler),
        ("InternetConnectW", connect),
        ("HttpOpenRequestW", open_request),
        ("HttpAddRequestHeadersW", add_headers),
        ("HttpSendRequestW", send),
        ("HttpQueryInfoW", query),
        ("InternetQueryDataAvailable", available),
        ("InternetReadFile", read),
        ("InternetCloseHandle", close),
    ] {
        d.register_handler("wininet.dll", name, f);
    }
}
fn done(ctx: &mut CallCtx<'_>, v: u32) -> Result<DispatchOutcome, KernelError> {
    crate::bluetooth::finish_wait(ctx)?;
    Ok(DispatchOutcome::ReturnedR0(v))
}
fn fail(ctx: &mut CallCtx<'_>, e: u32) -> Result<DispatchOutcome, KernelError> {
    crate::coredll::set_thread_error(ctx, e);
    done(ctx, 0)
}
fn probe(ctx: &mut CallCtx<'_>, p: u32, n: u32, prot: Prot) -> bool {
    p != 0 && p.checked_add(n).is_some() && ctx.cpu.check_guest_access(p, n, prot).is_ok()
}
fn wide(ctx: &mut CallCtx<'_>, p: u32, n: Option<u32>) -> net::Result<String> {
    if p == 0 {
        return if n.unwrap_or(0) == 0 {
            Ok(String::new())
        } else {
            Err(87)
        };
    }
    let max = n.unwrap_or(MAX_TEXT);
    if max > MAX_TEXT {
        return Err(87);
    }
    let mut data = Vec::new();
    for i in 0..max {
        let a = p.checked_add(i.checked_mul(2).ok_or(87u32)?).ok_or(87u32)?;
        if !probe(ctx, a, 2, Prot::READ) {
            return Err(87);
        }
        let v = ctx.cpu.read_mem(a, 2).map_err(|_| 87u32)?;
        let v = u16::from_le_bytes(v.try_into().unwrap());
        if n.is_none() && v == 0 {
            return String::from_utf16(&data).map_err(|_| 87u32);
        }
        if v == 0 {
            return Err(87);
        }
        data.push(v);
    }
    if n.is_none() {
        return Err(87);
    }
    String::from_utf16(&data).map_err(|_| 87)
}
fn headers(ctx: &mut CallCtx<'_>, p: u32, n: u32) -> net::Result<String> {
    if n == 0 {
        Ok(String::new())
    } else {
        wide(ctx, p, if n == u32::MAX { None } else { Some(n) })
    }
}
fn valid_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}
fn change_headers(current: &mut Vec<(String, String)>, text: &str, flags: u32) -> net::Result<()> {
    if flags & !(0xf1000000) != 0 {
        return Err(87);
    }
    let mut next = current.clone();
    for line in text.split("\r\n").filter(|s| !s.is_empty()) {
        let (k, v) = line.split_once(':').ok_or(87u32)?;
        let k = k.trim();
        let v = v.trim();
        if !valid_token(k) || v.contains(['\r', '\n', '\0']) {
            return Err(87);
        }
        let old = next
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case(k));
        if flags & 0x10000000 != 0 && old.is_some() {
            return Err(12155);
        }
        if flags & 0x80000000 != 0 {
            if old.is_none() && flags & 0x20000000 == 0 {
                return Err(12150);
            }
            next.retain(|(name, _)| !name.eq_ignore_ascii_case(k));
            if !v.is_empty() {
                next.push((k.into(), v.into()));
            }
        } else if flags & 0x41000000 != 0 && old.is_some() {
            let value = &mut next[old.unwrap()].1;
            value.push_str(if flags & 0x01000000 != 0 { "; " } else { ", " });
            value.push_str(v);
        } else {
            next.push((k.into(), v.into()));
        }
    }
    if next
        .iter()
        .map(|(k, v)| k.len() + v.len() + 4)
        .sum::<usize>()
        > 65536
    {
        return Err(8);
    }
    *current = next;
    Ok(())
}
fn open(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let agent = ctx.arg_u32(0)?;
    let access = ctx.arg_u32(1)?;
    let proxy = ctx.arg_u32(2)?;
    let bypass = ctx.arg_u32(3)?;
    let flags = ctx.arg_u32(4)?;
    if flags != 0 {
        return fail(ctx, 50);
    }
    if ![0, 1, 3].contains(&access) {
        return fail(ctx, 87);
    }
    let spec = (|| {
        Ok(SessionSpec {
            agent: if agent == 0 {
                "PocketHLE".into()
            } else {
                wide(ctx, agent, None)?
            },
            access,
            proxy: wide(ctx, proxy, None)?,
            bypass: wide(ctx, bypass, None)?,
        })
    })();
    let spec = match spec {
        Ok(s) => s,
        Err(e) => return fail(ctx, e),
    };
    if access == 3 && spec.proxy.is_empty() {
        return fail(ctx, 87);
    }
    match ctx.kernel.internet.open(spec) {
        Ok(h) => done(ctx, h),
        Err(e) => fail(ctx, e),
    }
}
fn connect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let parent = ctx.arg_u32(0)?;
    let server = ctx.arg_u32(1)?;
    let port = ctx.arg_u32(2)?;
    let user = ctx.arg_u32(3)?;
    let password = ctx.arg_u32(4)?;
    let service = ctx.arg_u32(5)?;
    let flags = ctx.arg_u32(6)?;
    match ctx.kernel.internet.handles.get(&parent) {
        Some(Handle::Session(_)) => {}
        Some(_) => return fail(ctx, 12018),
        None => return fail(ctx, 6),
    }
    if service != 3 {
        return fail(ctx, 12004);
    }
    if flags != 0 {
        return fail(ctx, 50);
    }
    if port > 65535 {
        return fail(ctx, 87);
    }
    let params = (|| {
        Ok(Connection {
            parent,
            server: wide(ctx, server, None)?,
            port: port as u16,
            user: wide(ctx, user, None)?,
            password: wide(ctx, password, None)?,
        })
    })();
    let params = match params {
        Ok(p) => p,
        Err(e) => return fail(ctx, e),
    };
    if params.server.is_empty()
        || params.server.chars().any(|c| {
            c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | '@' | '?' | '#')
        })
    {
        return fail(ctx, 12005);
    }
    match ctx.kernel.internet.insert(Handle::Connection(params)) {
        Ok(h) => done(ctx, h),
        Err(e) => fail(ctx, e),
    }
}
fn open_request(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let parent = ctx.arg_u32(0)?;
    let method = ctx.arg_u32(1)?;
    let path = ctx.arg_u32(2)?;
    let version = ctx.arg_u32(3)?;
    let referrer = ctx.arg_u32(4)?;
    let accept = ctx.arg_u32(5)?;
    let flags = ctx.arg_u32(6)?;
    let (connection, client) = match ctx.kernel.internet.handles.get(&parent) {
        Some(Handle::Connection(c)) => {
            let Some(Handle::Session(s)) = ctx.kernel.internet.handles.get(&c.parent) else {
                return fail(ctx, 6);
            };
            (
                (c.server.clone(), c.port, c.user.clone(), c.password.clone()),
                s.client.clone(),
            )
        }
        Some(_) => return fail(ctx, 12018),
        None => return fail(ctx, 6),
    };
    // Never silently bypass host certificate validation or promise async callbacks.
    if flags & 0x10003000 != 0 {
        return fail(ctx, 50);
    }
    let strings = (|| {
        Ok((
            if method == 0 {
                "GET".into()
            } else {
                wide(ctx, method, None)?
            },
            if path == 0 {
                "/".into()
            } else {
                wide(ctx, path, None)?
            },
            if version == 0 {
                "HTTP/1.0".into()
            } else {
                wide(ctx, version, None)?
            },
            wide(ctx, referrer, None)?,
        ))
    })();
    let (method, path, version, referrer) = match strings {
        Ok(v) => v,
        Err(e) => return fail(ctx, e),
    };
    if !valid_token(&method)
        || !path.starts_with('/')
        || path.contains(['\r', '\n', '\0'])
        || !["HTTP/1.0", "HTTP/1.1"].contains(&version.as_str())
    {
        return fail(ctx, 87);
    }
    let mut headers = Vec::new();
    if !referrer.is_empty() {
        if referrer.contains(['\r', '\n']) {
            return fail(ctx, 87);
        }
        headers.push(("Referer".into(), referrer));
    }
    if accept != 0 {
        let mut values = Vec::new();
        let mut ended = false;
        for i in 0..128u32 {
            let Some(at) = accept.checked_add(i * 4) else {
                return fail(ctx, 87);
            };
            if !probe(ctx, at, 4, Prot::READ) {
                return fail(ctx, 87);
            }
            let p = ctx.cpu.read_u32_le(at)?;
            if p == 0 {
                ended = true;
                break;
            }
            match wide(ctx, p, None) {
                Ok(v) => {
                    if v.contains(['\r', '\n']) {
                        return fail(ctx, 87);
                    }
                    values.push(v)
                }
                Err(e) => return fail(ctx, e),
            }
        }
        if !ended {
            return fail(ctx, 87);
        }
        if !values.is_empty() {
            headers.push(("Accept".into(), values.join(", ")));
        }
    }
    let secure = flags & 0x00800000 != 0;
    let spec = RequestSpec {
        server: connection.0,
        port: if connection.1 == 0 {
            if secure {
                443
            } else {
                80
            }
        } else {
            connection.1
        },
        secure,
        method,
        path,
        version,
        user: connection.2,
        password: connection.3,
        headers,
        flags,
    };
    match ctx.kernel.internet.insert(Handle::Request(Request {
        parent,
        client,
        spec,
        transfer: None,
        sending: None,
    })) {
        Ok(h) => done(ctx, h),
        Err(e) => fail(ctx, e),
    }
}
fn add_headers(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let flags = ctx.arg_u32(3)?;
    let text = match headers(ctx, p, n) {
        Ok(v) => v,
        Err(e) => return fail(ctx, e),
    };
    let result = ctx
        .kernel
        .internet
        .request(h)
        .and_then(|r| change_headers(&mut r.spec.headers, &text, flags));
    match result {
        Ok(()) => done(ctx, 1),
        Err(e) => fail(ctx, e),
    }
}
fn send(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let hp = ctx.arg_u32(1)?;
    let hn = ctx.arg_u32(2)?;
    let body = ctx.arg_u32(3)?;
    let length = ctx.arg_u32(4)?;
    if length > MAX_BODY || (length != 0 && !probe(ctx, body, length, Prot::READ)) {
        return fail(ctx, 87);
    }
    let key = (
        ctx.kernel.current_thread,
        ctx.thunk.thunk_va,
        ctx.cpu.read_reg(ArmReg::Sp)?,
    );
    let request = match ctx.kernel.internet.request(h) {
        Ok(r) => r,
        Err(e) => return fail(ctx, e),
    };
    let started = request.sending == Some(key);
    if request.sending.is_some() && !started {
        return fail(ctx, 12019);
    }
    if !started {
        if let Some(t) = request.transfer.as_mut() {
            match t.available() {
                Ok((0, true)) => {}
                Ok(_) => return fail(ctx, 12019),
                Err(_) => {}
            }
        }
        let text = match headers(ctx, hp, hn) {
            Ok(v) => v,
            Err(e) => return fail(ctx, e),
        };
        let bytes = if length == 0 {
            Vec::new()
        } else {
            ctx.cpu.read_mem(body, length)?
        };
        let r = ctx.kernel.internet.request(h).unwrap();
        let mut spec = r.spec.clone();
        if let Err(e) = change_headers(&mut spec.headers, &text, 0x20000000) {
            return fail(ctx, e);
        }
        let client = r.client.clone();
        let spec = ctx.kernel.internet.route_colors_request(spec);
        let r = ctx.kernel.internet.request(h).unwrap();
        match client.start(spec, bytes) {
            Ok(t) => {
                r.transfer = Some(t);
                r.sending = Some(key)
            }
            Err(e) => return fail(ctx, e),
        }
    }
    let r = ctx.kernel.internet.request(h).unwrap();
    let head = r.transfer.as_mut().unwrap().head();
    match head {
        Ok(Some(_)) => {
            r.sending = None;
            done(ctx, 1)
        }
        Ok(None) => crate::bluetooth::retry(ctx),
        Err(e) => {
            r.sending = None;
            fail(ctx, e)
        }
    }
}
fn available(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let out = ctx.arg_u32(1)?;
    let flags = ctx.arg_u32(2)?;
    if !probe(ctx, out, 4, Prot::WRITE) {
        return fail(ctx, 87);
    }
    ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
    if flags != 0 {
        return fail(ctx, 87);
    }
    let r = match ctx.kernel.internet.request(h) {
        Ok(r) => r,
        Err(e) => return fail(ctx, e),
    };
    let Some(t) = r.transfer.as_mut() else {
        return fail(ctx, 12019);
    };
    match t.available() {
        Ok((n, end)) if n != 0 || end => {
            ctx.cpu.write_mem(out, &(n as u32).to_le_bytes())?;
            done(ctx, 1)
        }
        Ok(_) => crate::bluetooth::retry(ctx),
        Err(e) => fail(ctx, e),
    }
}
fn read(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let out = ctx.arg_u32(3)?;
    if !probe(ctx, out, 4, Prot::WRITE) || (n != 0 && !probe(ctx, p, n, Prot::WRITE)) {
        return fail(ctx, 87);
    }
    ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
    let r = match ctx.kernel.internet.request(h) {
        Ok(r) => r,
        Err(e) => return fail(ctx, e),
    };
    let Some(t) = r.transfer.as_mut() else {
        return fail(ctx, 12019);
    };
    let mut bytes = vec![0u8; n.min(net::BUFFER_LIMIT as u32) as usize];
    match t.read(&mut bytes) {
        Ok(Some(got)) => {
            if got > bytes.len() {
                return fail(ctx, 12150);
            }
            if got != 0 {
                ctx.cpu.write_mem(p, &bytes[..got])?;
            }
            ctx.cpu.write_mem(out, &(got as u32).to_le_bytes())?;
            done(ctx, 1)
        }
        Ok(None) => crate::bluetooth::retry(ctx),
        Err(e) => fail(ctx, e),
    }
}
fn query(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let info = ctx.arg_u32(1)?;
    let out = ctx.arg_u32(2)?;
    let length = ctx.arg_u32(3)?;
    let index = ctx.arg_u32(4)?;
    if !probe(ctx, length, 4, Prot::READ | Prot::WRITE)
        || (index != 0 && !probe(ctx, index, 4, Prot::READ | Prot::WRITE))
    {
        return fail(ctx, 87);
    }
    let capacity = ctx.cpu.read_u32_le(length)?;
    let i = if index == 0 {
        0
    } else {
        ctx.cpu.read_u32_le(index)?
    };
    let kind = info & 0xffff;
    let custom = if kind == 65535 {
        match wide(ctx, out, None) {
            Ok(v) => Some(v),
            Err(e) => return fail(ctx, e),
        }
    } else {
        None
    };
    if info & 0x5fff0000 != 0 {
        return fail(ctx, 50);
    }
    let r = match ctx.kernel.internet.request(h) {
        Ok(r) => r,
        Err(e) => return fail(ctx, e),
    };
    let head = if info & 0x80000000 != 0 {
        net::ResponseHead {
            status: 0,
            version: r.spec.version.clone(),
            reason: String::new(),
            headers: r.spec.headers.clone(),
        }
    } else {
        let Some(t) = r.transfer.as_mut() else {
            return fail(ctx, 12019);
        };
        match t.head() {
            Ok(Some(h)) => h,
            Ok(None) => return fail(ctx, 12019),
            Err(e) => return fail(ctx, e),
        }
    };
    let named = match kind {
        1 => Some("Content-Type"),
        5 => Some("Content-Length"),
        6 => Some("Content-Language"),
        9 => Some("Date"),
        10 => Some("Expires"),
        11 => Some("Last-Modified"),
        23 => Some("Connection"),
        24 => Some("Accept"),
        25 => Some("Accept-Charset"),
        26 => Some("Accept-Encoding"),
        27 => Some("Accept-Language"),
        28 => Some("Authorization"),
        29 => Some("Content-Encoding"),
        33 => Some("Location"),
        35 => Some("Referer"),
        37 => Some("Server"),
        39 => Some("User-Agent"),
        43 => Some("Set-Cookie"),
        44 => Some("Cookie"),
        49 => Some("Cache-Control"),
        54 => Some("ETag"),
        55 => Some("Host"),
        63 => Some("Transfer-Encoding"),
        65535 => custom.as_deref(),
        _ => None,
    };
    let value = match kind {
        18 => Some(head.version.clone()),
        19 => Some(head.status.to_string()),
        20 => Some(head.reason.clone()),
        21 | 22 => {
            let mut lines = vec![if info & 0x80000000 != 0 {
                format!("{} {} {}", r.spec.method, r.spec.path, r.spec.version)
            } else {
                format!("{} {} {}", head.version, head.status, head.reason)
            }];
            lines.extend(head.headers.iter().map(|(k, v)| format!("{k}: {v}")));
            Some(if kind == 22 {
                lines.join("\r\n") + "\r\n\r\n"
            } else {
                lines.join("\0") + "\0"
            })
        }
        _ => named.and_then(|name| {
            head.headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .nth(i as usize)
                .map(|(_, v)| v.clone())
        }),
    };
    let Some(value) = value else {
        return fail(ctx, 12150);
    };
    let number = info & 0x20000000 != 0;
    let bytes = if number {
        match value.parse::<u32>() {
            Ok(v) => v.to_le_bytes().to_vec(),
            Err(_) => return fail(ctx, 12150),
        }
    } else {
        value
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    };
    if capacity < bytes.len() as u32 || out == 0 {
        ctx.cpu
            .write_mem(length, &(bytes.len() as u32).to_le_bytes())?;
        return fail(ctx, 122);
    }
    if !probe(ctx, out, bytes.len() as u32, Prot::WRITE) {
        return fail(ctx, 87);
    }
    ctx.cpu.write_mem(out, &bytes)?;
    ctx.cpu.write_mem(
        length,
        &(if number { 4 } else { bytes.len() as u32 - 2 }).to_le_bytes(),
    )?;
    if index != 0 && named.is_some() {
        ctx.cpu
            .write_mem(index, &i.saturating_add(1).to_le_bytes())?;
    }
    done(ctx, 1)
}
fn close(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    if ctx.kernel.internet.close(h) {
        done(ctx, 1)
    } else {
        fail(ctx, 6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluetooth::tests::{call, setup};
    use pocket_cpu::Cpu;
    use std::sync::{Arc, Mutex};
    struct Host {
        channel: net::Channel,
        sent: Arc<Mutex<Vec<(RequestSpec, Vec<u8>)>>>,
    }
    impl net::Backend for Host {
        fn open(&self, _: &SessionSpec) -> net::Result<Arc<dyn net::Client>> {
            Ok(Arc::new(Host {
                channel: self.channel.clone(),
                sent: self.sent.clone(),
            }))
        }
    }
    impl net::Client for Host {
        fn start(&self, s: RequestSpec, b: Vec<u8>) -> net::Result<Box<dyn net::Transfer>> {
            self.sent.lock().unwrap().push((s, b));
            Ok(self.channel.transfer(|| {}))
        }
    }
    fn value(v: DispatchOutcome) -> u32 {
        let DispatchOutcome::ReturnedR0(v) = v else {
            panic!("pending")
        };
        v
    }
    #[test]
    fn colors_post_utf16_queries_binary_fragmented_reads_and_parent_cleanup() {
        let (mut cpu, mut k, mut d) = setup();
        let channel = net::Channel::default();
        let sent = Arc::new(Mutex::new(vec![]));
        k.internet = net::State::with_backend(Arc::new(Host {
            channel: channel.clone(),
            sent: sent.clone(),
        }));
        k.internet
            .set_colors_endpoint("http://127.0.0.1:8080")
            .unwrap();
        for (p, s) in [
            (0x1000, "test"),
            (0x1040, "us.mygiz.gizmondo.com"),
            (0x1080, "POST"),
            (0x1600, "/applications/games/colors/open/command.do"),
            (
                0x1100,
                "Content-Type: application/x-www-form-urlencoded\r\n",
            ),
        ] {
            cpu.write_mem(
                p,
                &s.encode_utf16()
                    .chain(Some(0))
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        }
        macro_rules! api {
            ($name:expr,$args:expr) => {
                call(&mut cpu, &mut k, &mut d, "wininet.dll", $name, $args)
            };
        }
        let session = value(api!("InternetOpenW", &[0x1000, 1, 0, 0, 0]));
        assert_ne!(session, 0);
        let connection = value(api!(
            "InternetConnectW",
            &[session, 0x1040, 80, 0, 0, 3, 0, 0]
        ));
        let request = value(api!(
            "HttpOpenRequestW",
            &[connection, 0x1080, 0x1600, 0, 0, 0, 0x04000000, 0]
        ));
        assert_eq!(
            value(api!(
                "HttpAddRequestHeadersW",
                &[request, 0x1100, u32::MAX, 0x20000000]
            )),
            1
        );
        cpu.write_mem(0x1200, b"a=1").unwrap();
        assert_eq!(
            api!("HttpSendRequestW", &[request, 0, u32::MAX, 0x1200, 3]),
            DispatchOutcome::JumpTo(0x70000000)
        );
        assert_eq!(
            api!("HttpSendRequestW", &[request, 0, u32::MAX, 0x1200, 3]),
            DispatchOutcome::JumpTo(0x70000000)
        );
        assert_eq!(sent.lock().unwrap().len(), 1);
        channel.publish(net::ResponseHead {
            status: 404,
            version: "HTTP/1.1".into(),
            reason: "Not Found".into(),
            headers: vec![
                ("Content-Length".into(), "4".into()),
                ("Set-Cookie".into(), "a=1".into()),
                ("Set-Cookie".into(), "b=2".into()),
            ],
        });
        assert_eq!(
            value(api!("HttpSendRequestW", &[request, 0, u32::MAX, 0x1200, 3])),
            1
        );
        cpu.write_mem(0x1300, &0u32.to_le_bytes()).unwrap();
        assert_eq!(
            value(api!("HttpQueryInfoW", &[request, 5, 0, 0x1300, 0])),
            0
        );
        assert_eq!(cpu.read_u32_le(0x1300).unwrap(), 4);
        assert_eq!(
            value(call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "GetLastError",
                &[]
            )),
            122
        );
        assert_eq!(
            value(api!("HttpQueryInfoW", &[request, 5, 0x1400, 0x1300, 0])),
            1
        );
        assert_eq!(cpu.read_mem(0x1400, 4).unwrap(), [b'4', 0, 0, 0]);
        assert_eq!(cpu.read_u32_le(0x1300).unwrap(), 2);
        cpu.write_mem(0x1300, &4u32.to_le_bytes()).unwrap();
        assert_eq!(
            value(api!(
                "HttpQueryInfoW",
                &[request, 19 | 0x20000000, 0x1400, 0x1300, 0]
            )),
            1
        );
        assert_eq!(cpu.read_u32_le(0x1400).unwrap(), 404);
        assert_eq!(
            api!("InternetQueryDataAvailable", &[request, 0x1300, 0, 0]),
            DispatchOutcome::JumpTo(0x70000000)
        );
        channel.push(&[0, 255]).unwrap();
        assert_eq!(
            value(api!("InternetQueryDataAvailable", &[request, 0x1300, 0, 0])),
            1
        );
        assert_eq!(cpu.read_u32_le(0x1300).unwrap(), 2);
        assert_eq!(value(api!("InternetReadFile", &[request, 0, 2, 0x1300])), 0);
        assert_eq!(
            value(api!("InternetReadFile", &[request, 0x1400, 8, 0x1300])),
            1
        );
        assert_eq!(cpu.read_mem(0x1400, 2).unwrap(), [0, 255]);
        assert_eq!(
            api!("InternetReadFile", &[request, 0x1400, 8, 0x1300]),
            DispatchOutcome::JumpTo(0x70000000)
        );
        channel.push(b"xy").unwrap();
        channel.finish(Ok(()));
        assert_eq!(
            value(api!("InternetReadFile", &[request, 0x1400, 8, 0x1300])),
            1
        );
        assert_eq!(cpu.read_u32_le(0x1300).unwrap(), 2);
        assert_eq!(
            value(api!("InternetReadFile", &[request, 0x1400, 8, 0x1300])),
            1
        );
        assert_eq!(cpu.read_u32_le(0x1300).unwrap(), 0);
        assert_eq!(value(api!("InternetCloseHandle", &[session])), 1);
        assert!(channel.cancelled());
        assert!(k.internet.handles.is_empty());
        assert_eq!(value(api!("InternetCloseHandle", &[request])), 0);
        let sent = sent.lock().unwrap();
        assert_eq!(sent[0].1, b"a=1");
        assert_eq!(sent[0].0.server, "127.0.0.1");
        assert_eq!(sent[0].0.port, 8080);
        assert_eq!(sent[0].0.path, "/applications/games/colors/open/command.do");
        assert_eq!(sent[0].0.headers[0].0, "Content-Type");
        assert!(k.wait_deadlines.is_empty());
    }
    #[test]
    fn header_changes_are_atomic_and_case_insensitive() {
        let mut h = vec![("Accept".into(), "text/plain".into())];
        assert_eq!(change_headers(&mut h, "accept: x", 0x10000000), Err(12155));
        assert_eq!(h[0].1, "text/plain");
        assert_eq!(
            change_headers(&mut h, "Accept: x\r\nbroken", 0x80000000),
            Err(87)
        );
        assert_eq!(h[0].1, "text/plain");
        change_headers(&mut h, "accept: x", 0x40000000).unwrap();
        assert_eq!(h[0].1, "text/plain, x");
    }
}
