use pocket_kernel::winsock::{
    SockAddrV4, SocketError, AF_INET, FIONBIO, FIONREAD, HOSTENT_SIZE, INADDR_BROADCAST,
    MAX_DATAGRAM_BYTES, WSADATA_SIZE,
};

use crate::{CallCtx, DispatchOutcome, KernelError, WinCeDispatcher};

const SOCKET_ERROR: u32 = u32::MAX;
const WSAEFAULT: u32 = 10_014;
const WSAHOST_NOT_FOUND: u32 = 11_001;
const WSA_VERSION_2_2: u16 = 0x0202;
const SOCKADDR_IN_SIZE: u32 = 16;

pub fn register(dispatcher: &mut WinCeDispatcher) {
    for dll in ["ws2.dll", "ws2_32.dll"] {
        dispatcher.register_handler(dll, "WSAStartup", wsa_startup);
        dispatcher.register_handler(dll, "WSACleanup", wsa_cleanup);
        dispatcher.register_handler(dll, "WSAGetLastError", wsa_get_last_error);
        dispatcher.register_handler(dll, "WSASetLastError", wsa_set_last_error);
        dispatcher.register_handler(dll, "socket", socket);
        dispatcher.register_handler(dll, "bind", bind);
        dispatcher.register_handler(dll, "connect", connect);
        dispatcher.register_handler(dll, "closesocket", close_socket);
        dispatcher.register_handler(dll, "send", send);
        dispatcher.register_handler(dll, "sendto", send_to);
        dispatcher.register_handler(dll, "recv", recv);
        dispatcher.register_handler(dll, "recvfrom", recv_from);
        dispatcher.register_handler(dll, "setsockopt", set_socket_option);
        dispatcher.register_handler(dll, "ioctlsocket", ioctlsocket);
        dispatcher.register_handler(dll, "gethostname", get_hostname);
        dispatcher.register_handler(dll, "gethostbyname", get_host_by_name);
        dispatcher.register_handler(dll, "inet_addr", inet_addr);
        dispatcher.register_handler(dll, "htons", htons);
        dispatcher.register_handler(dll, "ntohl", ntohl);
    }
}

fn wsa_startup(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let version = ctx.arg_u32(0)? as u16;
    let output = ctx.arg_u32(1)?;
    if output == 0 {
        set_winsock_error(ctx, WSAEFAULT);
        return Ok(DispatchOutcome::ReturnedR0(WSAEFAULT));
    }
    if let Err(error) = ctx.kernel.winsock.startup(version) {
        set_winsock_error(ctx, error.code());
        return Ok(DispatchOutcome::ReturnedR0(error.code()));
    }
    if ctx.cpu.write_mem(output, &make_wsa_data(version)).is_err() {
        let _ = ctx.kernel.winsock.cleanup();
        set_winsock_error(ctx, WSAEFAULT);
        return Ok(DispatchOutcome::ReturnedR0(WSAEFAULT));
    }
    set_winsock_error(ctx, 0);
    log::debug!("WS2.dll!WSAStartup(0x{version:04x}) -> 0");
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn wsa_cleanup(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(match ctx.kernel.winsock.cleanup() {
        Ok(()) => DispatchOutcome::ReturnedR0(0),
        Err(error) => socket_error(ctx, error),
    })
}

fn wsa_get_last_error(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let thread = ctx.kernel.current_thread;
    Ok(DispatchOutcome::ReturnedR0(
        ctx.kernel.winsock.last_error_for_thread(thread),
    ))
}

fn wsa_set_last_error(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let error = ctx.arg_u32(0)?;
    set_winsock_error(ctx, error);
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn socket(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let family = ctx.arg_u32(0)?;
    let kind = ctx.arg_u32(1)?;
    let protocol = ctx.arg_u32(2)?;
    Ok(
        match ctx.kernel.winsock.create_socket(family, kind, protocol) {
            Ok(handle) => DispatchOutcome::ReturnedR0(handle),
            Err(error) => socket_error(ctx, error),
        },
    )
}

fn bind(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let address_pointer = ctx.arg_u32(1)?;
    let address_length = ctx.arg_u32(2)?;
    let address = match read_sockaddr(ctx, address_pointer, address_length) {
        Ok(address) => address,
        Err(error) => return Ok(socket_error(ctx, error)),
    };
    Ok(match ctx.kernel.winsock.bind(handle, address) {
        Ok(()) => DispatchOutcome::ReturnedR0(0),
        Err(error) => socket_error(ctx, error),
    })
}

fn connect(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let address_pointer = ctx.arg_u32(1)?;
    let address_length = ctx.arg_u32(2)?;
    let address = match read_sockaddr(ctx, address_pointer, address_length) {
        Ok(address) => address,
        Err(error) => return Ok(socket_error(ctx, error)),
    };
    Ok(match ctx.kernel.winsock.connect(handle, address) {
        Ok(()) => DispatchOutcome::ReturnedR0(0),
        Err(error) => socket_error(ctx, error),
    })
}

fn close_socket(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    Ok(match ctx.kernel.winsock.close(handle) {
        Ok(()) => DispatchOutcome::ReturnedR0(0),
        Err(error) => socket_error(ctx, error),
    })
}

fn send(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let buffer = ctx.arg_u32(1)?;
    let length = ctx.arg_u32(2)? as i32;
    if length < 0 || length as usize > MAX_DATAGRAM_BYTES {
        return Ok(socket_error(ctx, SocketError::InvalidArgument));
    }
    let data = match read_guest_bytes(ctx, buffer, length as usize) {
        Ok(data) => data,
        Err(error) => return Ok(socket_error(ctx, error)),
    };
    Ok(match ctx.kernel.winsock.send(handle, &data) {
        Ok(sent) => DispatchOutcome::ReturnedR0(sent as u32),
        Err(error) => socket_error(ctx, error),
    })
}

fn send_to(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let buffer = ctx.arg_u32(1)?;
    let length = ctx.arg_u32(2)? as i32;
    let destination_ptr = ctx.arg_u32(4)?;
    let destination_len = ctx.arg_u32(5)?;
    if length < 0 || length as usize > MAX_DATAGRAM_BYTES {
        return Ok(socket_error(ctx, SocketError::InvalidArgument));
    }
    let data = match read_guest_bytes(ctx, buffer, length as usize) {
        Ok(data) => data,
        Err(error) => return Ok(socket_error(ctx, error)),
    };
    let result = if destination_ptr == 0 && destination_len == 0 {
        ctx.kernel.winsock.send(handle, &data)
    } else {
        let destination = match read_sockaddr(ctx, destination_ptr, destination_len) {
            Ok(address) => address,
            Err(error) => return Ok(socket_error(ctx, error)),
        };
        ctx.kernel.winsock.send_to(handle, &data, destination)
    };
    Ok(match result {
        Ok(sent) => DispatchOutcome::ReturnedR0(sent as u32),
        Err(error) => socket_error(ctx, error),
    })
}

fn recv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let buffer = ctx.arg_u32(1)?;
    let length = ctx.arg_u32(2)? as i32;
    if length < 0 {
        return Ok(socket_error(ctx, SocketError::InvalidArgument));
    }
    let packet = match ctx.kernel.winsock.receive(handle) {
        Ok(packet) => packet,
        Err(error) => return Ok(socket_error(ctx, error)),
    };
    Ok(copy_received_packet(
        ctx,
        buffer,
        length as usize,
        packet.data,
    ))
}

fn recv_from(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let buffer = ctx.arg_u32(1)?;
    let length = ctx.arg_u32(2)? as i32;
    let from = ctx.arg_u32(4)?;
    let from_length_ptr = ctx.arg_u32(5)?;
    if length < 0 {
        return Ok(socket_error(ctx, SocketError::InvalidArgument));
    }
    let from_length = if from != 0 {
        if from_length_ptr == 0 {
            return Ok(socket_error(ctx, SocketError::Fault));
        }
        let length = match read_u32(ctx, from_length_ptr) {
            Ok(length) => length,
            Err(error) => return Ok(socket_error(ctx, error)),
        };
        if length < SOCKADDR_IN_SIZE {
            return Ok(socket_error(ctx, SocketError::Fault));
        }
        Some(length)
    } else {
        None
    };
    let packet = match ctx.kernel.winsock.receive_from(handle) {
        Ok(packet) => packet,
        Err(error) => return Ok(socket_error(ctx, error)),
    };
    if packet.data.len() > length as usize {
        return Ok(socket_error(ctx, SocketError::MessageTooLarge));
    }
    let copied = packet.data.len();
    if copied > 0 && (buffer == 0 || ctx.cpu.write_mem(buffer, &packet.data[..copied]).is_err()) {
        return Ok(socket_error(ctx, SocketError::Fault));
    }
    if from != 0 {
        let Some(from_length) = from_length else {
            return Ok(socket_error(ctx, SocketError::Fault));
        };
        let address = write_sockaddr(packet.source);
        if ctx.cpu.write_mem(from, &address).is_err()
            || write_u32(ctx, from_length_ptr, from_length.min(SOCKADDR_IN_SIZE)).is_err()
        {
            return Ok(socket_error(ctx, SocketError::Fault));
        }
    }
    Ok(DispatchOutcome::ReturnedR0(copied as u32))
}

fn set_socket_option(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let level = ctx.arg_u32(1)?;
    let option = ctx.arg_u32(2)?;
    let value_ptr = ctx.arg_u32(3)?;
    let value_len = ctx.arg_u32(4)?;
    if value_ptr == 0 || value_len < 4 {
        return Ok(socket_error(ctx, SocketError::Fault));
    }
    let value = match read_u32(ctx, value_ptr) {
        Ok(value) => value,
        Err(error) => return Ok(socket_error(ctx, error)),
    };
    Ok(
        match ctx.kernel.winsock.set_option(handle, level, option, value) {
            Ok(()) => DispatchOutcome::ReturnedR0(0),
            Err(error) => socket_error(ctx, error),
        },
    )
}

fn ioctlsocket(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let command = ctx.arg_u32(1)?;
    let value_ptr = ctx.arg_u32(2)?;
    let result = match command {
        FIONBIO => {
            if value_ptr == 0 {
                Err(SocketError::Fault)
            } else {
                match read_u32(ctx, value_ptr) {
                    Ok(value) => ctx.kernel.winsock.set_nonblocking(handle, value != 0),
                    Err(error) => Err(error),
                }
            }
        }
        FIONREAD => match ctx.kernel.winsock.pending_bytes(handle) {
            Ok(value) if value_ptr != 0 => write_u32(ctx, value_ptr, value),
            Ok(_) => Err(SocketError::Fault),
            Err(error) => Err(error),
        },
        _ => Err(SocketError::InvalidArgument),
    };
    Ok(match result {
        Ok(()) => DispatchOutcome::ReturnedR0(0),
        Err(error) => socket_error(ctx, error),
    })
}

fn get_hostname(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let output = ctx.arg_u32(0)?;
    let capacity = ctx.arg_u32(1)? as usize;
    let mut hostname = ctx.kernel.winsock.hostname().as_bytes().to_vec();
    hostname.push(0);
    if output == 0 || capacity < hostname.len() {
        return Ok(socket_error(ctx, SocketError::Fault));
    }
    if ctx.cpu.write_mem(output, &hostname).is_err() {
        return Ok(socket_error(ctx, SocketError::Fault));
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn get_host_by_name(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let input = ctx.arg_u32(0)?;
    let name = match read_guest_string(ctx, input, 256) {
        Ok(name) => name,
        Err(error) => return Ok(socket_error(ctx, error)),
    };
    let address = match ctx.kernel.winsock.resolve_ipv4(&name) {
        Ok(address) => address,
        Err(SocketError::HostNotFound) => {
            set_winsock_error(ctx, WSAHOST_NOT_FOUND);
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
        Err(error) => return Ok(socket_error(ctx, error)),
    };
    let buffer = match ctx.kernel.winsock.hostent_buffer(&mut ctx.kernel.heap) {
        Ok(address) => address,
        Err(error) => return Ok(socket_error(ctx, error)),
    };
    let data = make_hostent(buffer, address, &name);
    if ctx.cpu.write_mem(buffer, &data).is_err() {
        return Ok(socket_error(ctx, SocketError::Fault));
    }
    Ok(DispatchOutcome::ReturnedR0(buffer))
}

fn inet_addr(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pointer = ctx.arg_u32(0)?;
    let text = match read_guest_string(ctx, pointer, 64) {
        Ok(text) => text,
        Err(_) => return Ok(DispatchOutcome::ReturnedR0(INADDR_BROADCAST)),
    };
    let address = ctx
        .kernel
        .winsock
        .resolve_ipv4(&text)
        .unwrap_or(INADDR_BROADCAST);
    Ok(DispatchOutcome::ReturnedR0(address))
}

fn htons(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let value = ctx.arg_u32(0)? as u16;
    Ok(DispatchOutcome::ReturnedR0(u32::from(value.swap_bytes())))
}

fn ntohl(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.arg_u32(0)?.swap_bytes()))
}

fn read_sockaddr(
    ctx: &mut CallCtx<'_>,
    pointer: u32,
    length: u32,
) -> Result<SockAddrV4, SocketError> {
    if pointer == 0 || length < SOCKADDR_IN_SIZE {
        return Err(SocketError::Fault);
    }
    let bytes = ctx
        .cpu
        .read_mem(pointer, SOCKADDR_IN_SIZE)
        .map_err(|_| SocketError::Fault)?;
    let family = u16::from_le_bytes([bytes[0], bytes[1]]) as u32;
    if family != AF_INET {
        return Err(SocketError::AddressFamilyNotSupported);
    }
    Ok(SockAddrV4 {
        port: u16::from_be_bytes([bytes[2], bytes[3]]),
        address: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    })
}

fn write_sockaddr(address: SockAddrV4) -> [u8; SOCKADDR_IN_SIZE as usize] {
    let mut bytes = [0u8; SOCKADDR_IN_SIZE as usize];
    bytes[..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    bytes[2..4].copy_from_slice(&address.port.to_be_bytes());
    bytes[4..8].copy_from_slice(&address.address.to_le_bytes());
    bytes
}

fn read_guest_bytes(
    ctx: &mut CallCtx<'_>,
    pointer: u32,
    length: usize,
) -> Result<Vec<u8>, SocketError> {
    if length == 0 {
        return Ok(Vec::new());
    }
    if pointer == 0 {
        return Err(SocketError::Fault);
    }
    ctx.cpu
        .read_mem(pointer, length as u32)
        .map_err(|_| SocketError::Fault)
}

fn read_guest_string(
    ctx: &mut CallCtx<'_>,
    pointer: u32,
    max_length: usize,
) -> Result<String, SocketError> {
    if pointer == 0 {
        return Err(SocketError::Fault);
    }
    let mut bytes = Vec::new();
    for index in 0..max_length {
        let address = pointer
            .checked_add(index as u32)
            .ok_or(SocketError::Fault)?;
        let byte = ctx
            .cpu
            .read_mem(address, 1)
            .map_err(|_| SocketError::Fault)?[0];
        if byte == 0 {
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        bytes.push(byte);
    }
    Err(SocketError::Fault)
}

fn read_u32(ctx: &mut CallCtx<'_>, pointer: u32) -> Result<u32, SocketError> {
    let bytes = ctx
        .cpu
        .read_mem(pointer, 4)
        .map_err(|_| SocketError::Fault)?;
    Ok(u32::from_le_bytes(
        bytes.try_into().map_err(|_| SocketError::Fault)?,
    ))
}

fn write_u32(ctx: &mut CallCtx<'_>, pointer: u32, value: u32) -> Result<(), SocketError> {
    ctx.cpu
        .write_mem(pointer, &value.to_le_bytes())
        .map_err(|_| SocketError::Fault)
}

fn copy_received_packet(
    ctx: &mut CallCtx<'_>,
    buffer: u32,
    capacity: usize,
    data: Vec<u8>,
) -> DispatchOutcome {
    if data.len() > capacity {
        return socket_error(ctx, SocketError::MessageTooLarge);
    }
    let copied = data.len();
    if copied > 0 && (buffer == 0 || ctx.cpu.write_mem(buffer, &data[..copied]).is_err()) {
        return socket_error(ctx, SocketError::Fault);
    }
    DispatchOutcome::ReturnedR0(copied as u32)
}

fn make_wsa_data(version: u16) -> [u8; WSADATA_SIZE] {
    let mut data = [0u8; WSADATA_SIZE];
    data[..2].copy_from_slice(&version.to_le_bytes());
    data[2..4].copy_from_slice(&WSA_VERSION_2_2.to_le_bytes());
    write_fixed_string(&mut data[4..261], "PocketHLE loopback Winsock");
    write_fixed_string(&mut data[261..390], "Local-only IPv4");
    data[390..392].copy_from_slice(&128u16.to_le_bytes());
    data[392..394].copy_from_slice(&(MAX_DATAGRAM_BYTES as u16).to_le_bytes());
    data[396..400].copy_from_slice(&0u32.to_le_bytes());
    data
}

fn write_fixed_string(output: &mut [u8], text: &str) {
    let count = output.len().saturating_sub(1).min(text.len());
    output[..count].copy_from_slice(&text.as_bytes()[..count]);
}

fn make_hostent(base: u32, address: u32, name: &str) -> [u8; HOSTENT_SIZE] {
    let mut data = [0u8; HOSTENT_SIZE];
    data[0..4].copy_from_slice(&base.wrapping_add(32).to_le_bytes());
    data[4..8].copy_from_slice(&base.wrapping_add(16).to_le_bytes());
    data[8..10].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    data[10..12].copy_from_slice(&4u16.to_le_bytes());
    data[12..16].copy_from_slice(&base.wrapping_add(20).to_le_bytes());
    data[16..20].copy_from_slice(&0u32.to_le_bytes());
    data[20..24].copy_from_slice(&base.wrapping_add(28).to_le_bytes());
    data[24..28].copy_from_slice(&0u32.to_le_bytes());
    data[28..32].copy_from_slice(&address.to_le_bytes());
    write_fixed_string(&mut data[32..], name);
    data
}

fn socket_error(ctx: &mut CallCtx<'_>, error: SocketError) -> DispatchOutcome {
    set_winsock_error(ctx, error.code());
    DispatchOutcome::ReturnedR0(SOCKET_ERROR)
}

fn set_winsock_error(ctx: &mut CallCtx<'_>, error: u32) {
    let thread = ctx.kernel.current_thread;
    ctx.kernel.winsock.set_last_error_for_thread(thread, error);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket_kernel::winsock::IPV4_LOOPBACK;
    use pocket_kernel::Dispatcher;

    #[test]
    fn dispatcher_registers_quake3_winsock_imports() {
        let dispatcher = WinCeDispatcher::new();
        let names = dispatcher.dynamic_names("ws2.dll");
        for name in [
            "WSAStartup",
            "WSAGetLastError",
            "socket",
            "bind",
            "sendto",
            "recvfrom",
        ] {
            assert!(names.contains(&name.to_string()), "missing {name}");
        }
    }

    #[test]
    fn sockaddr_in_uses_network_byte_order_for_port_and_address_bytes() {
        let address = SockAddrV4 {
            address: IPV4_LOOPBACK,
            port: 27_960,
        };
        let bytes = write_sockaddr(address);
        assert_eq!(&bytes[..2], &2u16.to_le_bytes());
        assert_eq!(&bytes[2..4], &27_960u16.to_be_bytes());
        assert_eq!(&bytes[4..8], &[127, 0, 0, 1]);
    }

    #[test]
    fn hostent_result_contains_a_stable_loopback_address() {
        let base = 0x5000_1000;
        let result = make_hostent(base, IPV4_LOOPBACK, "localhost");
        assert_eq!(
            u32::from_le_bytes(result[0..4].try_into().unwrap()),
            base + 32
        );
        assert_eq!(
            u32::from_le_bytes(result[12..16].try_into().unwrap()),
            base + 20
        );
        assert_eq!(&result[28..32], &[127, 0, 0, 1]);
        assert_eq!(&result[32..42], b"localhost\0");
    }
}
