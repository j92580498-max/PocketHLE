//! Exercise loader wiring, including ordinals, adjacent stopping hooks and GetProcAddress exports.
#![cfg(feature = "unicorn")]
use pocket_core::{
    cpu::{regs::ArmReg, unicorn::UnicornCpu, Cpu, StopReason},
    kernel::Process,
    pe::{self, ImportBinding, ImportSymbol, LoadedImage, LoadedSection},
    winceapi::{resolve_ordinal, WinCeDispatcher},
};

#[test]
fn static_ordinal_and_dynamic_arithmetic_return_without_entering_the_kernel_loop() {
    let names = [
        "__rt_sdiv64by64",
        "Sleep",
        "__rt_udiv64by64",
        "__rt_srem64by64",
        "__rt_urem64by64",
    ];
    let image = LoadedImage {
        source_path: "arithmetic_inline_regression".into(),
        machine: pe::machine::ARM,
        subsystem: pe::subsystem::WINDOWS_CE_GUI,
        image_base: 0x10000,
        size_of_image: 0x2000,
        stack_reserve: 0x10000,
        entry_point: 0x1000,
        sections: vec![LoadedSection {
            name: ".text".into(),
            virtual_address: 0x1000,
            virtual_size: 0x1000,
            characteristics: 0xe0000020,
            data: vec![0; 0x1000],
        }],
        imports: names
            .iter()
            .enumerate()
            .map(|(index, name)| ImportSymbol {
                dll: "COREDLL.dll".into(),
                binding: if index == 0 {
                    ImportBinding::Ordinal(2000)
                } else {
                    ImportBinding::Name((*name).into())
                },
                iat_va: 0x11800 + index as u32 * 4,
            })
            .collect(),
        exports: Default::default(),
        resources: vec![],
        managed_runtime: None,
    };
    let mut cpu = UnicornCpu::new().unwrap();
    let dispatcher = WinCeDispatcher::new();
    let mut process = Process::map_into(image, &mut cpu, &resolve_ordinal, &dispatcher).unwrap();
    cpu.add_code_hook(0x11100).unwrap();
    process.state.thread_last_errors.insert(0, 0x12345678);
    let dividend = (-1234567890123i64) as u64;
    let divisor = 97u64;
    for name in names.into_iter().filter(|name| *name != "Sleep") {
        let entries: Vec<_> = process
            .thunks
            .iter()
            .filter(|thunk| {
                thunk.dll.eq_ignore_ascii_case("coredll.dll")
                    && thunk.friendly_name.as_deref() == Some(name)
            })
            .map(|thunk| (thunk.thunk_va, thunk.iat_va))
            .collect();
        assert!(entries.iter().any(|(_, iat)| *iat != 0), "static {name}");
        assert!(entries.iter().any(|(_, iat)| *iat == 0), "dynamic {name}");
        let expected = match name {
            "__rt_sdiv64by64" => ((dividend as i64) / divisor as i64) as u64,
            "__rt_udiv64by64" => dividend / divisor,
            "__rt_srem64by64" => ((dividend as i64) % divisor as i64) as u64,
            _ => dividend % divisor,
        };
        for (entry, _) in entries {
            cpu.write_reg(ArmReg::R0, dividend as u32).unwrap();
            cpu.write_reg(ArmReg::R1, (dividend >> 32) as u32).unwrap();
            cpu.write_reg(ArmReg::R2, divisor as u32).unwrap();
            cpu.write_reg(ArmReg::R3, 0).unwrap();
            cpu.write_reg(ArmReg::Lr, 0x11100).unwrap();
            assert_eq!(
                cpu.run_until_hook(entry, 100).unwrap(),
                StopReason::Hook(0x11100)
            );
            let actual = u64::from(cpu.read_reg(ArmReg::R0).unwrap())
                | (u64::from(cpu.read_reg(ArmReg::R1).unwrap()) << 32);
            assert_eq!(actual, expected, "{name}");
        }
    }
    // A neighbouring ordinary API must still stop and be dispatched normally.
    let sleep = cpu.read_u32_le(0x11804).unwrap();
    assert_eq!(
        cpu.run_until_hook(sleep, 100).unwrap(),
        StopReason::Hook(sleep)
    );
    assert_eq!(process.state.thread_last_errors[&0], 0x12345678);
}
