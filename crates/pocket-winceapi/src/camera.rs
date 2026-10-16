//! CAM1 stream-driver ABI from the Gizmondo developer SDK.
use crate::CallCtx;
use pocket_cpu::{regs::ArmReg, Prot};
use pocket_kernel::{camera, DispatchOutcome, KernelError};
use std::time::{Duration, Instant};
const BASE: u32 = 0x01010000;
fn key(ctx: &mut CallCtx<'_>) -> Result<(usize, u32, u32), KernelError> {
    Ok((
        ctx.kernel.current_thread,
        ctx.thunk.thunk_va,
        ctx.cpu.read_reg(ArmReg::Sp)?,
    ))
}
fn finish(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    let k = key(ctx)?;
    ctx.kernel.vfs.camera_deadlines.remove(&k);
    crate::bluetooth::finish_wait(ctx)
}
fn fail(ctx: &mut CallCtx<'_>, error: u32) -> Result<DispatchOutcome, KernelError> {
    finish(ctx)?;
    crate::coredll::set_thread_error(ctx, error);
    Ok(DispatchOutcome::ReturnedR0(0))
}
fn access(ctx: &mut CallCtx<'_>, ptr: u32, len: u32, prot: Prot) -> bool {
    ptr != 0 && ptr.checked_add(len).is_some() && ctx.cpu.check_guest_access(ptr, len, prot).is_ok()
}
pub(crate) fn control(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let code = ctx.arg_u32(1)?;
    let input = ctx.arg_u32(2)?;
    let input_len = ctx.arg_u32(3)?;
    let output = ctx.arg_u32(4)?;
    let output_len = ctx.arg_u32(5)?;
    let returned = ctx.arg_u32(6)?;
    let overlapped = ctx.arg_u32(7)?;
    let Some(open) = ctx.kernel.vfs.camera_open(handle) else {
        return fail(ctx, 6);
    };
    if returned != 0 && !access(ctx, returned, 4, Prot::WRITE) {
        return fail(ctx, 87);
    }
    if returned != 0 {
        ctx.cpu.write_mem(returned, &0u32.to_le_bytes())?;
    }
    if overlapped != 0 {
        return fail(ctx, 50);
    }
    // SDK camera IOCTLs use FILE_ANY_ACCESS; the service permission gate applies
    // at open/start, including handles opened only for DeviceIoControl.
    let function = if code & 0xffff0000 == BASE {
        (code & 0xffff) >> 2
    } else {
        0
    };
    if code & 3 != 0 {
        return fail(ctx, 50);
    }
    let mut device = open.device.lock().unwrap();
    let mut bytes = Vec::new();
    let result = match function {
        2101 => {
            if input_len != 16 || !access(ctx, input, 16, Prot::READ) {
                return fail(ctx, 87);
            }
            let words = ctx.cpu.read_mem(input, 16)?;
            let format = std::array::from_fn(|i| {
                u32::from_le_bytes(words[i * 4..i * 4 + 4].try_into().unwrap())
            });
            device.set_format(format)
        }
        2102 => {
            if output_len < 16 {
                return fail(ctx, 122);
            }
            if !access(ctx, output, 16, Prot::WRITE) {
                return fail(ctx, 87);
            }
            for word in device.format {
                bytes.extend_from_slice(&word.to_le_bytes());
            }
            Ok(())
        }
        2103 => device.start(),
        2104 => {
            device.stop();
            Ok(())
        }
        2105 | 2106 => {
            if input_len != 16 || !access(ctx, input, 16, Prot::READ | Prot::WRITE) {
                return fail(ctx, 87);
            }
            let info = ctx.cpu.read_mem(input, 16)?;
            let word = |i| u32::from_le_bytes(info[i..i + 4].try_into().unwrap());
            let (w, h) = if function == 2105 {
                (device.format[2], device.format[3])
            } else {
                (device.format[0], device.format[1])
            };
            if word(0) != w || word(4) != h {
                return fail(ctx, 87);
            }
            let required = if function == 2105 {
                w * h * 2
            } else {
                w * h * 3 / 2
            };
            if output_len < required {
                return fail(ctx, 122);
            }
            if !access(ctx, output, required, Prot::WRITE) {
                return fail(ctx, 87);
            }
            let frame = match device.latest() {
                Ok(frame) => frame,
                Err(e) => return fail(ctx, e),
            };
            let now = Instant::now();
            if let Some(frame) = frame.filter(|_| function != 2105 || now >= device.next_preview) {
                bytes = if function == 2105 {
                    camera::preview(&frame, w, h)
                } else {
                    camera::capture(&frame)
                };
                device.consumed(frame.serial);
                if function == 2105 {
                    device.next_preview = now + Duration::from_millis(50);
                }
                ctx.cpu
                    .write_mem(input + 8, &device.frame_count.to_le_bytes())?;
                Ok(())
            } else {
                let timeout = word(12);
                let k = key(ctx)?;
                let deadline = *ctx
                    .kernel
                    .vfs
                    .camera_deadlines
                    .entry(k)
                    .or_insert_with(|| now + Duration::from_millis(timeout as u64));
                if now >= deadline {
                    return fail(ctx, 1460);
                }
                drop(device);
                return crate::bluetooth::retry(ctx);
            }
        }
        _ => Err(50),
    };
    drop(device);
    if let Err(error) = result {
        return fail(ctx, error);
    }
    if !bytes.is_empty() {
        ctx.cpu.write_mem(output, &bytes)?;
    }
    if returned != 0 {
        ctx.cpu
            .write_mem(returned, &(bytes.len() as u32).to_le_bytes())?;
    }
    finish(ctx)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bluetooth::tests::{call, setup},
        WinCeDispatcher,
    };
    use pocket_cpu::{stub::StubCpu, Cpu};
    use pocket_kernel::{
        camera::{Backend, Capture, Frame, Service},
        KernelState,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    struct Fake {
        frame: Arc<Mutex<Option<Arc<Frame>>>>,
        drops: Arc<AtomicUsize>,
    }
    struct Stream {
        frame: Arc<Mutex<Option<Arc<Frame>>>>,
        drops: Arc<AtomicUsize>,
    }
    impl Backend for Fake {
        fn start(&self) -> camera::Result<Box<dyn Capture>> {
            Ok(Box::new(Stream {
                frame: self.frame.clone(),
                drops: self.drops.clone(),
            }))
        }
    }
    impl Capture for Stream {
        fn latest(&mut self) -> camera::Result<Option<Arc<Frame>>> {
            Ok(self.frame.lock().unwrap().clone())
        }
    }
    impl Drop for Stream {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Rig {
        cpu: StubCpu,
        k: KernelState,
        d: WinCeDispatcher,
        frame: Arc<Mutex<Option<Arc<Frame>>>>,
        drops: Arc<AtomicUsize>,
        h: u32,
    }
    impl Rig {
        fn new() -> Self {
            let (mut cpu, mut k, mut d) = setup();
            cpu.map_region(0x10000, 0xa0000, Prot::READ | Prot::WRITE)
                .unwrap();
            let frame = Arc::new(Mutex::new(Some(Arc::new(Frame {
                width: 2,
                height: 2,
                serial: 1,
                rgb: vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
            }))));
            let drops = Arc::new(AtomicUsize::new(0));
            let service = Service::with_backend(Arc::new(Fake {
                frame: frame.clone(),
                drops: drops.clone(),
            }));
            service.set_allowed(true);
            k.vfs.set_camera_service(service);
            cpu.write_mem(
                0x1000,
                &"CAM1:"
                    .encode_utf16()
                    .chain(Some(0))
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            let h = match call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "CreateFileW",
                &[0x1000, 0xc0000000, 0, 0, 3, 0, 0],
            ) {
                DispatchOutcome::ReturnedR0(h) => h,
                _ => panic!(),
            };
            assert_ne!(h, u32::MAX);
            Self {
                cpu,
                k,
                d,
                frame,
                drops,
                h,
            }
        }
        fn ctl(
            &mut self,
            function: u32,
            input: u32,
            input_len: u32,
            output: u32,
            output_len: u32,
        ) -> DispatchOutcome {
            call(
                &mut self.cpu,
                &mut self.k,
                &mut self.d,
                "coredll.dll",
                "DeviceIoControl",
                &[
                    self.h,
                    BASE | (function << 2),
                    input,
                    input_len,
                    output,
                    output_len,
                    0x1300,
                    0,
                ],
            )
        }
        fn error(&mut self) -> u32 {
            match call(
                &mut self.cpu,
                &mut self.k,
                &mut self.d,
                "coredll.dll",
                "GetLastError",
                &[],
            ) {
                DispatchOutcome::ReturnedR0(v) => v,
                _ => panic!(),
            }
        }
        fn info(&mut self, w: u32, h: u32, timeout: u32) {
            self.cpu
                .write_mem(
                    0x1100,
                    &[w, h, 0, timeout]
                        .into_iter()
                        .flat_map(u32::to_le_bytes)
                        .collect::<Vec<_>>(),
                )
                .unwrap();
        }
        fn start(&mut self) {
            assert_eq!(self.ctl(2103, 0, 0, 0, 0), DispatchOutcome::ReturnedR0(1));
        }
    }
    #[test]
    fn closed_camera_handles_set_last_error_after_each_reopen() {
        let mut r = Rig::new();
        for cycle in 0..8 {
            r.start();
            r.info(320, 240, 1000);
            assert_eq!(
                r.ctl(2105, 0x1100, 16, 0x10000, 153600),
                DispatchOutcome::ReturnedR0(1)
            );
            assert_eq!(r.ctl(2107, 0, 0, 0, 0), DispatchOutcome::ReturnedR0(0));
            assert_eq!(r.error(), 50);
            assert_eq!(r.ctl(2104, 0, 0, 0, 0), DispatchOutcome::ReturnedR0(1));
            assert_eq!(
                call(
                    &mut r.cpu,
                    &mut r.k,
                    &mut r.d,
                    "coredll.dll",
                    "CloseHandle",
                    &[r.h]
                ),
                DispatchOutcome::ReturnedR0(1)
            );
            assert_eq!(r.drops.load(Ordering::SeqCst), cycle + 1);
            assert!(!r.k.vfs.is_handle(r.h));
            assert_eq!(
                call(
                    &mut r.cpu,
                    &mut r.k,
                    &mut r.d,
                    "coredll.dll",
                    "SetLastError",
                    &[50]
                ),
                DispatchOutcome::ReturnedR0(0)
            );
            assert_eq!(
                r.ctl(2102, 0, 0, 0x1400, 16),
                DispatchOutcome::ReturnedR0(0)
            );
            assert_eq!(r.error(), 6);
            assert!(r.k.vfs.camera_deadlines.is_empty());
            if cycle != 7 {
                r.h = match call(
                    &mut r.cpu,
                    &mut r.k,
                    &mut r.d,
                    "coredll.dll",
                    "CreateFileW",
                    &[0x1000, 0xc0000000, 0, 0, 3, 0, 0],
                ) {
                    DispatchOutcome::ReturnedR0(h) => h,
                    _ => panic!(),
                };
                assert_ne!(r.h, u32::MAX);
            }
        }
    }

    #[test]
    fn sdk_stop_set_start_preview_capture_sequence() {
        let mut r = Rig::new();
        assert_eq!(r.ctl(2104, 0, 0, 0, 0), DispatchOutcome::ReturnedR0(1));
        r.cpu
            .write_mem(
                0x1200,
                &[640u32, 480, 8, 8]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert_eq!(
            r.ctl(2101, 0x1200, 16, 0, 0),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(
            r.ctl(2102, 0, 0, 0x1400, 16),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(
            r.cpu.read_mem(0x1200, 16).unwrap(),
            r.cpu.read_mem(0x1400, 16).unwrap()
        );
        r.start();
        r.info(8, 8, 1000);
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0x10000, 128),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(r.cpu.read_mem(0x10000, 2).unwrap(), [0x1f, 0]);
        assert_eq!(r.cpu.read_u32_le(0x1108).unwrap(), 1);
        assert_eq!(r.cpu.read_u32_le(0x1300).unwrap(), 128);
        r.frame.lock().unwrap().as_mut().map(|frame| {
            *frame = Arc::new(Frame {
                width: 1,
                height: 1,
                serial: 2,
                rgb: vec![255, 0, 0],
            })
        });
        r.info(640, 480, 1000);
        assert_eq!(
            r.ctl(2106, 0x1100, 16, 0x10000, 460800),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(r.cpu.read_u32_le(0x1300).unwrap(), 460800);
        assert_eq!(r.cpu.read_mem(0x10000, 1).unwrap(), [82]);
        assert_eq!(r.cpu.read_mem(0x10000 + 307200, 1).unwrap(), [90]);
        assert_eq!(r.cpu.read_mem(0x10000 + 384000, 1).unwrap(), [240]);
        assert_eq!(r.cpu.read_u32_le(0x1108).unwrap(), 2);
        assert_eq!(r.ctl(2104, 0, 0, 0, 0), DispatchOutcome::ReturnedR0(1));
        assert_eq!(r.drops.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn errors_and_invalid_output_do_not_consume_a_frame() {
        let mut r = Rig::new();
        r.info(320, 240, 0);
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0x10000, 153600),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(r.error(), 21);
        r.start();
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0, 153600),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(r.error(), 87);
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0x10000, 2),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(r.error(), 122);
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0x10000, 153600),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(r.ctl(2107, 0, 0, 0, 0), DispatchOutcome::ReturnedR0(0));
        assert_eq!(r.error(), 50);
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0x10000, 153600),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(r.error(), 1460);
        assert!(r.k.vfs.camera_deadlines.is_empty());
    }
    #[test]
    fn pending_capture_yields_and_deadline_survives_retries() {
        let mut r = Rig::new();
        r.start();
        *r.frame.lock().unwrap() = None;
        r.info(320, 240, 1000);
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0x10000, 153600),
            DispatchOutcome::JumpTo(0x70000000)
        );
        let key = *r.k.vfs.camera_deadlines.keys().next().unwrap();
        let deadline = r.k.vfs.camera_deadlines[&key];
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0x10000, 153600),
            DispatchOutcome::JumpTo(0x70000000)
        );
        assert_eq!(r.k.vfs.camera_deadlines[&key], deadline);
        r.k.vfs
            .camera_deadlines
            .insert(key, Instant::now() - Duration::from_millis(1));
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0x10000, 153600),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(r.error(), 1460);
        assert!(r.k.wait_deadlines.is_empty());
    }
    #[test]
    fn preview_enforces_twenty_fps_without_sleeping_guest_thread() {
        let mut r = Rig::new();
        r.start();
        r.info(320, 240, 1000);
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0x10000, 153600),
            DispatchOutcome::ReturnedR0(1)
        );
        *r.frame.lock().unwrap() = Some(Arc::new(Frame {
            width: 1,
            height: 1,
            serial: 2,
            rgb: vec![255; 3],
        }));
        r.k.vfs
            .camera_open(r.h)
            .unwrap()
            .device
            .lock()
            .unwrap()
            .next_preview = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0x10000, 153600),
            DispatchOutcome::JumpTo(0x70000000)
        );
        r.k.vfs
            .camera_open(r.h)
            .unwrap()
            .device
            .lock()
            .unwrap()
            .next_preview = Instant::now();
        assert_eq!(
            r.ctl(2105, 0x1100, 16, 0x10000, 153600),
            DispatchOutcome::ReturnedR0(1)
        );
        assert!(r.k.vfs.camera_deadlines.is_empty());
    }
    #[test]
    fn duplicate_process_handle_retains_capture_and_sharing_lease() {
        let mut r = Rig::new();
        r.start();
        assert_eq!(r.k.vfs.open_file("CAM1:", 3, 0, 3, false, 0), Err(32));
        let mut child = pocket_kernel::vfs::Vfs::new();
        child.attach_shared_context(r.k.vfs.shared_context());
        assert!(child.import_handle(99, r.k.vfs.export_handle(r.h).unwrap()));
        r.k.vfs.close(r.h);
        assert_eq!(r.drops.load(Ordering::SeqCst), 0);
        assert_eq!(r.k.vfs.open_file("CAM1:", 3, 0, 3, false, 0), Err(32));
        assert!(child
            .camera_open(99)
            .unwrap()
            .device
            .lock()
            .unwrap()
            .latest()
            .unwrap()
            .is_some());
        child.close(99);
        assert_eq!(r.drops.load(Ordering::SeqCst), 1);
        assert!(r.k.vfs.open_file("CAM1:", 3, 0, 3, false, 0).is_ok());
    }
    #[test]
    fn guest_duplicate_close_and_file_any_access_obey_device_contract() {
        let mut r = Rig::new();
        r.start();
        let process = match call(
            &mut r.cpu,
            &mut r.k,
            &mut r.d,
            "coredll.dll",
            "GetCurrentProcess",
            &[],
        ) {
            DispatchOutcome::ReturnedR0(p) => p,
            _ => panic!(),
        };
        assert_eq!(
            call(
                &mut r.cpu,
                &mut r.k,
                &mut r.d,
                "coredll.dll",
                "DuplicateHandle",
                &[process, r.h, process, 0x1500, 0, 0, 2]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        let alias = r.cpu.read_u32_le(0x1500).unwrap();
        assert_eq!(
            call(
                &mut r.cpu,
                &mut r.k,
                &mut r.d,
                "coredll.dll",
                "CloseHandle",
                &[r.h]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(r.drops.load(Ordering::SeqCst), 0);
        r.h = alias;
        assert_eq!(
            r.ctl(2102, 0, 0, 0x1600, 16),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(
            call(
                &mut r.cpu,
                &mut r.k,
                &mut r.d,
                "coredll.dll",
                "CloseHandle",
                &[alias]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(r.drops.load(Ordering::SeqCst), 1);
        // FILE_ANY_ACCESS permits the SDK IOCTLs on a query-only handle.
        r.h =
            r.k.vfs
                .open_file("CAM1:", 0, 3, 3, false, 0)
                .unwrap()
                .handle;
        assert_eq!(
            r.ctl(2102, 0, 0, 0x1600, 16),
            DispatchOutcome::ReturnedR0(1)
        );
        r.k.vfs.camera_service().set_allowed(false);
        assert_eq!(r.ctl(2103, 0, 0, 0, 0), DispatchOutcome::ReturnedR0(0));
        assert_eq!(r.error(), 5);
    }
}
