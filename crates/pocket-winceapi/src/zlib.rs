//! WinCE zlib.dll ABI used by Unfinished Business (Gametrac Demo).
use crate::{CallCtx, WinCeDispatcher};
use pocket_kernel::{DispatchOutcome, KernelError};
const MAX_BUFFER: usize = 64 * 1024 * 1024;
fn result(code: i32) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(code as u32))
}
pub fn register(d: &mut WinCeDispatcher) {
    for (ordinal, name, handler) in [
        (22, "inflateInit_", init as crate::Handler),
        (19, "inflate", inflate as crate::Handler),
        (20, "inflateEnd", end as crate::Handler),
        (2, "compress", compress as crate::Handler),
    ] {
        for key in [
            name.to_owned(),
            format!("#{ordinal}"),
            format!("ord:{ordinal}"),
        ] {
            d.register_handler("zlib.dll", &key, handler);
        }
    }
}
fn word(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}
fn put(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn init(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    let version = ctx.arg_u32(1)?;
    let size = ctx.arg_u32(2)?;
    if ptr == 0 {
        return result(-2);
    }
    if version == 0 || size != 56 || ctx.cpu.read_mem(version, 1)?[0] != b'1' {
        return result(-6);
    }
    let mut data = ctx.cpu.read_mem(ptr, 56)?;
    let Some(handle) = ctx.kernel.zlib_streams.init() else {
        return result(-4);
    };
    for offset in [8, 20, 24] {
        put(&mut data, offset, 0);
    }
    put(&mut data, 28, handle);
    put(&mut data, 44, 2);
    put(&mut data, 48, 1);
    if let Err(error) = ctx.cpu.write_mem(ptr, &data) {
        ctx.kernel.zlib_streams.end(handle);
        return Err(error.into());
    }
    result(0)
}
fn inflate(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    let flush = ctx.arg_u32(1)?;
    if ptr == 0 {
        return result(-2);
    }
    let mut data = ctx.cpu.read_mem(ptr, 56)?;
    let handle = word(&data, 28);
    let next_in = word(&data, 0);
    let avail_in = word(&data, 4) as usize;
    let next_out = word(&data, 12);
    let avail_out = word(&data, 16) as usize;
    if !ctx.kernel.zlib_streams.contains(handle)
        || (avail_in != 0 && next_in == 0)
        || next_out == 0
        || flush > 4
    {
        return result(-2);
    }
    if avail_in > MAX_BUFFER || avail_out > MAX_BUFFER {
        return result(-4);
    }
    if avail_out == 0 {
        return result(-5);
    }
    if next_in.checked_add(avail_in as u32).is_none()
        || next_out.checked_add(avail_out as u32).is_none()
    {
        return result(-2);
    }
    let input = if avail_in == 0 {
        Vec::new()
    } else {
        ctx.cpu.read_mem(next_in, avail_in as u32)?
    };
    // Validate the full destination before advancing the host stream.
    let mut output = ctx.cpu.read_mem(next_out, avail_out as u32)?;
    let step = match ctx
        .kernel
        .zlib_streams
        .inflate(handle, &input, &mut output, flush)
    {
        Ok(step) => step,
        Err(code) => return result(code),
    };
    ctx.cpu.write_mem(next_out, &output[..step.written])?;
    put(&mut data, 0, next_in + step.consumed as u32);
    put(&mut data, 4, (avail_in - step.consumed) as u32);
    let total_in = word(&data, 8).wrapping_add(step.consumed as u32);
    put(&mut data, 8, total_in);
    put(&mut data, 12, next_out + step.written as u32);
    put(&mut data, 16, (avail_out - step.written) as u32);
    let total_out = word(&data, 20).wrapping_add(step.written as u32);
    put(&mut data, 20, total_out);
    put(&mut data, 48, step.adler);
    ctx.cpu.write_mem(ptr, &data)?;
    result(step.status)
}
fn end(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    if ptr == 0 {
        return result(-2);
    }
    let mut data = ctx.cpu.read_mem(ptr, 56)?;
    if !ctx.kernel.zlib_streams.end(word(&data, 28)) {
        return result(-2);
    }
    put(&mut data, 28, 0);
    ctx.cpu.write_mem(ptr, &data)?;
    result(0)
}
fn compress(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dest = ctx.arg_u32(0)?;
    let size_ptr = ctx.arg_u32(1)?;
    let source = ctx.arg_u32(2)?;
    let len = ctx.arg_u32(3)? as usize;
    if dest == 0 || size_ptr == 0 || (source == 0 && len != 0) {
        return result(-2);
    }
    let capacity = word(&ctx.cpu.read_mem(size_ptr, 4)?, 0) as usize;
    if len > MAX_BUFFER || capacity > MAX_BUFFER {
        return result(-4);
    }
    if source.checked_add(len as u32).is_none() || dest.checked_add(capacity as u32).is_none() {
        return result(-2);
    }
    let input = if len == 0 {
        Vec::new()
    } else {
        ctx.cpu.read_mem(source, len as u32)?
    };
    let encoded = match pocket_kernel::zlib::ZlibStreams::compress(&input) {
        Ok(data) => data,
        Err(_) => return result(-4),
    };
    let count = encoded.len().min(capacity);
    ctx.cpu.write_mem(dest, &encoded[..count])?;
    ctx.cpu.write_mem(size_ptr, &(count as u32).to_le_bytes())?;
    result(if count == encoded.len() { 0 } else { -5 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu, Prot};
    fn call(
        cpu: &mut StubCpu,
        kernel: &mut pocket_kernel::KernelState,
        handler: crate::Handler,
        args: &[u32],
    ) -> u32 {
        for (i, &value) in args.iter().enumerate() {
            cpu.write_reg([ArmReg::R0, ArmReg::R1, ArmReg::R2, ArmReg::R3][i], value)
                .unwrap();
        }
        let thunk = pocket_kernel::Thunk {
            thunk_va: 0x70000000,
            iat_va: 0,
            dll: "zlib.dll".into(),
            binding: pocket_pe::ImportBinding::Name("test".into()),
            friendly_name: None,
        };
        match handler(&mut CallCtx {
            cpu,
            thunk: &thunk,
            kernel,
        })
        .unwrap()
        {
            DispatchOutcome::ReturnedR0(value) => value,
            _ => panic!("unexpected outcome"),
        }
    }
    #[test]
    fn guest_z_stream_partial_buffers_totals_and_cleanup() {
        let mut cpu = StubCpu::new();
        cpu.map_region(0x1000, 0x4000, Prot::READ | Prot::WRITE)
            .unwrap();
        let mut kernel = crate::gx::tests::fresh_kernel();
        cpu.write_mem(0x1100, b"1.1.3\0").unwrap();
        assert_eq!(
            call(&mut cpu, &mut kernel, init, &[0x1000, 0x1100, 55]),
            (-6i32) as u32
        );
        let payload = b"Unfinished Business CFL asset compressed in multiple output buffers";
        let compressed = pocket_kernel::zlib::ZlibStreams::compress(payload).unwrap();
        cpu.write_mem(0x2000, &compressed).unwrap();
        let mut stream = [0; 56];
        put(&mut stream, 0, 0x2000);
        put(&mut stream, 4, compressed.len() as u32);
        cpu.write_mem(0x1000, &stream).unwrap();
        assert_eq!(call(&mut cpu, &mut kernel, init, &[0x1000, 0x1100, 56]), 0);
        let mut out = Vec::new();
        for _ in 0..100 {
            let mut data = cpu.read_mem(0x1000, 56).unwrap();
            put(&mut data, 12, 0x3000);
            put(&mut data, 16, 7);
            cpu.write_mem(0x1000, &data).unwrap();
            let code = call(&mut cpu, &mut kernel, inflate, &[0x1000, 0]);
            let data = cpu.read_mem(0x1000, 56).unwrap();
            out.extend(cpu.read_mem(0x3000, 7 - word(&data, 16)).unwrap());
            assert!(code == 0 || code == 1, "status {code:#x}");
            if code == 1 {
                break;
            }
        }
        assert_eq!(out, payload);
        let data = cpu.read_mem(0x1000, 56).unwrap();
        assert_eq!(word(&data, 8), compressed.len() as u32);
        assert_eq!(word(&data, 20), payload.len() as u32);
        assert_eq!(call(&mut cpu, &mut kernel, end, &[0x1000]), 0);
        assert_eq!(word(&cpu.read_mem(0x1000, 56).unwrap(), 28), 0);
        assert_eq!(call(&mut cpu, &mut kernel, end, &[0x1000]), (-2i32) as u32);
    }
    #[test]
    fn guest_compress_roundtrip_and_small_destination() {
        let mut cpu = StubCpu::new();
        cpu.map_region(0x1000, 0x4000, Prot::READ | Prot::WRITE)
            .unwrap();
        let mut kernel = crate::gx::tests::fresh_kernel();
        let input = b"saved progress saved progress saved progress";
        cpu.write_mem(0x1000, input).unwrap();
        cpu.write_mem(0x1800, &256u32.to_le_bytes()).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                compress,
                &[0x2000, 0x1800, 0x1000, input.len() as u32]
            ),
            0
        );
        let count = word(&cpu.read_mem(0x1800, 4).unwrap(), 0);
        let encoded = cpu.read_mem(0x2000, count).unwrap();
        let mut decoder = pocket_kernel::zlib::ZlibStreams::default();
        let handle = decoder.init().unwrap();
        let mut output = [0; 256];
        let step = decoder.inflate(handle, &encoded, &mut output, 4).unwrap();
        assert_eq!(step.status, 1);
        assert_eq!(&output[..step.written], input);
        cpu.write_mem(0x1800, &1u32.to_le_bytes()).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                compress,
                &[0x2000, 0x1800, 0x1000, input.len() as u32]
            ),
            (-5i32) as u32
        );
    }
}
