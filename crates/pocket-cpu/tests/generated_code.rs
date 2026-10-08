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
