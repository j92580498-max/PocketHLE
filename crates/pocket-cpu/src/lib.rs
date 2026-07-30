//! CPU emulation abstractions.
//!
//! `pocket-cpu` exposes a backend-agnostic [`Cpu`] trait that the rest
//! of the emulator talks to. Two backends are provided:
//!
//! * [`stub`] — a no-op CPU that just stores register & memory state.
//!   Always built; lets the workspace compile without native deps and
//!   is enough for tests that only exercise the loader / kernel stubs.
//! * [`unicorn`] — wraps the [Unicorn](https://www.unicorn-engine.org/)
//!   emulator. Behind the `unicorn` Cargo feature because it requires
//!   cmake / a C compiler at build time.
//!
//! The trait is intentionally tiny — we only need ARM-mode features
//! relevant to user-mode PE32 execution under HLE.

use thiserror::Error;

pub mod stub;
#[cfg(feature = "unicorn")]
pub mod unicorn;

pub mod regs {
    /// Guest ABI register slots used by the WinCE HLE layer. On ARM
    /// these map to r0-r12/sp/lr/pc; on MIPS they map to a0-a3,
    /// s0-s8, sp/ra/pc so handlers can use one architecture-neutral
    /// calling-convention view.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u8)]
    pub enum ArmReg {
        R0 = 0,
        R1 = 1,
        R2 = 2,
        R3 = 3,
        R4 = 4,
        R5 = 5,
        R6 = 6,
        R7 = 7,
        R8 = 8,
        R9 = 9,
        R10 = 10,
        R11 = 11,
        R12 = 12,
        Sp = 13,
        Lr = 14,
        Pc = 15,
        Cpsr = 16,
    }

    /// 32-bit little-endian MIPS32 general-purpose registers and the PC.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u8)]
    pub enum MipsReg {
        R0 = 0,
        R1 = 1,
        R2 = 2,
        R3 = 3,
        R4 = 4,
        R5 = 5,
        R6 = 6,
        R7 = 7,
        R8 = 8,
        R9 = 9,
        R10 = 10,
        R11 = 11,
        R12 = 12,
        R13 = 13,
        R14 = 14,
        R15 = 15,
        R16 = 16,
        R17 = 17,
        R18 = 18,
        R19 = 19,
        R20 = 20,
        R21 = 21,
        R22 = 22,
        R23 = 23,
        R24 = 24,
        R25 = 25,
        R26 = 26,
        R27 = 27,
        R28 = 28,
        R29 = 29,
        R30 = 30,
        R31 = 31,
        Pc = 32,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    /// 32-bit ARM, little endian.
    Arm,
    /// 32-bit MIPS little endian used by Windows CE.
    Mips,
}

bitflags::bitflags! {
    /// Memory protection flags. Mirrors Unicorn / `mmap` semantics.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub struct Prot: u32 {
        const NONE = 0;
        const READ = 1;
        const WRITE = 2;
        const EXEC = 4;
        const ALL = Self::READ.bits() | Self::WRITE.bits() | Self::EXEC.bits();
    }
}

#[derive(Debug, Error)]
pub enum CpuError {
    #[error("unsupported feature: {0}")]
    Unsupported(&'static str),
    #[error("invalid memory access at va=0x{va:08x} size={size}")]
    BadMemory { va: u32, size: u32 },
    #[error("execution stopped at va=0x{0:08x}")]
    Stopped(u32),
    #[error("backend error: {0}")]
    Backend(String),
}

/// Reason `run_until_hook` returned to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Execution counter exhausted.
    InstructionLimit,
    /// One of the registered hooks fired.
    Hook(u32),
    /// `Cpu::request_stop` was called.
    Requested,
    /// The CPU executed beyond mapped memory.
    OutOfBounds,
}

/// Common interface to a CPU emulation backend.
pub trait Cpu {
    fn arch(&self) -> Arch;

    fn map_region(&mut self, va: u32, size: u32, prot: Prot) -> Result<(), CpuError>;
    fn write_mem(&mut self, va: u32, data: &[u8]) -> Result<(), CpuError>;
    fn read_mem(&mut self, va: u32, len: u32) -> Result<Vec<u8>, CpuError>;

    /// Like [`Cpu::read_mem`] but writes into a caller-provided
    /// buffer. Hot paths (e.g. the per-frame GAPI flush which moves
    /// 150 KiB of pixels every `GXEndDraw`) call this with a
    /// pre-allocated `Vec<u8>` so we don't allocate a fresh
    /// `Vec` per frame. The default implementation falls back to
    /// `read_mem` so backends that don't care can stay simple.
    fn read_mem_into(&mut self, va: u32, dst: &mut [u8]) -> Result<(), CpuError> {
        let bytes = self.read_mem(va, dst.len() as u32)?;
        if bytes.len() != dst.len() {
            return Err(CpuError::BadMemory {
                va,
                size: dst.len() as u32,
            });
        }
        dst.copy_from_slice(&bytes);
        Ok(())
    }

    /// Read a single byte without going through `read_mem`'s
    /// per-call `Vec<u8>` allocation. Used by the WinCE string
    /// helpers (`read_cstr`, `read_wstr`) which used to call
    /// `read_mem(va, 1)` once per scanned byte; on Derby that
    /// alone showed up as the dominating per-frame cost in `perf`.
    fn read_u8(&mut self, va: u32) -> Result<u8, CpuError> {
        let mut buf = [0u8; 1];
        self.read_mem_into(va, &mut buf)?;
        Ok(buf[0])
    }

    /// Little-endian `u16` read using a stack buffer.
    fn read_u16_le(&mut self, va: u32) -> Result<u16, CpuError> {
        let mut buf = [0u8; 2];
        self.read_mem_into(va, &mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    /// Little-endian `u32` read using a stack buffer. Hot in the
    /// argument-fetch path for any handler whose 5th+ argument
    /// lives on the stack.
    fn read_u32_le(&mut self, va: u32) -> Result<u32, CpuError> {
        let mut buf = [0u8; 4];
        self.read_mem_into(va, &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    /// Stack offset of the first argument beyond the register argument
    /// registers. ARM WinCE uses the current SP; MIPS32 O32 uses the
    /// caller-provided four-word argument home area first.
    fn stack_arg_offset(&self) -> u32 {
        if self.arch() == Arch::Mips {
            16
        } else {
            0
        }
    }

    fn read_reg(&mut self, reg: regs::ArmReg) -> Result<u32, CpuError>;
    fn write_reg(&mut self, reg: regs::ArmReg, value: u32) -> Result<(), CpuError>;

    fn read_arg(&mut self, index: u8) -> Result<u32, CpuError> {
        let reg = match index {
            0 => regs::ArmReg::R0,
            1 => regs::ArmReg::R1,
            2 => regs::ArmReg::R2,
            3 => regs::ArmReg::R3,
            _ => return Err(CpuError::Unsupported("register argument index")),
        };
        self.read_reg(reg)
    }

    fn write_arg(&mut self, index: u8, value: u32) -> Result<(), CpuError> {
        let reg = match index {
            0 => regs::ArmReg::R0,
            1 => regs::ArmReg::R1,
            2 => regs::ArmReg::R2,
            3 => regs::ArmReg::R3,
            _ => return Err(CpuError::Unsupported("register argument index")),
        };
        self.write_reg(reg, value)
    }

    fn read_return(&mut self) -> Result<u32, CpuError> {
        self.read_reg(regs::ArmReg::R0)
    }

    fn write_return(&mut self, value: u32) -> Result<(), CpuError> {
        self.write_reg(regs::ArmReg::R0, value)
    }

    fn write_return_pair(&mut self, first: u32, second: u32) -> Result<(), CpuError> {
        self.write_return(first)?;
        self.write_reg(regs::ArmReg::R1, second)
    }

    /// Register an executable address that should immediately stop
    /// emulation when the PC reaches it. Used to install IAT thunks
    /// for unimplemented imports.
    fn add_code_hook(&mut self, va: u32) -> Result<(), CpuError>;

    /// Run starting at `start_va` until either an [`StopReason::Hook`]
    /// fires, `max_instructions` is reached, or `request_stop` is
    /// called from another hook. Set `max_instructions` to `0` to run
    /// without a limit.
    fn run_until_hook(
        &mut self,
        start_va: u32,
        max_instructions: u64,
    ) -> Result<StopReason, CpuError>;

    fn request_stop(&mut self);
}

/// Format a multi-line dump of every general purpose register.
/// Implemented as a free function so it works with any [`Cpu`] backend.
pub fn dump_regs(cpu: &mut dyn Cpu) -> String {
    use regs::ArmReg::*;
    let mut out = String::new();
    let order = [
        R0, R1, R2, R3, R4, R5, R6, R7, R8, R9, R10, R11, R12, Sp, Lr, Pc, Cpsr,
    ];
    for (i, r) in order.iter().enumerate() {
        let v = cpu.read_reg(*r).unwrap_or(0);
        let label = format!("{r:?}");
        out.push_str(&format!("  {label:>4}=0x{v:08x}"));
        if i % 4 == 3 {
            out.push('\n');
        }
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Read up to `len` bytes around `va` and format them as hex. Used in
/// crash dumps to show what code the CPU was about to execute.
pub fn dump_mem_around(cpu: &mut dyn Cpu, va: u32, span: u32) -> String {
    let start = va.saturating_sub(span);
    let total = span.saturating_mul(2);
    match cpu.read_mem(start, total) {
        Ok(bytes) => {
            let mut out = String::new();
            for chunk in bytes.chunks(16).enumerate() {
                let (i, c) = chunk;
                out.push_str(&format!("  0x{:08x}:", start + (i as u32) * 16));
                for b in c {
                    out.push_str(&format!(" {b:02x}"));
                }
                out.push('\n');
            }
            out
        }
        Err(_) => format!("  <unreadable around 0x{va:08x}>\n"),
    }
}

/// Walk the ARM APCS frame chain from `sp` and format the return
/// addresses that land inside the loaded image.
///
/// Pocket PC compilers emit `mov ip, sp; stmdb sp!, {..., ip, lr, pc}`
/// prologues, so the saved `LR` slots sit in a predictable spot on the
/// stack. A crash inside a deeply nested call is almost impossible to
/// diagnose from `PC` alone -- what matters is which caller passed the
/// bad pointer down -- so scan the top of the stack and report every
/// word that looks like a code address in `[image_base, image_end)`.
pub fn dump_stack_code_addrs(
    cpu: &mut dyn Cpu,
    sp: u32,
    words: u32,
    image_base: u32,
    image_end: u32,
) -> String {
    let bytes = match cpu.read_mem(sp, words.saturating_mul(4)) {
        Ok(b) => b,
        Err(_) => return format!("  <unreadable stack at 0x{sp:08x}>\n"),
    };
    let mut out = String::new();
    out.push_str("  stack return-address candidates (sp first):\n");
    let mut hits = 0usize;
    for (i, chunk) in bytes.chunks_exact(4).enumerate() {
        let v = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if v >= image_base && v < image_end {
            out.push_str(&format!("    [sp+0x{:03x}] = 0x{v:08x}\n", (i as u32) * 4));
            hits += 1;
        }
    }
    if hits == 0 {
        out.push_str("    <none>\n");
    }
    out
}

pub fn round_up_to_page(size: u32) -> u32 {
    const PAGE: u32 = 0x1000;
    (size + PAGE - 1) & !(PAGE - 1)
}
