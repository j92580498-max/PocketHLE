//! PE resource directory parser.
//!
//! Implements just enough of the `.rsrc` directory format to flatten
//! it into a list of `(type, id_or_name) → (data_rva, size)` entries.
//! That is all PocketHLE needs for `FindResourceW`/`LoadResource`/
//! `LockResource` to return real bytes from the loaded image.
//!
//! Reference: <https://learn.microsoft.com/en-us/windows/win32/debug/pe-format#the-rsrc-section>

use byteorder::{ByteOrder, LittleEndian};
use goblin::pe::PE;

use crate::LoadError;

/// Either a named or integer-keyed resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceKey {
    Id(u32),
    Name(String),
}

/// One leaf node from the resource directory.
#[derive(Debug, Clone)]
pub struct ResourceEntry {
    pub ty: ResourceKey,
    pub name: ResourceKey,
    /// Resource virtual address relative to the image base.
    pub data_rva: u32,
    pub size: u32,
    pub codepage: u32,
}

/// Walk the resource directory and return one [`ResourceEntry`] per
/// leaf. Returns an empty vector if the image has no `.rsrc` section.
pub fn collect_resources(bytes: &[u8], pe: &PE) -> Result<Vec<ResourceEntry>, LoadError> {
    let mut out = Vec::new();
    let oh = match pe.header.optional_header {
        Some(h) => h,
        None => return Ok(out),
    };
    let dir = match oh.data_directories.get_resource_table() {
        Some(d) if d.size > 0 => d,
        _ => return Ok(out),
    };

    // Resolve the section that contains the resource directory so we
    // can convert sub-offsets to file offsets.
    let rsrc_section = pe.sections.iter().find(|s| {
        let start = s.virtual_address;
        let end = start.saturating_add(s.virtual_size.max(s.size_of_raw_data));
        dir.virtual_address >= start && dir.virtual_address < end
    });
    let rsrc_section = match rsrc_section {
        Some(s) => s,
        None => return Ok(out),
    };

    let section_va = rsrc_section.virtual_address;
    let section_off = rsrc_section.pointer_to_raw_data as usize;
    let section_size = rsrc_section.size_of_raw_data.max(rsrc_section.virtual_size) as usize;

    let read = |off: usize, n: usize| -> Option<&[u8]> {
        let abs = section_off + off;
        if abs + n > bytes.len() || off + n > section_size {
            None
        } else {
            Some(&bytes[abs..abs + n])
        }
    };

    let read_string = |string_off: u32| -> Option<String> {
        // IMAGE_RESOURCE_DIR_STRING_U: u16 length, then UTF-16LE chars.
        let off = string_off as usize;
        let lh = read(off, 2)?;
        let len = LittleEndian::read_u16(lh) as usize;
        let raw = read(off + 2, len * 2)?;
        let mut us = Vec::with_capacity(len);
        for c in raw.chunks_exact(2) {
            us.push(LittleEndian::read_u16(c));
        }
        Some(String::from_utf16_lossy(&us))
    };

    fn parse_directory<F: FnMut(u32, bool, u32)>(
        section_off: usize,
        section_size: usize,
        bytes: &[u8],
        dir_off: u32,
        cb: &mut F,
    ) -> Option<()> {
        let dir_abs = section_off + dir_off as usize;
        if dir_abs + 16 > bytes.len() || dir_off as usize + 16 > section_size {
            return None;
        }
        let header = &bytes[dir_abs..dir_abs + 16];
        let named = LittleEndian::read_u16(&header[12..14]) as usize;
        let id = LittleEndian::read_u16(&header[14..16]) as usize;
        let total = named + id;
        let entries_abs = dir_abs + 16;
        for i in 0..total {
            let entry_abs = entries_abs + i * 8;
            if entry_abs + 8 > bytes.len() {
                break;
            }
            let entry = &bytes[entry_abs..entry_abs + 8];
            let name = LittleEndian::read_u32(&entry[0..4]);
            let off = LittleEndian::read_u32(&entry[4..8]);
            let is_dir = off & 0x8000_0000 != 0;
            let off = off & 0x7fff_ffff;
            cb(name, is_dir, off);
        }
        Some(())
    }

    let key_from = |raw: u32| -> Option<ResourceKey> {
        if raw & 0x8000_0000 != 0 {
            let off = raw & 0x7fff_ffff;
            Some(ResourceKey::Name(read_string(off).unwrap_or_default()))
        } else {
            Some(ResourceKey::Id(raw))
        }
    };

    // Three-level walk: type -> name -> language -> data.
    let root = dir.virtual_address - section_va;
    let mut type_entries: Vec<(u32, u32)> = Vec::new();
    parse_directory(
        section_off,
        section_size,
        bytes,
        root,
        &mut |name, is_dir, off| {
            if is_dir {
                type_entries.push((name, off));
            }
        },
    );
    for (type_raw, type_dir_off) in type_entries {
        let ty = match key_from(type_raw) {
            Some(k) => k,
            None => continue,
        };
        let mut name_entries: Vec<(u32, u32)> = Vec::new();
        parse_directory(
            section_off,
            section_size,
            bytes,
            type_dir_off,
            &mut |name, is_dir, off| {
                if is_dir {
                    name_entries.push((name, off));
                }
            },
        );
        for (name_raw, name_dir_off) in name_entries {
            let name = match key_from(name_raw) {
                Some(k) => k,
                None => continue,
            };
            // Pick the first language in this name's directory — that
            // is what `LoadResource` does when LANG_NEUTRAL is asked.
            let mut lang_entries: Vec<u32> = Vec::new();
            parse_directory(
                section_off,
                section_size,
                bytes,
                name_dir_off,
                &mut |_name, is_dir, off| {
                    if !is_dir {
                        lang_entries.push(off);
                    }
                },
            );
            if let Some(data_entry_off) = lang_entries.first() {
                let abs = section_off + (*data_entry_off as usize);
                if abs + 16 > bytes.len() {
                    continue;
                }
                let data_rva = LittleEndian::read_u32(&bytes[abs..abs + 4]);
                let size = LittleEndian::read_u32(&bytes[abs + 4..abs + 8]);
                let codepage = LittleEndian::read_u32(&bytes[abs + 8..abs + 12]);
                out.push(ResourceEntry {
                    ty: ty.clone(),
                    name,
                    data_rva,
                    size,
                    codepage,
                });
            }
        }
    }
    Ok(out)
}
