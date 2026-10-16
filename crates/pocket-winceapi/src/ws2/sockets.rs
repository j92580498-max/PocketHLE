//! Guest WinCE sockets: process-local IDs and cooperative blocking over native
//! nonblocking RFCOMM sockets. No guest pointers or padded ARM structs reach FFI.
use super::*;
use pocket_cpu::{regs::ArmReg, Prot};
use pocket_kernel::bluetooth::{SocketAddress, WOULD_BLOCK};
use std::time::{Duration, Instant};
type Outcome = Result<DispatchOutcome, KernelError>;

pub(super) fn register(d: &mut WinCeDispatcher) {
    for (name, handler) in [
        ("socket", socket as crate::Handler),
        ("bind", bind),
        ("listen", listen),
        ("connect", connect),
        ("accept", accept),
        ("send", send),
        ("recv", recv),
        ("closesocket", close),
        ("shutdown", shutdown),
        ("select", select),
        ("setsockopt", set_option),
        ("getsockopt", get_option),
        ("ioctlsocket", ioctl),
        ("getsockname", local_address),
        ("getpeername", peer_address),
    ] {
        d.register_handler("ws2.dll", name, handler);
    }
}
fn key(ctx: &mut CallCtx<'_>) -> Result<(usize, u32, u32), KernelError> {
    Ok((
        ctx.kernel.current_thread,
        ctx.thunk.thunk_va,
        ctx.cpu.read_reg(ArmReg::Sp)?,
    ))
}
fn done(ctx: &mut CallCtx<'_>, result: u32) -> Outcome {
    let k = key(ctx)?;
    ctx.kernel.vfs.bluetooth.socket_deadlines.remove(&k);
    crate::bluetooth::finish_wait(ctx)?;
    Ok(DispatchOutcome::ReturnedR0(result))
}
fn error(ctx: &mut CallCtx<'_>, code: u32) -> Outcome {
    done(ctx, SOCKET_ERROR)?;
    fail(ctx, code)
}
fn initialized(ctx: &mut CallCtx<'_>) -> bool {
    ctx.kernel.vfs.bluetooth.startups != 0
}
fn access(ctx: &mut CallCtx<'_>, ptr: u32, len: u32, prot: Prot) -> bool {
    len == 0
        || (ptr != 0
            && ptr.checked_add(len).is_some()
            && ctx.cpu.check_guest_access(ptr, len, prot).is_ok())
}
fn valid(ctx: &mut CallCtx<'_>, handle: u32) -> bool {
    ctx.kernel.vfs.bluetooth.sockets.contains_key(&handle)
}
fn wait(ctx: &mut CallCtx<'_>, nonblocking: bool, timeout: u32) -> Outcome {
    if nonblocking {
        return error(ctx, WOULD_BLOCK);
    }
    if timeout != 0 {
        let k = key(ctx)?;
        let deadline = *ctx
            .kernel
            .vfs
            .bluetooth
            .socket_deadlines
            .entry(k)
            .or_insert_with(|| Instant::now() + Duration::from_millis(timeout as u64));
        if Instant::now() >= deadline {
            return error(ctx, 10060);
        }
    }
    crate::bluetooth::retry(ctx)
}
fn socket(ctx: &mut CallCtx<'_>) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let family = ctx.arg_u32(0)?;
    let kind = ctx.arg_u32(1)?;
    let protocol = ctx.arg_u32(2)?;
    if family != 32 {
        return error(ctx, 10047);
    }
    if kind != 1 {
        return error(ctx, 10044);
    }
    if protocol != 3 && protocol != 0 {
        return error(ctx, 10043);
    }
    let native = match ctx.kernel.vfs.bluetooth.service.socket() {
        Ok(s) => s,
        Err(e) => return error(ctx, e),
    };
    match ctx.kernel.vfs.bluetooth.insert_socket(native, false) {
        Ok(id) => done(ctx, id),
        Err(e) => error(ctx, e),
    }
}
/// ARM/CE's normal ABI is 40 bytes (u64 at 8); also handle 4-byte-aligned
/// and packed callers, including addresses returned by legacy inquiry builds.
fn decode(bytes: &[u8]) -> Result<SocketAddress, u32> {
    let offset = if bytes.len() >= 40 {
        8
    } else if bytes.len() == 32 {
        4
    } else if bytes.len() == 30 {
        2
    } else {
        return Err(10014);
    };
    if u16::from_le_bytes(bytes[..2].try_into().unwrap()) != 32 {
        return Err(10047);
    }
    Ok(SocketAddress {
        address: u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()),
        uuid: bytes[offset + 8..offset + 24].try_into().unwrap(),
        port: u32::from_le_bytes(bytes[offset + 24..offset + 28].try_into().unwrap()),
    })
}
fn encode(a: &SocketAddress) -> [u8; 40] {
    let mut bytes = [0; 40];
    bytes[..2].copy_from_slice(&32u16.to_le_bytes());
    bytes[8..16].copy_from_slice(&a.address.to_le_bytes());
    bytes[16..32].copy_from_slice(&a.uuid);
    bytes[32..36].copy_from_slice(&a.port.to_le_bytes());
    bytes
}
fn read_address(ctx: &mut CallCtx<'_>, ptr: u32, len: u32) -> Result<SocketAddress, u32> {
    if (len as i32) < 0
        || !(len == 30 || len == 32 || len >= 40)
        || !access(ctx, ptr, len.min(40), Prot::READ)
    {
        return Err(10014);
    }
    decode(&ctx.cpu.read_mem(ptr, len.min(40)).map_err(|_| 10014u32)?)
}
fn bind(ctx: &mut CallCtx<'_>) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let h = ctx.arg_u32(0)?;
    let ptr = ctx.arg_u32(1)?;
    let len = ctx.arg_u32(2)?;
    if !valid(ctx, h) {
        return error(ctx, 10038);
    }
    let addr = match read_address(ctx, ptr, len) {
        Ok(a) => a,
        Err(e) => return error(ctx, e),
    };
    match ctx
        .kernel
        .vfs
        .bluetooth
        .sockets
        .get_mut(&h)
        .unwrap()
        .native
        .bind(&addr)
    {
        Ok(()) => done(ctx, 0),
        Err(e) => error(ctx, e),
    }
}
fn listen(ctx: &mut CallCtx<'_>) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let h = ctx.arg_u32(0)?;
    let backlog = ctx.arg_u32(1)? as i32;
    if !valid(ctx, h) {
        return error(ctx, 10038);
    }
    match ctx
        .kernel
        .vfs
        .bluetooth
        .sockets
        .get_mut(&h)
        .unwrap()
        .native
        .listen(backlog)
    {
        Ok(()) => done(ctx, 0),
        Err(e) => error(ctx, e),
    }
}
fn connect(ctx: &mut CallCtx<'_>) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let h = ctx.arg_u32(0)?;
    let ptr = ctx.arg_u32(1)?;
    let len = ctx.arg_u32(2)?;
    if !valid(ctx, h) {
        return error(ctx, 10038);
    }
    let addr = match read_address(ctx, ptr, len) {
        Ok(a) => a,
        Err(e) => return error(ctx, e),
    };
    let s = ctx.kernel.vfs.bluetooth.sockets.get_mut(&h).unwrap();
    let nonblocking = s.nonblocking;
    let timeout = 0;
    let result = if s.connecting {
        if nonblocking {
            return error(ctx, 10037);
        }
        match s.native.readiness() {
            Ok((_, w, e)) if w || e => match s.native.get_option(0xffff, 0x1007, 4) {
                Ok(bytes) if bytes.len() == 4 => {
                    let code = u32::from_le_bytes(bytes.try_into().unwrap());
                    if code == 0 {
                        Ok(())
                    } else {
                        Err(code)
                    }
                }
                Ok(_) => Err(10014),
                Err(e) => Err(e),
            },
            Ok(_) => Err(WOULD_BLOCK),
            Err(e) => Err(e),
        }
    } else {
        s.native.connect(&addr)
    };
    match result {
        Ok(()) => {
            ctx.kernel
                .vfs
                .bluetooth
                .sockets
                .get_mut(&h)
                .unwrap()
                .connecting = false;
            done(ctx, 0)
        }
        Err(e) if e == WOULD_BLOCK || e == 10036 => {
            ctx.kernel
                .vfs
                .bluetooth
                .sockets
                .get_mut(&h)
                .unwrap()
                .connecting = true;
            wait(ctx, nonblocking, timeout)
        }
        Err(e) => {
            ctx.kernel
                .vfs
                .bluetooth
                .sockets
                .get_mut(&h)
                .unwrap()
                .connecting = false;
            error(ctx, e)
        }
    }
}
fn output_address_valid(ctx: &mut CallCtx<'_>, ptr: u32, lenptr: u32) -> bool {
    access(ctx, lenptr, 4, Prot::READ | Prot::WRITE)
        && ctx.cpu.read_u32_le(lenptr).is_ok_and(|n| n >= 40)
        && access(ctx, ptr, 40, Prot::WRITE)
}
fn output_address(
    ctx: &mut CallCtx<'_>,
    ptr: u32,
    lenptr: u32,
    addr: &SocketAddress,
) -> Result<(), KernelError> {
    ctx.cpu.write_mem(ptr, &encode(addr))?;
    ctx.cpu
        .write_mem(lenptr, &40u32.to_le_bytes())
        .map_err(Into::into)
}
fn accept(ctx: &mut CallCtx<'_>) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let h = ctx.arg_u32(0)?;
    let ptr = ctx.arg_u32(1)?;
    let lenptr = ctx.arg_u32(2)?;
    if !valid(ctx, h) {
        return error(ctx, 10038);
    }
    if ptr != 0 && !output_address_valid(ctx, ptr, lenptr) {
        return error(ctx, 10014);
    }
    let s = ctx.kernel.vfs.bluetooth.sockets.get_mut(&h).unwrap();
    let nonblocking = s.nonblocking;
    let timeout = s.recv_timeout_ms;
    let send_timeout = s.send_timeout_ms;
    match s.native.accept() {
        Ok((native, addr)) => {
            let id = match ctx.kernel.vfs.bluetooth.insert_socket(native, nonblocking) {
                Ok(id) => id,
                Err(e) => return error(ctx, e),
            };
            let s = ctx.kernel.vfs.bluetooth.sockets.get_mut(&id).unwrap();
            s.recv_timeout_ms = timeout;
            s.send_timeout_ms = send_timeout;
            if ptr != 0 {
                output_address(ctx, ptr, lenptr, &addr)?;
            }
            done(ctx, id)
        }
        Err(WOULD_BLOCK) => wait(ctx, nonblocking, 0),
        Err(e) => error(ctx, e),
    }
}
fn recv(ctx: &mut CallCtx<'_>) -> Outcome {
    transfer(ctx, false)
}
fn send(ctx: &mut CallCtx<'_>) -> Outcome {
    transfer(ctx, true)
}
fn transfer(ctx: &mut CallCtx<'_>, sending: bool) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let h = ctx.arg_u32(0)?;
    let ptr = ctx.arg_u32(1)?;
    let len = ctx.arg_u32(2)?;
    let flags = ctx.arg_u32(3)? as i32;
    if !valid(ctx, h) {
        return error(ctx, 10038);
    }
    if (len as i32) < 0 {
        return error(ctx, 10022);
    }
    if len > 4 * 1024 * 1024 {
        return error(ctx, 10055);
    }
    if !access(
        ctx,
        ptr,
        len,
        if sending { Prot::READ } else { Prot::WRITE },
    ) {
        return error(ctx, 10014);
    }
    let mut bytes = if sending && len != 0 {
        ctx.cpu.read_mem(ptr, len)?
    } else {
        vec![0; len as usize]
    };
    let s = ctx.kernel.vfs.bluetooth.sockets.get_mut(&h).unwrap();
    let nonblocking = s.nonblocking;
    let timeout = if sending {
        s.send_timeout_ms
    } else {
        s.recv_timeout_ms
    };
    let result = if sending {
        s.native.write(&bytes, flags)
    } else {
        s.native.read(&mut bytes, flags)
    };
    match result {
        Ok(n) if n <= bytes.len() => {
            if !sending && n != 0 {
                ctx.cpu.write_mem(ptr, &bytes[..n])?;
            }
            done(ctx, n as u32)
        }
        Ok(_) => error(ctx, 10014),
        Err(WOULD_BLOCK) => wait(ctx, nonblocking, timeout),
        Err(e) => error(ctx, e),
    }
}
fn close(ctx: &mut CallCtx<'_>) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let h = ctx.arg_u32(0)?;
    if ctx.kernel.vfs.bluetooth.sockets.remove(&h).is_none() {
        error(ctx, 10038)
    } else {
        done(ctx, 0)
    }
}
fn shutdown(ctx: &mut CallCtx<'_>) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let h = ctx.arg_u32(0)?;
    let how = ctx.arg_u32(1)? as i32;
    if !valid(ctx, h) {
        return error(ctx, 10038);
    }
    if !(0..=2).contains(&how) {
        return error(ctx, 10022);
    }
    match ctx
        .kernel
        .vfs
        .bluetooth
        .sockets
        .get_mut(&h)
        .unwrap()
        .native
        .shutdown(how)
    {
        Ok(()) => done(ctx, 0),
        Err(e) => error(ctx, e),
    }
}
fn ioctl(ctx: &mut CallCtx<'_>) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let h = ctx.arg_u32(0)?;
    let cmd = ctx.arg_u32(1)?;
    let ptr = ctx.arg_u32(2)?;
    if !valid(ctx, h) {
        return error(ctx, 10038);
    }
    if !access(ctx, ptr, 4, Prot::READ | Prot::WRITE) {
        return error(ctx, 10014);
    }
    match cmd {
        0x8004667e => {
            let value = ctx.cpu.read_u32_le(ptr)?;
            ctx.kernel
                .vfs
                .bluetooth
                .sockets
                .get_mut(&h)
                .unwrap()
                .nonblocking = value != 0;
            done(ctx, 0)
        }
        0x4004667f => match ctx
            .kernel
            .vfs
            .bluetooth
            .sockets
            .get_mut(&h)
            .unwrap()
            .native
            .available()
        {
            Ok(n) => {
                ctx.cpu.write_mem(ptr, &n.to_le_bytes())?;
                done(ctx, 0)
            }
            Err(e) => error(ctx, e),
        },
        _ => error(ctx, 10022),
    }
}
fn set_option(ctx: &mut CallCtx<'_>) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let h = ctx.arg_u32(0)?;
    let level = ctx.arg_u32(1)? as i32;
    let name = ctx.arg_u32(2)? as i32;
    let ptr = ctx.arg_u32(3)?;
    let len = ctx.arg_u32(4)?;
    if !valid(ctx, h) {
        return error(ctx, 10038);
    }
    if len > 4096 || !access(ctx, ptr, len, Prot::READ) {
        return error(ctx, 10014);
    }
    let bytes = if len == 0 {
        Vec::new()
    } else {
        ctx.cpu.read_mem(ptr, len)?
    };
    if level == 0xffff && (name == 0x1005 || name == 0x1006) {
        if len != 4 {
            return error(ctx, 10014);
        }
        let ms = u32::from_le_bytes(bytes.try_into().unwrap());
        let s = ctx.kernel.vfs.bluetooth.sockets.get_mut(&h).unwrap();
        if name == 0x1005 {
            s.send_timeout_ms = ms;
        } else {
            s.recv_timeout_ms = ms;
        }
        return done(ctx, 0);
    }
    match ctx
        .kernel
        .vfs
        .bluetooth
        .sockets
        .get_mut(&h)
        .unwrap()
        .native
        .set_option(level, name, &bytes)
    {
        Ok(()) => done(ctx, 0),
        Err(e) => error(ctx, e),
    }
}
fn get_option(ctx: &mut CallCtx<'_>) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let h = ctx.arg_u32(0)?;
    let level = ctx.arg_u32(1)? as i32;
    let name = ctx.arg_u32(2)? as i32;
    let ptr = ctx.arg_u32(3)?;
    let lenptr = ctx.arg_u32(4)?;
    if !valid(ctx, h) {
        return error(ctx, 10038);
    }
    if !access(ctx, lenptr, 4, Prot::READ | Prot::WRITE) {
        return error(ctx, 10014);
    }
    let len = ctx.cpu.read_u32_le(lenptr)?;
    if len > 4096 || !access(ctx, ptr, len, Prot::WRITE) {
        return error(ctx, 10014);
    }
    let s = ctx.kernel.vfs.bluetooth.sockets.get_mut(&h).unwrap();
    let result = if level == 0xffff && (name == 0x1005 || name == 0x1006) {
        if len < 4 {
            Err(10014)
        } else {
            Ok((if name == 0x1005 {
                s.send_timeout_ms
            } else {
                s.recv_timeout_ms
            })
            .to_le_bytes()
            .to_vec())
        }
    } else {
        s.native.get_option(level, name, len as usize)
    };
    match result {
        Ok(bytes) if bytes.len() <= len as usize => {
            if !bytes.is_empty() {
                ctx.cpu.write_mem(ptr, &bytes)?;
            }
            ctx.cpu
                .write_mem(lenptr, &(bytes.len() as u32).to_le_bytes())?;
            done(ctx, 0)
        }
        Ok(_) => error(ctx, 10014),
        Err(e) => error(ctx, e),
    }
}
fn local_address(ctx: &mut CallCtx<'_>) -> Outcome {
    address(ctx, false)
}
fn peer_address(ctx: &mut CallCtx<'_>) -> Outcome {
    address(ctx, true)
}
fn address(ctx: &mut CallCtx<'_>, peer: bool) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let h = ctx.arg_u32(0)?;
    let ptr = ctx.arg_u32(1)?;
    let lenptr = ctx.arg_u32(2)?;
    if !valid(ctx, h) {
        return error(ctx, 10038);
    }
    if !output_address_valid(ctx, ptr, lenptr) {
        return error(ctx, 10014);
    }
    match ctx
        .kernel
        .vfs
        .bluetooth
        .sockets
        .get_mut(&h)
        .unwrap()
        .native
        .address(peer)
    {
        Ok(addr) => {
            output_address(ctx, ptr, lenptr, &addr)?;
            done(ctx, 0)
        }
        Err(e) => error(ctx, e),
    }
}
fn select(ctx: &mut CallCtx<'_>) -> Outcome {
    if !initialized(ctx) {
        return error(ctx, 10093);
    }
    let pointers = [ctx.arg_u32(1)?, ctx.arg_u32(2)?, ctx.arg_u32(3)?];
    let timeout = ctx.arg_u32(4)?;
    let mut sets: [Vec<u32>; 3] = Default::default();
    for (i, ptr) in pointers.iter().enumerate() {
        if *ptr == 0 {
            continue;
        }
        if !access(ctx, *ptr, 4, Prot::READ | Prot::WRITE) {
            return error(ctx, 10014);
        }
        let count = ctx.cpu.read_u32_le(*ptr)?;
        if count > 64 {
            return error(ctx, 10022);
        }
        if !access(ctx, *ptr, 4 + count * 4, Prot::READ | Prot::WRITE) {
            return error(ctx, 10014);
        }
        for j in 0..count {
            let h = ctx.cpu.read_u32_le(*ptr + 4 + j * 4)?;
            if !valid(ctx, h) {
                return error(ctx, 10038);
            }
            if !sets[i].contains(&h) {
                sets[i].push(h);
            }
        }
    }
    if sets.iter().all(Vec::is_empty) {
        return error(ctx, 10022);
    }
    let duration = if timeout == 0 {
        None
    } else {
        if !access(ctx, timeout, 8, Prot::READ) {
            return error(ctx, 10014);
        }
        let sec = ctx.cpu.read_u32_le(timeout)? as i32;
        let usec = ctx.cpu.read_u32_le(timeout + 4)? as i32;
        if sec < 0 || !(0..1_000_000).contains(&usec) {
            return error(ctx, 10022);
        }
        Some(Duration::from_secs(sec as u64) + Duration::from_micros(usec as u64))
    };
    let mut ready: [Vec<u32>; 3] = Default::default();
    let mut results = std::collections::HashMap::new();
    for i in 0..3 {
        for &h in &sets[i] {
            let result = match results.get(&h) {
                Some(r) => *r,
                None => {
                    let r = match ctx
                        .kernel
                        .vfs
                        .bluetooth
                        .sockets
                        .get_mut(&h)
                        .unwrap()
                        .native
                        .readiness()
                    {
                        Ok(r) => r,
                        Err(e) => return error(ctx, e),
                    };
                    results.insert(h, r);
                    r
                }
            };
            if [result.0, result.1, result.2][i] {
                ready[i].push(h);
            }
        }
    }
    let count = ready.iter().map(Vec::len).sum::<usize>();
    if count == 0 {
        let expired = match duration {
            Some(d) if d.is_zero() => true,
            Some(d) => {
                let k = key(ctx)?;
                let deadline = *ctx
                    .kernel
                    .vfs
                    .bluetooth
                    .socket_deadlines
                    .entry(k)
                    .or_insert_with(|| Instant::now() + d);
                Instant::now() >= deadline
            }
            None => false,
        };
        if !expired {
            return crate::bluetooth::retry(ctx);
        }
    }
    for (i, ptr) in pointers.iter().enumerate() {
        if *ptr != 0 {
            let mut bytes = (ready[i].len() as u32).to_le_bytes().to_vec();
            for h in &ready[i] {
                bytes.extend_from_slice(&h.to_le_bytes());
            }
            ctx.cpu.write_mem(*ptr, &bytes)?;
        }
    }
    done(ctx, count as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluetooth::tests::{call, setup};
    use pocket_cpu::Cpu;
    use pocket_kernel::bluetooth::{
        Backend, BtResult, Device, PortParams, RfcommSocket, Service, Stream,
    };
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    struct Shared {
        rx: VecDeque<u8>,
        accepts: u32,
        bound: SocketAddress,
        ready: bool,
        closed: u32,
    }
    struct Native(Arc<Mutex<Shared>>);
    impl Drop for Native {
        fn drop(&mut self) {
            self.0.lock().unwrap().closed += 1;
        }
    }
    impl RfcommSocket for Native {
        fn bind(&mut self, a: &SocketAddress) -> BtResult<()> {
            self.0.lock().unwrap().bound = a.clone();
            Ok(())
        }
        fn listen(&mut self, _: i32) -> BtResult<()> {
            Ok(())
        }
        fn connect(&mut self, _: &SocketAddress) -> BtResult<()> {
            Err(WOULD_BLOCK)
        }
        fn accept(&mut self) -> BtResult<(Box<dyn RfcommSocket>, SocketAddress)> {
            let mut s = self.0.lock().unwrap();
            if !s.ready {
                return Err(WOULD_BLOCK);
            }
            s.accepts += 1;
            Ok((Box::new(Native(self.0.clone())), s.bound.clone()))
        }
        fn read(&mut self, b: &mut [u8], flags: i32) -> BtResult<usize> {
            let mut s = self.0.lock().unwrap();
            if s.rx.is_empty() {
                return Err(WOULD_BLOCK);
            }
            let n = b.len().min(s.rx.len());
            for (i, byte) in b[..n].iter_mut().enumerate() {
                *byte = if flags == 2 {
                    s.rx[i]
                } else {
                    s.rx.pop_front().unwrap()
                };
            }
            Ok(n)
        }
        fn write(&mut self, b: &[u8], _: i32) -> BtResult<usize> {
            let n = b.len().min(2);
            self.0.lock().unwrap().rx.extend(&b[..n]);
            Ok(n)
        }
        fn readiness(&mut self) -> BtResult<(bool, bool, bool)> {
            let s = self.0.lock().unwrap();
            Ok((!s.rx.is_empty() || s.ready, s.ready, false))
        }
        fn available(&mut self) -> BtResult<u32> {
            Ok(self.0.lock().unwrap().rx.len() as u32)
        }
        fn set_option(&mut self, level: i32, _: i32, _: &[u8]) -> BtResult<()> {
            if level == 6 {
                Err(10042)
            } else {
                Ok(())
            }
        }
        fn get_option(&mut self, _: i32, _: i32, _: usize) -> BtResult<Vec<u8>> {
            Ok(0u32.to_le_bytes().to_vec())
        }
        fn address(&mut self, _: bool) -> BtResult<SocketAddress> {
            Ok(self.0.lock().unwrap().bound.clone())
        }
        fn shutdown(&mut self, _: i32) -> BtResult<()> {
            Ok(())
        }
    }
    struct Host(Arc<Mutex<Shared>>);
    impl Backend for Host {
        fn socket(&self) -> BtResult<Box<dyn RfcommSocket>> {
            Ok(Box::new(Native(self.0.clone())))
        }
        fn hostname(&self) -> BtResult<String> {
            Ok("test".into())
        }
        fn scan(&self) -> BtResult<Vec<Device>> {
            Ok(vec![])
        }
        fn open(&self, _: &PortParams) -> BtResult<Box<dyn Stream>> {
            Err(10045)
        }
    }
    fn started() -> (
        pocket_cpu::stub::StubCpu,
        pocket_kernel::KernelState,
        WinCeDispatcher,
        Arc<Mutex<Shared>>,
    ) {
        let (mut cpu, mut k, mut d) = setup();
        let shared = Arc::new(Mutex::new(Shared {
            rx: VecDeque::new(),
            accepts: 0,
            bound: Default::default(),
            ready: false,
            closed: 0,
        }));
        let service = Service::with_backend(Arc::new(Host(shared.clone())));
        service.set_allowed(true);
        service.enable(true);
        k.vfs.set_bluetooth_service(service);
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "WSAStartup",
                &[0x202, 0x1800]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        (cpu, k, d, shared)
    }
    fn ret(outcome: DispatchOutcome) -> u32 {
        match outcome {
            DispatchOutcome::ReturnedR0(n) => n,
            other => panic!("unexpected {other:?}"),
        }
    }
    #[test]
    fn chicane_server_sequence_real_handles_options_nonblocking_and_cleanup() {
        let (mut cpu, mut k, mut d, shared) = started();
        let h = ret(call(
            &mut cpu,
            &mut k,
            &mut d,
            "ws2.dll",
            "socket",
            &[32, 1, 3],
        ));
        assert_ne!(h, 0);
        assert_ne!(h, u32::MAX);
        cpu.write_mem(0x1100, &4096u32.to_le_bytes()).unwrap();
        for option in [0x1001, 0x1002] {
            assert_eq!(
                call(
                    &mut cpu,
                    &mut k,
                    &mut d,
                    "ws2.dll",
                    "setsockopt",
                    &[h, 0xffff, option, 0x1100, 4]
                ),
                DispatchOutcome::ReturnedR0(0)
            );
        }
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "setsockopt",
                &[h, 6, 1, 0x1100, 4]
            ),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        assert_eq!(k.winsock_last_errors[&0], 10042);
        assert!(k.thread_last_errors.is_empty());
        cpu.write_mem(0x1100, &1u32.to_le_bytes()).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "ioctlsocket",
                &[h, 0x8004667e, 0x1100]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        let addr = SocketAddress {
            address: 0,
            port: 7,
            uuid: [0; 16],
        };
        cpu.write_mem(0x1200, &encode(&addr)).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "bind",
                &[h, 0x1200, 40]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(shared.lock().unwrap().bound.port, 7);
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "ws2.dll", "listen", &[h, 8]),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "ws2.dll", "accept", &[h, 0, 0]),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        assert_eq!(k.winsock_last_errors[&0], WOULD_BLOCK);
        shared.lock().unwrap().ready = true;
        // Invalid output must not consume an incoming connection.
        cpu.write_mem(0x1300, &39u32.to_le_bytes()).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "accept",
                &[h, 0x1400, 0x1300]
            ),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        assert_eq!(shared.lock().unwrap().accepts, 0);
        cpu.write_mem(0x1300, &40u32.to_le_bytes()).unwrap();
        let peer = ret(call(
            &mut cpu,
            &mut k,
            &mut d,
            "ws2.dll",
            "accept",
            &[h, 0x1400, 0x1300],
        ));
        assert!(k.vfs.bluetooth.sockets[&peer].nonblocking);
        assert_eq!(decode(&cpu.read_mem(0x1400, 40).unwrap()).unwrap(), addr);
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "ws2.dll", "WSACleanup", &[]),
            DispatchOutcome::ReturnedR0(0)
        );
        assert!(k.vfs.bluetooth.sockets.is_empty());
        assert_eq!(shared.lock().unwrap().closed, 2);
    }
    #[test]
    fn data_peek_partial_send_select_and_invalid_pointers_do_not_lose_bytes() {
        let (mut cpu, mut k, mut d, shared) = started();
        let h = ret(call(
            &mut cpu,
            &mut k,
            &mut d,
            "ws2.dll",
            "socket",
            &[32, 1, 3],
        ));
        k.vfs.bluetooth.sockets.get_mut(&h).unwrap().nonblocking = true;
        shared.lock().unwrap().rx.extend(b"hello");
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "ws2.dll", "recv", &[h, 0, 5, 0]),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        assert_eq!(shared.lock().unwrap().rx.len(), 5);
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "recv",
                &[h, 0x1100, 2, 2]
            ),
            DispatchOutcome::ReturnedR0(2)
        );
        assert_eq!(cpu.read_mem(0x1100, 2).unwrap(), b"he");
        assert_eq!(shared.lock().unwrap().rx.len(), 5);
        cpu.write_mem(0x1200, &[1, 0, 0, 0]).unwrap();
        cpu.write_mem(0x1204, &h.to_le_bytes()).unwrap();
        cpu.write_mem(0x1300, &[0; 8]).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "select",
                &[0, 0x1200, 0, 0, 0x1300]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "recv",
                &[h, 0x1100, 5, 0]
            ),
            DispatchOutcome::ReturnedR0(5)
        );
        assert_eq!(cpu.read_mem(0x1100, 5).unwrap(), b"hello");
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "recv",
                &[h, 0x1100, 5, 0]
            ),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        assert_eq!(k.winsock_last_errors[&0], WOULD_BLOCK);
        cpu.write_mem(0x1200, &1u32.to_le_bytes()).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "select",
                &[0, 0x1200, 0, 0, 0x1300]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(cpu.read_u32_le(0x1200).unwrap(), 0);
        cpu.write_mem(0x1100, b"ABCD").unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "send",
                &[h, 0x1100, 4, 0]
            ),
            DispatchOutcome::ReturnedR0(2)
        );
        assert_eq!(
            shared
                .lock()
                .unwrap()
                .rx
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            b"AB"
        );
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "ws2.dll", "closesocket", &[h]),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "recv",
                &[h, 0x1100, 4, 0]
            ),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        assert_eq!(k.winsock_last_errors[&0], 10038);
    }
    #[test]
    fn arm_address_padding_and_new_handler_registration() {
        let addr = SocketAddress {
            address: 0x123456789abc,
            uuid: [7; 16],
            port: 12,
        };
        assert_eq!(decode(&encode(&addr)).unwrap(), addr);
        for size in [30, 32] {
            let offset = if size == 30 { 2 } else { 4 };
            let mut b = vec![0; size];
            b[..2].copy_from_slice(&32u16.to_le_bytes());
            b[offset..offset + 8].copy_from_slice(&addr.address.to_le_bytes());
            b[offset + 8..offset + 24].copy_from_slice(&addr.uuid);
            b[offset + 24..offset + 28].copy_from_slice(&addr.port.to_le_bytes());
            assert_eq!(decode(&b).unwrap(), addr);
        }
        let (mut cpu, mut k, mut d, _) = started();
        let api = "?_set_new_handler@@YAP6AHI@ZP6AHI@Z@Z";
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "coredll.dll", api, &[0x1234]),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "coredll.dll", api, &[0x5678]),
            DispatchOutcome::ReturnedR0(0x1234)
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "?_query_new_handler@@YAP6AHI@ZXZ",
                &[]
            ),
            DispatchOutcome::ReturnedR0(0x5678)
        );
    }
    #[test]
    fn blocking_receive_timeout_select_deadline_and_pending_connect() {
        let (mut cpu, mut k, mut d, shared) = started();
        let h = ret(call(
            &mut cpu,
            &mut k,
            &mut d,
            "ws2.dll",
            "socket",
            &[32, 1, 3],
        ));
        // A blocking receive with no bytes waits instead of fabricating EOF.
        let outcome = call(
            &mut cpu,
            &mut k,
            &mut d,
            "ws2.dll",
            "recv",
            &[h, 0x1100, 4, 0],
        );
        assert!(!matches!(outcome, DispatchOutcome::ReturnedR0(_)));
        k.vfs.bluetooth.sockets.get_mut(&h).unwrap().recv_timeout_ms = 1;
        let deadline_key = (0, 0x70000000, 0x4000);
        k.vfs
            .bluetooth
            .socket_deadlines
            .insert(deadline_key, Instant::now() - Duration::from_millis(1));
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "recv",
                &[h, 0x1100, 4, 0]
            ),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        assert_eq!(k.winsock_last_errors[&0], 10060);
        assert!(k.vfs.bluetooth.socket_deadlines.is_empty());
        // select preserves its input sets while waiting, then clears them on timeout.
        cpu.write_mem(0x1200, &1u32.to_le_bytes()).unwrap();
        cpu.write_mem(0x1204, &h.to_le_bytes()).unwrap();
        cpu.write_mem(0x1300, &[1, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        let outcome = call(
            &mut cpu,
            &mut k,
            &mut d,
            "ws2.dll",
            "select",
            &[0, 0x1200, 0, 0, 0x1300],
        );
        assert!(!matches!(outcome, DispatchOutcome::ReturnedR0(_)));
        assert_eq!(cpu.read_u32_le(0x1200).unwrap(), 1);
        k.vfs
            .bluetooth
            .socket_deadlines
            .insert(deadline_key, Instant::now() - Duration::from_millis(1));
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "select",
                &[0, 0x1200, 0, 0, 0x1300]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(cpu.read_u32_le(0x1200).unwrap(), 0);
        assert!(k.vfs.bluetooth.socket_deadlines.is_empty());
        cpu.write_mem(
            0x1400,
            &encode(&SocketAddress {
                address: 0x123456789abc,
                port: 7,
                uuid: [0; 16],
            }),
        )
        .unwrap();
        let outcome = call(
            &mut cpu,
            &mut k,
            &mut d,
            "ws2.dll",
            "connect",
            &[h, 0x1400, 40],
        );
        assert!(!matches!(outcome, DispatchOutcome::ReturnedR0(_)));
        assert!(k.vfs.bluetooth.sockets[&h].connecting);
        shared.lock().unwrap().ready = true;
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "connect",
                &[h, 0x1400, 40]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert!(!k.vfs.bluetooth.sockets[&h].connecting);
    }

    #[test]
    fn disabling_shared_bluetooth_closes_sockets_without_fake_eof() {
        let (mut cpu, mut k, mut d, shared) = started();
        let h = ret(call(
            &mut cpu,
            &mut k,
            &mut d,
            "ws2.dll",
            "socket",
            &[32, 1, 3],
        ));
        k.vfs.bluetooth.service.enable(false);
        assert_eq!(shared.lock().unwrap().closed, 1);
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "ws2.dll",
                "recv",
                &[h, 0x1100, 4, 0]
            ),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        assert_eq!(k.winsock_last_errors[&0], 10050);
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "ws2.dll", "closesocket", &[h]),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(shared.lock().unwrap().closed, 1);
    }
}
