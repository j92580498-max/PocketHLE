//! Colors uses the SDK registry/event protocol, not a new brightness export.
use crate::bluetooth::tests::{call, setup};
use pocket_cpu::Cpu;
use pocket_kernel::DispatchOutcome;
fn value(out: DispatchOutcome) -> u32 {
    let DispatchOutcome::ReturnedR0(v) = out else {
        panic!("unexpected retry")
    };
    v
}
#[test]
fn colors_slider_registry_and_event_drive_host_brightness() {
    let (mut cpu, mut kernel, mut dispatcher) = setup();
    kernel.registry = pocket_kernel::registry::Registry::with_device_defaults();
    let mut wstr = |address, text: &str| {
        cpu.write_mem(
            address,
            &text
                .encode_utf16()
                .chain(Some(0))
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap()
    };
    wstr(0x1000, r"ControlPanel\Backlight");
    wstr(0x1100, "OnLevel");
    wstr(0x1200, "DimLevel");
    wstr(0x1300, "BackLightChangeEvent");
    macro_rules! api {
        ($name:expr,$args:expr) => {
            value(call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "coredll.dll",
                $name,
                $args,
            ))
        };
    }
    api!("SetLastError", &[0x1234]);
    assert_eq!(
        api!("RegOpenKeyExW", &[0x80000001, 0x1000, 0, 0, 0x1400]),
        0
    );
    let key = cpu.read_u32_le(0x1400).unwrap();
    cpu.write_mem(0x1500, &4u32.to_le_bytes()).unwrap();
    assert_eq!(
        api!(
            "RegQueryValueExW",
            &[key, 0x1100, 0, 0x1504, 0x1508, 0x1500]
        ),
        0
    );
    assert_eq!(cpu.read_u32_le(0x1508).unwrap(), 11);
    let event = api!("CreateEventW", &[0, 0, 0, 0x1300]);
    assert_ne!(event, 0);
    // CreateEvent reports whether the named event already existed. Registry
    // writes and successful event/wait calls preserve the error set afterwards.
    api!("SetLastError", &[0x1234]);
    kernel.framebuffer.pixels[0..2].copy_from_slice(&0x7befu16.to_le_bytes());
    let pixels = kernel.framebuffer.pixels.clone();
    let normal = kernel.framebuffer.snapshot_rgba8888();
    for (level, dim) in [(1u32, 0u32), (11, 0), (21, 10), (31, 20)] {
        cpu.write_mem(0x1600, &level.to_le_bytes()).unwrap();
        cpu.write_mem(0x1604, &dim.to_le_bytes()).unwrap();
        assert_eq!(api!("RegSetValueExW", &[key, 0x1100, 0, 4, 0x1600, 4]), 0);
        assert_eq!(api!("RegSetValueExW", &[key, 0x1200, 0, 4, 0x1604, 4]), 0);
        assert_eq!(api!("EventModify", &[event, 3]), 1);
        assert_eq!(api!("WaitForSingleObject", &[event, 0]), 0);
        kernel.sync_backlight();
        assert_eq!(kernel.framebuffer.backlight_level(), level);
        let displayed = kernel.framebuffer.snapshot_rgba8888();
        assert_eq!(kernel.framebuffer.pixels, pixels);
        if level < 11 {
            assert!(displayed[0] < normal[0]);
        } else if level == 11 {
            assert_eq!(displayed, normal);
        } else {
            assert!(displayed[0] > normal[0]);
        }
    }
    // Absent, zero, invalid and wrong-type values all restore the second position,
    // including when the preceding valid slider value was the minimum.
    for invalid in [0u32, 32, u32::MAX] {
        cpu.write_mem(0x1600, &invalid.to_le_bytes()).unwrap();
        assert_eq!(api!("RegSetValueExW", &[key, 0x1100, 0, 4, 0x1600, 4]), 0);
        kernel.sync_backlight();
        assert_eq!(kernel.framebuffer.backlight_level(), 11);
        assert_eq!(kernel.framebuffer.snapshot_rgba8888(), normal);
    }
    assert_eq!(api!("RegSetValueExW", &[key, 0x1100, 0, 3, 0x1600, 4]), 0);
    kernel.sync_backlight();
    assert_eq!(kernel.framebuffer.snapshot_rgba8888(), normal);
    assert_eq!(api!("RegDeleteValueW", &[key, 0x1100]), 0);
    kernel.framebuffer.set_backlight_level(1);
    kernel.sync_backlight();
    assert_eq!(kernel.framebuffer.backlight_level(), 11);
    assert_eq!(kernel.framebuffer.snapshot_rgba8888(), normal);
    assert_eq!(api!("GetLastError", &[]), 0x1234);
    assert_eq!(api!("RegCloseKey", &[key]), 0);
    assert_eq!(api!("CloseHandle", &[event]), 1);
}
