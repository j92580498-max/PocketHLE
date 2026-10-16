use pocket_kernel::cedb::{
    Property, PropertyValue, WriteProperty, SEEK_VALUE_FIRST_EQUAL, SEEK_VALUE_GREATER,
    SEEK_VALUE_NEXT_EQUAL, SEEK_VALUE_SMALLER,
};
use pocket_kernel::{DispatchOutcome, KernelError};

use crate::{CallCtx, WinCeDispatcher};

const INVALID_HANDLE_VALUE: u32 = 0xFFFF_FFFF;
const CEDB_VALIDNAME: u32 = 0x0001;
const CEDB_VALIDTYPE: u32 = 0x0002;
const CEDB_VALIDSORTSPEC: u32 = 0x0004;
const CEDB_MAXDBASENAMELEN: u32 = 32;
const CEDB_MAXSORTORDER: u32 = 4;
const CEDB_PROPDELETE: u16 = 0x0200;
const CEDB_PROPNOTFOUND: u16 = 0x0100;
const CEDB_ALLOWREALLOC: u32 = 0x0001;
const CEDB_MAXPROPS: u32 = 256;
const CEDB_MAX_PROPERTY_BYTES: u32 = 1024 * 1024;
const CEDB_PROPVAL_SIZE: u32 = 16;

pub fn register(dispatcher: &mut WinCeDispatcher) {
    let dll = "coredll.dll";
    dispatcher.register_handler(dll, "CeCreateDatabase", create_database);
    dispatcher.register_handler(dll, "CeCreateDatabaseEx", create_database_ex);
    dispatcher.register_handler(dll, "CeOpenDatabase", open_database);
    dispatcher.register_handler(dll, "CeOpenDatabaseEx", open_database_ex);
    dispatcher.register_handler(dll, "CeSeekDatabase", seek_database);
    dispatcher.register_handler(dll, "CeWriteRecordProps", write_record_props);
    dispatcher.register_handler(dll, "CeReadRecordProps", read_record_props);
    dispatcher.register_handler(dll, "CeReadRecordPropsEx", read_record_props_ex);
    dispatcher.register_handler(dll, "CeDeleteRecord", delete_record);
    dispatcher.register_handler(dll, "CeDeleteDatabase", delete_database);
    dispatcher.register_handler(dll, "CeDeleteDatabaseEx", delete_database_ex);
}

fn create_database_ex(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _volume_guid = ctx.arg_u32(0)?;
    let info = ctx.arg_u32(1)?;
    if info == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let flags = read_u32(ctx, info)?;
    let name = crate::coredll::read_guest_wstr_lossy(ctx, info + 4, CEDB_MAXDBASENAMELEN)?;
    if name.is_empty() || (flags & CEDB_VALIDNAME == 0 && flags != 0) {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let database_type = if flags & CEDB_VALIDTYPE != 0 {
        read_u32(ctx, info + 68)?
    } else {
        0
    };
    let sort_count = if flags & CEDB_VALIDSORTSPEC != 0 {
        read_u16(ctx, info + 74)? as u32
    } else {
        0
    };
    if sort_count > CEDB_MAXSORTORDER {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut sort_orders = Vec::with_capacity(sort_count as usize);
    for index in 0..sort_count {
        let at = info + 88 + index * 8;
        sort_orders.push((read_u32(ctx, at)?, read_u32(ctx, at + 4)?));
    }
    let oid = ctx
        .kernel
        .cedb
        .create_database(name.clone(), database_type, sort_orders)
        .unwrap_or(0);
    log::debug!("CeCreateDatabaseEx({name:?}) -> {oid:#010x}");
    Ok(DispatchOutcome::ReturnedR0(oid))
}

fn create_database(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name_ptr = ctx.arg_u32(0)?;
    let database_type = ctx.arg_u32(1)?;
    let sort_count = ctx.arg_u32(2)?;
    let sort_ptr = ctx.arg_u32(3)?;
    if name_ptr == 0 || sort_count > CEDB_MAXSORTORDER || (sort_count > 0 && sort_ptr == 0) {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let name = crate::coredll::read_guest_wstr_lossy(ctx, name_ptr, CEDB_MAXDBASENAMELEN)?;
    if name.is_empty() {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut sort_orders = Vec::with_capacity(sort_count as usize);
    for index in 0..sort_count {
        let at = sort_ptr + index * 8;
        sort_orders.push((read_u32(ctx, at)?, read_u32(ctx, at + 4)?));
    }
    let oid = ctx
        .kernel
        .cedb
        .create_database(name.clone(), database_type, sort_orders)
        .unwrap_or(0);
    log::debug!("CeCreateDatabase({name:?}) -> {oid:#010x}");
    Ok(DispatchOutcome::ReturnedR0(oid))
}

fn open_database(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let oid_ptr = ctx.arg_u32(0)?;
    let name_ptr = ctx.arg_u32(1)?;
    let propid = ctx.arg_u32(2)?;
    let flags = ctx.arg_u32(3)?;
    open_database_with_args(ctx, oid_ptr, name_ptr, propid, flags)
}

fn open_database_ex(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _volume_guid = ctx.arg_u32(0)?;
    let oid_ptr = ctx.arg_u32(1)?;
    let name_ptr = ctx.arg_u32(2)?;
    let propid = ctx.arg_u32(3)?;
    let flags = ctx.arg_u32(4)?;
    let _notification_request = ctx.arg_u32(5)?;
    open_database_with_args(ctx, oid_ptr, name_ptr, propid, flags)
}

fn open_database_with_args(
    ctx: &mut CallCtx<'_>,
    oid_ptr: u32,
    name_ptr: u32,
    propid: u32,
    flags: u32,
) -> Result<DispatchOutcome, KernelError> {
    let requested_oid = if oid_ptr == 0 {
        0
    } else {
        read_u32(ctx, oid_ptr)?
    };
    let name = if requested_oid == 0 && name_ptr != 0 {
        Some(crate::coredll::read_guest_wstr_lossy(
            ctx,
            name_ptr,
            CEDB_MAXDBASENAMELEN,
        )?)
    } else {
        None
    };
    let opened = ctx
        .kernel
        .cedb
        .open_database(requested_oid, name.as_deref(), propid, flags);
    let Some((handle, database_oid)) = opened else {
        log::debug!("CeOpenDatabase(name={name:?}, oid={requested_oid:#010x}) -> invalid");
        return Ok(DispatchOutcome::ReturnedR0(INVALID_HANDLE_VALUE));
    };
    if oid_ptr != 0 {
        write_u32(ctx, oid_ptr, database_oid)?;
    }
    log::debug!("CeOpenDatabase(name={name:?}, oid={database_oid:#010x}) -> {handle:#010x}");
    Ok(DispatchOutcome::ReturnedR0(handle))
}

fn seek_database(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let seek_type = ctx.arg_u32(1)?;
    let value = ctx.arg_u32(2)?;
    let index_ptr = ctx.arg_u32(3)?;
    let query = if matches!(
        seek_type,
        SEEK_VALUE_SMALLER | SEEK_VALUE_FIRST_EQUAL | SEEK_VALUE_GREATER | SEEK_VALUE_NEXT_EQUAL
    ) && value != 0
    {
        read_property(ctx, value)?.map(|(propid, _, value)| Property { propid, value })
    } else {
        None
    };
    let result = ctx
        .kernel
        .cedb
        .seek(handle, seek_type, value, query.as_ref());
    if let Some((oid, index)) = result {
        if index_ptr != 0 {
            write_u32(ctx, index_ptr, index)?;
        }
        Ok(DispatchOutcome::ReturnedR0(oid))
    } else {
        Ok(DispatchOutcome::ReturnedR0(0))
    }
}

fn write_record_props(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let record_oid = ctx.arg_u32(1)?;
    let count = ctx.arg_u32(2)?;
    let properties_ptr = ctx.arg_u32(3)?;
    if count == 0 || count > CEDB_MAXPROPS || properties_ptr == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut properties = Vec::with_capacity(count as usize);
    for index in 0..count {
        let Some((propid, flags, value)) =
            read_property(ctx, properties_ptr + index * CEDB_PROPVAL_SIZE)?
        else {
            return Ok(DispatchOutcome::ReturnedR0(0));
        };
        properties.push(WriteProperty {
            propid,
            value: if flags & CEDB_PROPDELETE != 0 {
                None
            } else {
                Some(value)
            },
        });
    }
    let oid = ctx
        .kernel
        .cedb
        .write_record(handle, record_oid, properties)
        .unwrap_or(0);
    log::debug!("CeWriteRecordProps(handle={handle:#010x}, oid={record_oid:#010x}, count={count}) -> {oid:#010x}");
    Ok(DispatchOutcome::ReturnedR0(oid))
}

fn read_record_props(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let flags = ctx.arg_u32(1)?;
    let count_ptr = ctx.arg_u32(2)?;
    let propids_ptr = ctx.arg_u32(3)?;
    let buffer_ptr_ptr = ctx.arg_u32(4)?;
    let buffer_size_ptr = ctx.arg_u32(5)?;
    if count_ptr == 0 || buffer_ptr_ptr == 0 || buffer_size_ptr == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let record = ctx.kernel.cedb.current_record(handle);
    let Some(record) = record else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let (requested_count, requested_ids) = if propids_ptr == 0 {
        (record.properties.len(), None)
    } else {
        let count = read_u16(ctx, count_ptr)? as usize;
        if count > CEDB_MAXPROPS as usize {
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
        let mut ids = Vec::with_capacity(count);
        for index in 0..count {
            ids.push(read_u32(ctx, propids_ptr + index as u32 * 4)?);
        }
        (count, Some(ids))
    };
    let requested = if let Some(ids) = requested_ids {
        ids.into_iter()
            .map(|propid| {
                record
                    .properties
                    .get(&propid)
                    .cloned()
                    .map(|value| (propid, Some(value)))
                    .unwrap_or((propid, None))
            })
            .collect::<Vec<_>>()
    } else {
        record
            .properties
            .iter()
            .map(|(propid, value)| (*propid, Some(value.clone())))
            .collect::<Vec<_>>()
    };
    if requested.len() != requested_count {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let required = marshal_properties(&requested, 0).len() as u32;
    let mut buffer_ptr = read_u32(ctx, buffer_ptr_ptr)?;
    let available = read_u32(ctx, buffer_size_ptr)?;
    if buffer_ptr == 0 || available < required {
        if flags & CEDB_ALLOWREALLOC == 0 {
            write_u32(ctx, buffer_size_ptr, required)?;
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
        let Some(new_buffer) = ctx.kernel.heap.alloc(required.max(1)) else {
            return Ok(DispatchOutcome::ReturnedR0(0));
        };
        if buffer_ptr != 0 {
            ctx.kernel.heap.free(buffer_ptr);
        }
        buffer_ptr = new_buffer;
        write_u32(ctx, buffer_ptr_ptr, buffer_ptr)?;
    }
    let bytes = marshal_properties(&requested, buffer_ptr);
    ctx.cpu.write_mem(buffer_ptr, &bytes)?;
    ctx.cpu
        .write_mem(count_ptr, &(requested.len() as u16).to_le_bytes())?;
    write_u32(ctx, buffer_size_ptr, bytes.len() as u32)?;
    ctx.kernel.cedb.advance_cursor(handle);
    Ok(DispatchOutcome::ReturnedR0(record.oid))
}

fn read_record_props_ex(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _heap = ctx.arg_u32(6)?;
    read_record_props(ctx)
}

fn delete_record(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let oid = ctx.arg_u32(1)?;
    Ok(DispatchOutcome::ReturnedR0(u32::from(
        ctx.kernel.cedb.delete_record(handle, oid),
    )))
}

fn delete_database(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let oid = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(u32::from(
        ctx.kernel.cedb.delete_database(oid),
    )))
}

fn delete_database_ex(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _volume_guid = ctx.arg_u32(0)?;
    let oid = ctx.arg_u32(1)?;
    Ok(DispatchOutcome::ReturnedR0(u32::from(
        ctx.kernel.cedb.delete_database(oid),
    )))
}

fn read_property(
    ctx: &mut CallCtx<'_>,
    address: u32,
) -> Result<Option<(u32, u16, PropertyValue)>, KernelError> {
    let bytes = ctx.cpu.read_mem(address, CEDB_PROPVAL_SIZE)?;
    let propid = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let flags = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
    let value = &bytes[8..16];
    let type_code = propid as u16;
    let property = match type_code {
        2 => PropertyValue::I2(i16::from_le_bytes(value[0..2].try_into().unwrap())),
        3 => PropertyValue::I4(i32::from_le_bytes(value[0..4].try_into().unwrap())),
        5 => PropertyValue::R8(u64::from_le_bytes(value.try_into().unwrap())),
        11 => PropertyValue::Bool(i32::from_le_bytes(value[0..4].try_into().unwrap())),
        18 => PropertyValue::Ui2(u16::from_le_bytes(value[0..2].try_into().unwrap())),
        19 => PropertyValue::Ui4(u32::from_le_bytes(value[0..4].try_into().unwrap())),
        31 => {
            let string_ptr = u32::from_le_bytes(value[0..4].try_into().unwrap());
            let text = if string_ptr == 0 {
                String::new()
            } else {
                crate::coredll::read_guest_wstr_lossy(ctx, string_ptr, 4096)?
            };
            PropertyValue::String(text)
        }
        64 => PropertyValue::FileTime(u64::from_le_bytes(value.try_into().unwrap())),
        65 => {
            let length = u32::from_le_bytes(value[0..4].try_into().unwrap());
            let data_ptr = u32::from_le_bytes(value[4..8].try_into().unwrap());
            if length > CEDB_MAX_PROPERTY_BYTES || (length > 0 && data_ptr == 0) {
                return Ok(None);
            }
            PropertyValue::Blob(if length == 0 {
                Vec::new()
            } else {
                ctx.cpu.read_mem(data_ptr, length)?
            })
        }
        _ => PropertyValue::Raw {
            type_code,
            bits: u64::from_le_bytes(value.try_into().unwrap()),
        },
    };
    Ok(Some((propid, flags, property)))
}

fn marshal_properties(properties: &[(u32, Option<PropertyValue>)], base: u32) -> Vec<u8> {
    let fixed_size = properties.len() * CEDB_PROPVAL_SIZE as usize;
    let mut output = vec![0; fixed_size];
    for (index, (propid, value)) in properties.iter().enumerate() {
        let at = index * CEDB_PROPVAL_SIZE as usize;
        output[at..at + 4].copy_from_slice(&propid.to_le_bytes());
        let mut flags = 0u16;
        let mut variable = None;
        match value {
            None => flags |= CEDB_PROPNOTFOUND,
            Some(PropertyValue::I2(value)) => {
                output[at + 8..at + 10].copy_from_slice(&value.to_le_bytes());
            }
            Some(PropertyValue::I4(value)) => {
                output[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
            }
            Some(PropertyValue::R8(bits)) | Some(PropertyValue::FileTime(bits)) => {
                output[at + 8..at + 16].copy_from_slice(&bits.to_le_bytes());
            }
            Some(PropertyValue::Bool(value)) => {
                output[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
            }
            Some(PropertyValue::Ui2(value)) => {
                output[at + 8..at + 10].copy_from_slice(&value.to_le_bytes());
            }
            Some(PropertyValue::Ui4(value)) => {
                output[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
            }
            Some(PropertyValue::String(value)) => {
                let payload: Vec<u8> = value
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(u16::to_le_bytes)
                    .collect();
                output[at + 4..at + 6]
                    .copy_from_slice(&(payload.len().min(u16::MAX as usize) as u16).to_le_bytes());
                variable = Some(payload);
            }
            Some(PropertyValue::Blob(value)) => {
                output[at + 4..at + 6]
                    .copy_from_slice(&(value.len().min(u16::MAX as usize) as u16).to_le_bytes());
                output[at + 8..at + 12].copy_from_slice(&(value.len() as u32).to_le_bytes());
                variable = Some(value.clone());
            }
            Some(PropertyValue::Raw { bits, .. }) => {
                output[at + 8..at + 16].copy_from_slice(&bits.to_le_bytes());
            }
        }
        output[at + 6..at + 8].copy_from_slice(&flags.to_le_bytes());
        if let Some(payload) = variable {
            while (output.len() - fixed_size) & 3 != 0 {
                output.push(0);
            }
            let offset = output.len();
            let pointer = base.wrapping_add(offset as u32);
            if matches!(value, Some(PropertyValue::Blob(_))) {
                output[at + 12..at + 16].copy_from_slice(&pointer.to_le_bytes());
            } else {
                output[at + 8..at + 12].copy_from_slice(&pointer.to_le_bytes());
            }
            output.extend_from_slice(&payload);
            while (output.len() - fixed_size) & 3 != 0 {
                output.push(0);
            }
        }
    }
    output
}

fn read_u16(ctx: &mut CallCtx<'_>, address: u32) -> Result<u16, KernelError> {
    Ok(ctx.cpu.read_u16_le(address)?)
}

fn read_u32(ctx: &mut CallCtx<'_>, address: u32) -> Result<u32, KernelError> {
    Ok(ctx.cpu.read_u32_le(address)?)
}

fn write_u32(ctx: &mut CallCtx<'_>, address: u32, value: u32) -> Result<(), KernelError> {
    ctx.cpu.write_mem(address, &value.to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu, Prot};
    use pocket_kernel::cedb::SEEK_BEGINNING;
    use pocket_kernel::{Heap, KernelState, Thunk};
    use pocket_pe::ImportBinding;

    const GUEST: u32 = 0x0001_0000;
    const HEAP: u32 = 0x5000_0000;

    fn thunk() -> Thunk {
        Thunk {
            thunk_va: 0x7000_0000,
            iat_va: 0x0002_0000,
            dll: "coredll.dll".to_string(),
            binding: ImportBinding::Name("test".to_string()),
            friendly_name: None,
        }
    }

    fn write_wstr(cpu: &mut StubCpu, address: u32, text: &str) {
        let bytes: Vec<u8> = text
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        cpu.write_mem(address, &bytes).unwrap();
    }

    fn set_reg(cpu: &mut StubCpu, register: ArmReg, value: u32) {
        cpu.write_reg(register, value).unwrap();
    }

    #[test]
    fn guest_cedb_create_open_write_seek_and_read() {
        let mut cpu = StubCpu::new();
        cpu.map_region(GUEST, 0x10000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.map_region(HEAP, 0x10000, Prot::READ | Prot::WRITE)
            .unwrap();
        let mut kernel: KernelState = crate::coredll::tests::fresh_kernel();
        kernel.heap = Heap::new(HEAP, 0x10000);
        let thunk = thunk();
        let propid: u32 = 0x0001_0003;
        let info = GUEST + 0x100;
        let oid_ptr = GUEST + 0x500;
        let name_ptr = GUEST + 0x600;
        let index_ptr = GUEST + 0x700;
        let ids_ptr = GUEST + 0x800;
        let buffer_ptr_ptr = GUEST + 0x900;
        let buffer_size_ptr = GUEST + 0x904;
        let property_ptr = HEAP + 0x100;

        let mut info_bytes = vec![0; 120];
        info_bytes[0..4]
            .copy_from_slice(&(CEDB_VALIDNAME | CEDB_VALIDTYPE | CEDB_VALIDSORTSPEC).to_le_bytes());
        info_bytes[68..72].copy_from_slice(&17u32.to_le_bytes());
        info_bytes[74..76].copy_from_slice(&1u16.to_le_bytes());
        info_bytes[88..92].copy_from_slice(&propid.to_le_bytes());
        cpu.write_mem(info, &info_bytes).unwrap();
        write_wstr(&mut cpu, info + 4, "3D Air Disc Properties");
        set_reg(&mut cpu, ArmReg::R0, 0);
        set_reg(&mut cpu, ArmReg::R1, info);
        let database_oid = {
            let mut ctx = CallCtx {
                cpu: &mut cpu,
                thunk: &thunk,
                kernel: &mut kernel,
            };
            match create_database_ex(&mut ctx).unwrap() {
                DispatchOutcome::ReturnedR0(value) => value,
                other => panic!("unexpected result: {other:?}"),
            }
        };
        assert_ne!(database_oid, 0);

        cpu.write_mem(oid_ptr, &0u32.to_le_bytes()).unwrap();
        write_wstr(&mut cpu, name_ptr, "3D Air Disc Properties");
        set_reg(&mut cpu, ArmReg::R0, oid_ptr);
        set_reg(&mut cpu, ArmReg::R1, name_ptr);
        set_reg(&mut cpu, ArmReg::R2, propid);
        set_reg(&mut cpu, ArmReg::R3, 0);
        let handle = {
            let mut ctx = CallCtx {
                cpu: &mut cpu,
                thunk: &thunk,
                kernel: &mut kernel,
            };
            match open_database(&mut ctx).unwrap() {
                DispatchOutcome::ReturnedR0(value) => value,
                other => panic!("unexpected result: {other:?}"),
            }
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        assert_eq!(read_u32_from_cpu(&mut cpu, oid_ptr), database_oid);

        let mut prop = [0u8; 16];
        prop[0..4].copy_from_slice(&propid.to_le_bytes());
        prop[8..12].copy_from_slice(&42i32.to_le_bytes());
        cpu.write_mem(property_ptr, &prop).unwrap();
        set_reg(&mut cpu, ArmReg::R0, handle);
        set_reg(&mut cpu, ArmReg::R1, 0);
        set_reg(&mut cpu, ArmReg::R2, 1);
        set_reg(&mut cpu, ArmReg::R3, property_ptr);
        let record_oid = {
            let mut ctx = CallCtx {
                cpu: &mut cpu,
                thunk: &thunk,
                kernel: &mut kernel,
            };
            match write_record_props(&mut ctx).unwrap() {
                DispatchOutcome::ReturnedR0(value) => value,
                other => panic!("unexpected result: {other:?}"),
            }
        };
        assert_ne!(record_oid, 0);

        set_reg(&mut cpu, ArmReg::R0, handle);
        set_reg(&mut cpu, ArmReg::R1, SEEK_BEGINNING);
        set_reg(&mut cpu, ArmReg::R2, 0);
        set_reg(&mut cpu, ArmReg::R3, index_ptr);
        let sought_oid = {
            let mut ctx = CallCtx {
                cpu: &mut cpu,
                thunk: &thunk,
                kernel: &mut kernel,
            };
            match seek_database(&mut ctx).unwrap() {
                DispatchOutcome::ReturnedR0(value) => value,
                other => panic!("unexpected result: {other:?}"),
            }
        };
        assert_eq!(sought_oid, record_oid);
        assert_eq!(read_u32_from_cpu(&mut cpu, index_ptr), 0);

        cpu.write_mem(GUEST + 0xA00, &1u16.to_le_bytes()).unwrap();
        cpu.write_mem(ids_ptr, &propid.to_le_bytes()).unwrap();
        cpu.write_mem(buffer_ptr_ptr, &0u32.to_le_bytes()).unwrap();
        cpu.write_mem(buffer_size_ptr, &0u32.to_le_bytes()).unwrap();
        let stack = GUEST + 0x2000;
        set_reg(&mut cpu, ArmReg::Sp, stack);
        let stack_args = stack + cpu.stack_arg_offset();
        cpu.write_mem(stack_args, &buffer_ptr_ptr.to_le_bytes())
            .unwrap();
        cpu.write_mem(stack_args + 4, &buffer_size_ptr.to_le_bytes())
            .unwrap();
        set_reg(&mut cpu, ArmReg::R0, handle);
        set_reg(&mut cpu, ArmReg::R1, CEDB_ALLOWREALLOC);
        set_reg(&mut cpu, ArmReg::R2, GUEST + 0xA00);
        set_reg(&mut cpu, ArmReg::R3, ids_ptr);
        let read_oid = {
            let mut ctx = CallCtx {
                cpu: &mut cpu,
                thunk: &thunk,
                kernel: &mut kernel,
            };
            match read_record_props(&mut ctx).unwrap() {
                DispatchOutcome::ReturnedR0(value) => value,
                other => panic!("unexpected result: {other:?}"),
            }
        };
        assert_eq!(read_oid, record_oid);
        let buffer = read_u32_from_cpu(&mut cpu, buffer_ptr_ptr);
        let stored_value =
            u32::from_le_bytes(cpu.read_mem(buffer + 8, 4).unwrap().try_into().unwrap());
        assert_eq!(stored_value, 42);
        assert_eq!(read_u32_from_cpu(&mut cpu, buffer_size_ptr), 16);
    }

    fn read_u32_from_cpu(cpu: &mut StubCpu, address: u32) -> u32 {
        u32::from_le_bytes(cpu.read_mem(address, 4).unwrap().try_into().unwrap())
    }
}
