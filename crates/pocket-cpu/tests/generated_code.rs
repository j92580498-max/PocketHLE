#![cfg(feature = "unicorn")]

use pocket_cpu::{unicorn::UnicornCpu, Arch, Cpu, Prot, StopReason};

const DATA_PAGE: u32 = 0x0001_0000;
const FIRST_HOOK: u32 = 0x0002_0000;
const SECOND_HOOK: u32 = 0x0003_0000;

#[test]
fn flush_instruction_cache_runs_and_refreshes_guest_generated_code() {
    let mut cpu = UnicornCpu::new_for_arch(Arch::Arm).unwrap();
    cpu.map_region(DATA_PAGE, 0x1000, Prot::READ | Prot::WRITE)
        .unwrap();
    cpu.map_region(FIRST_HOOK, 0x1000, Prot::READ | Prot::EXEC)
        .unwrap();
    cpu.map_region(SECOND_HOOK, 0x1000, Prot::READ | Prot::EXEC)
        .unwrap();
    cpu.write_mem(DATA_PAGE, &[0x04, 0xf0, 0x1f, 0xe5, 0x00, 0x00, 0x02, 0x00])
        .unwrap();
    cpu.add_code_hook(FIRST_HOOK).unwrap();
    cpu.add_code_hook(SECOND_HOOK).unwrap();

    cpu.flush_instruction_cache(DATA_PAGE, 8).unwrap();
    assert_eq!(
        cpu.run_until_hook(DATA_PAGE, 16).unwrap(),
        StopReason::Hook(FIRST_HOOK)
    );

    cpu.write_mem(DATA_PAGE + 4, &SECOND_HOOK.to_le_bytes())
        .unwrap();
    cpu.flush_instruction_cache(DATA_PAGE, 8).unwrap();
    assert_eq!(
        cpu.run_until_hook(DATA_PAGE, 16).unwrap(),
        StopReason::Hook(SECOND_HOOK)
    );
}

#[test]
fn first_fetch_promotes_guest_code_after_unicorn_returns() {
    let mut cpu = UnicornCpu::new_for_arch(Arch::Arm).unwrap();
    cpu.map_region(DATA_PAGE, 0x1000, Prot::READ | Prot::WRITE)
        .unwrap();
    cpu.map_region(FIRST_HOOK, 0x1000, Prot::READ | Prot::EXEC)
        .unwrap();
    cpu.write_mem(DATA_PAGE, &[0x04, 0xf0, 0x1f, 0xe5, 0x00, 0x00, 0x02, 0x00])
        .unwrap();
    cpu.add_code_hook(FIRST_HOOK).unwrap();

    assert_eq!(
        cpu.run_until_hook(DATA_PAGE, 16).unwrap(),
        StopReason::Hook(FIRST_HOOK)
    );
}

#[test]
fn xscale_cp0_probe_falls_back_when_the_optional_coprocessor_is_absent() {
    const CODE: u32 = 0x0004_0000;
    const AFTER_PROBE: u32 = CODE + 24;

    let mut cpu = UnicornCpu::new_for_arch(Arch::Arm).unwrap();
    cpu.map_region(CODE, 0x1000, Prot::ALL).unwrap();
    cpu.write_mem(
        CODE,
        &[
            0x01, 0x04, 0xa0, 0xe3, // mov r0, #0x01000000
            0x01, 0x14, 0xa0, 0xe3, // mov r1, #0x01000000
            0x00, 0x00, 0x41, 0xec, // mcrr p0, #0, r0, r1, c0
            0x20, 0x00, 0xa0, 0xe3, // mov r0, #0x20
            0x20, 0x10, 0xa0, 0xe3, // mov r1, #0x20
            0x00, 0x00, 0x51, 0xec, // mrrc p0, #0, r0, r1, c0
        ],
    )
    .unwrap();
    cpu.add_code_hook(AFTER_PROBE).unwrap();

    assert_eq!(
        cpu.run_until_hook(CODE, 32).unwrap(),
        StopReason::Hook(AFTER_PROBE)
    );
    assert_eq!(cpu.read_reg(pocket_cpu::regs::ArmReg::R0).unwrap(), 0x20);
    assert_eq!(cpu.read_reg(pocket_cpu::regs::ArmReg::R1).unwrap(), 0x20);
}
