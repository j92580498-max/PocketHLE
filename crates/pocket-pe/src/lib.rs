//! Loader for Windows CE / Windows Mobile PE32 executables.
//!
//! This is a *high-level emulation* loader: we load the image into a
//! flat virtual address space, build the import address table by
//! resolving each (DLL, ordinal-or-name) pair to a synthetic stub
//! address managed by [`pocket-kernel`], and apply base relocations if
//! the requested image base is not free.
//!
//! We intentionally do **not** apply low-level WinCE specific tricks
//! such as XIP-in-ROM mapping or per-process slot relocation: HLE
//! emulators run each game in its own private 32-bit address space, so
//! a single contiguous mapping is sufficient.

use std::collections::BTreeMap;
use std::path::Path;

use byteorder::{ByteOrder, LittleEndian};
use goblin::pe::{options::ParseOptions, PE};
use indexmap::IndexMap;
use thiserror::Error;

pub mod icon;
pub use icon::{icon_from_pe_bytes, IconImage};

pub mod resources;
pub use resources::{collect_resources, ResourceEntry, ResourceKey};

pub mod machine {
    /// Legacy ARM (`IMAGE_FILE_MACHINE_ARM`). Used by Pocket PC 2002 / 2003
    /// and most Windows Mobile 5/6 binaries — typically ARMv4T or ARMv5TE.
    pub const ARM: u16 = 0x01c0;
    /// ARMv7 Thumb-2 (`IMAGE_FILE_MACHINE_THUMB`). Rare in WinCE.
    pub const THUMB: u16 = 0x01c2;
    /// `IMAGE_FILE_MACHINE_ARMNT` — Windows Phone 8 / WinRT.
    pub const ARMNT: u16 = 0x01c4;
    pub const I386: u16 = 0x014c;
    pub const MIPS_R3000: u16 = 0x0162;
    pub const MIPS_R4000: u16 = 0x0166;
    pub const SH3: u16 = 0x01a2;
    pub const SH4: u16 = 0x01a6;
}

pub mod subsystem {
    pub const WINDOWS_CE_GUI: u16 = 9;
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a PE: {0}")]
    NotPe(String),
    #[error("unsupported machine: 0x{0:04x}")]
    UnsupportedMachine(u16),
    #[error("unsupported subsystem: {0}")]
    UnsupportedSubsystem(u16),
    #[error("section out of bounds: {0}")]
    SectionOob(String),
    #[error("malformed import directory: {0}")]
    BadImports(String),
}

/// One imported symbol from a foreign DLL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSymbol {
    /// DLL name as written in the import directory (e.g. `COREDLL.dll`).
    pub dll: String,
    /// Either an exported name or an ordinal.
    pub binding: ImportBinding,
    /// Address in the image's virtual address space at which the
    /// resolved function address must be written before code is run.
    pub iat_va: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportBinding {
    Name(String),
    Ordinal(u16),
}

impl ImportBinding {
    pub fn to_string_short(&self) -> String {
        match self {
            ImportBinding::Name(n) => n.clone(),
            ImportBinding::Ordinal(o) => format!("ord {o}"),
        }
    }
}

/// One section that has been laid out in the emulated address space.
#[derive(Debug, Clone)]
pub struct LoadedSection {
    pub name: String,
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub characteristics: u32,
    pub data: Vec<u8>,
}

impl LoadedSection {
    pub fn is_executable(&self) -> bool {
        // IMAGE_SCN_MEM_EXECUTE
        (self.characteristics & 0x2000_0000) != 0
    }
    pub fn is_writable(&self) -> bool {
        // IMAGE_SCN_MEM_WRITE
        (self.characteristics & 0x8000_0000) != 0
    }
    pub fn is_readable(&self) -> bool {
        // IMAGE_SCN_MEM_READ
        (self.characteristics & 0x4000_0000) != 0
    }
}

/// A fully laid-out image ready to be mapped into the emulator.
#[derive(Debug, Clone)]
pub struct LoadedImage {
    pub source_path: String,
    pub machine: u16,
    pub subsystem: u16,
    pub image_base: u32,
    pub size_of_image: u32,
    pub entry_point: u32,
    pub sections: Vec<LoadedSection>,
    /// Imports keyed by (dll lower-cased, binding) so callers can build
    /// the IAT stub map.
    pub imports: Vec<ImportSymbol>,
    /// Map from RVA to "DLL name" for every export of this image, if the
    /// PE happens to be a DLL. For executables this is empty.
    pub exports: IndexMap<String, u32>,
    /// Flat list of `.rsrc` directory entries. Empty if the image
    /// has no resource section.
    pub resources: Vec<ResourceEntry>,
    /// CLR metadata for a managed PE image. Native WinCE games do not
    /// have this directory; .NET Compact Framework assemblies do.
    pub managed_runtime: Option<String>,
}

impl LoadedImage {
    pub fn entry_va(&self) -> u32 {
        let va = self.image_base.wrapping_add(self.entry_point);
        if self.machine == machine::THUMB || self.machine == machine::ARMNT {
            va | 1
        } else {
            va
        }
    }

    pub fn machine_name(&self) -> &'static str {
        match self.machine {
            machine::ARM => "ARM (legacy)",
            machine::THUMB => "ARM Thumb",
            machine::ARMNT => "ARMv7 Thumb-2 (ARMNT)",
            machine::I386 => "x86",
            machine::MIPS_R3000 | machine::MIPS_R4000 => "MIPS",
            machine::SH3 => "SH-3",
            machine::SH4 => "SH-4",
            _ => "unknown",
        }
    }
}

/// Load a PE32 file from disk.
pub fn load_file(path: impl AsRef<Path>) -> Result<LoadedImage, LoadError> {
    let bytes = std::fs::read(path.as_ref())?;
    let mut img = load_bytes(&bytes)?;
    img.source_path = path.as_ref().display().to_string();
    Ok(img)
}

fn parse_wince_pe(bytes: &[u8]) -> Result<PE<'_>, LoadError> {
    let mut options = ParseOptions::default();
    options.parse_attribute_certificates = false;
    PE::parse_with_opts(bytes, &options).map_err(|e| LoadError::NotPe(e.to_string()))
}

/// Parse and lay out a PE32 image from raw bytes.
pub fn load_bytes(bytes: &[u8]) -> Result<LoadedImage, LoadError> {
    let pe = parse_wince_pe(bytes)?;
    if pe.is_64 {
        return Err(LoadError::UnsupportedMachine(pe.header.coff_header.machine));
    }
    let machine = pe.header.coff_header.machine;
    let oh = pe
        .header
        .optional_header
        .ok_or_else(|| LoadError::NotPe("missing optional header".into()))?;

    let image_base = oh.windows_fields.image_base as u32;
    let size_of_image = oh.windows_fields.size_of_image;
    let entry_point = oh.standard_fields.address_of_entry_point as u32;
    let subsys = oh.windows_fields.subsystem;
    let managed_runtime = clr_runtime_version(bytes, &pe);

    if !matches!(
        machine,
        machine::ARM
            | machine::THUMB
            | machine::ARMNT
            | machine::I386
            | machine::MIPS_R3000
            | machine::MIPS_R4000
    ) {
        return Err(LoadError::UnsupportedMachine(machine));
    }
    // Don't reject other subsystems — the user may want to inspect a
    // desktop image (the JumpyBall zip is x86 GUI for example).

    let mut sections = Vec::with_capacity(pe.sections.len());
    for s in &pe.sections {
        let name = String::from_utf8_lossy(&s.name)
            .trim_end_matches('\0')
            .to_string();
        let va = s.virtual_address;
        let vs = s.virtual_size;
        let raw_off = s.pointer_to_raw_data as usize;
        let raw_size = s.size_of_raw_data as usize;
        let mut data = Vec::with_capacity(vs as usize);
        if raw_off + raw_size <= bytes.len() && raw_size > 0 {
            data.extend_from_slice(&bytes[raw_off..raw_off + raw_size.min(vs as usize)]);
        }
        // Pad up to virtual size with zeroes (BSS).
        if data.len() < vs as usize {
            data.resize(vs as usize, 0);
        }
        sections.push(LoadedSection {
            name,
            virtual_address: va,
            virtual_size: vs,
            characteristics: s.characteristics,
            data,
        });
    }

    let imports = collect_imports(bytes, &pe)?;
    let exports = collect_exports(&pe);
    let resources = collect_resources(bytes, &pe).unwrap_or_default();

    Ok(LoadedImage {
        source_path: String::new(),
        machine,
        subsystem: subsys,
        image_base,
        size_of_image,
        entry_point,
        sections,
        imports,
        exports,
        resources,
        managed_runtime,
    })
}

fn clr_runtime_version(bytes: &[u8], pe: &PE<'_>) -> Option<String> {
    let optional_header = pe.header.optional_header?;
    let dir = optional_header.data_directories.get_clr_runtime_header()?;
    if dir.virtual_address == 0 || dir.size < 16 {
        return None;
    }
    let section = pe.sections.iter().find(|s| {
        let start = s.virtual_address;
        let end = start.saturating_add(s.virtual_size.max(s.size_of_raw_data));
        dir.virtual_address >= start && dir.virtual_address.saturating_add(16) <= end
    })?;
    let clr_offset = section
        .pointer_to_raw_data
        .checked_add(dir.virtual_address.checked_sub(section.virtual_address)?)?
        as usize;
    let clr_end = clr_offset.checked_add(dir.size as usize)?;
    if clr_end > bytes.len() {
        return None;
    }
    let metadata_rva = u32::from_le_bytes(bytes[clr_offset + 8..clr_offset + 12].try_into().ok()?);
    let metadata_size =
        u32::from_le_bytes(bytes[clr_offset + 12..clr_offset + 16].try_into().ok()?);
    let metadata_section = pe.sections.iter().find(|s| {
        let start = s.virtual_address;
        let end = start.saturating_add(s.virtual_size.max(s.size_of_raw_data));
        metadata_rva >= start && metadata_rva < end
    })?;
    let metadata_offset = metadata_section
        .pointer_to_raw_data
        .checked_add(metadata_rva.checked_sub(metadata_section.virtual_address)?)?
        as usize;
    let metadata_end = metadata_offset.checked_add(metadata_size as usize)?;
    if metadata_end > bytes.len() || metadata_size < 16 {
        return Some("unknown CLR runtime".to_string());
    }
    if bytes.get(metadata_offset..metadata_offset + 4) != Some(b"BSJB") {
        return Some("unknown CLR runtime".to_string());
    }
    let version_len = u32::from_le_bytes(
        bytes[metadata_offset + 12..metadata_offset + 16]
            .try_into()
            .ok()?,
    ) as usize;
    let version_start = metadata_offset + 16;
    let version_end = version_start.checked_add(version_len)?.min(metadata_end);
    let version = bytes.get(version_start..version_end)?;
    let nul = version
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(version.len());
    let text = String::from_utf8_lossy(&version[..nul]).trim().to_string();
    Some(if text.is_empty() {
        "unknown CLR runtime".to_string()
    } else {
        text
    })
}

fn collect_exports(pe: &PE) -> IndexMap<String, u32> {
    let mut out = IndexMap::new();
    let has_export_table = pe
        .header
        .optional_header
        .map(|h| h.data_directories.get_export_table().is_some())
        .unwrap_or(false);
    if has_export_table {
        for e in &pe.exports {
            if let Some(name) = e.name {
                out.insert(name.to_string(), e.rva as u32);
            }
        }
    }
    out
}

/// Normalize a DLL name taken from the import directory.
///
/// MSVC writes `COREDLL.dll`, but CeGCC / mingw32ce writes the bare
/// module name with no extension (`COREDLL`). Everything downstream —
/// the dispatcher's `(dll, name)` handler keys, the native ARM thunk
/// table, the constant-return thunks and the dynamic-export list — keys
/// on the lower-cased name *with* the `.dll` suffix, so a suffix-less
/// name misses all of them and every import silently degrades to an
/// unimplemented stub. Appending the extension here, at the single point
/// the name enters the loader, keeps that assumption true for both
/// toolchains.
///
/// A name that already carries an extension is left alone: WinCE modules
/// are referenced by file name and `.dll` is not the only one in use
/// (`.cpl`, `.drv`).
fn normalize_import_dll(name: &str) -> String {
    if name.is_empty() || name.contains('.') {
        return name.to_string();
    }
    format!("{name}.dll")
}

fn collect_imports(bytes: &[u8], pe: &PE) -> Result<Vec<ImportSymbol>, LoadError> {
    let mut out = Vec::new();
    let oh = pe
        .header
        .optional_header
        .ok_or_else(|| LoadError::BadImports("no optional header".into()))?;
    let dir = match oh.data_directories.get_import_table() {
        Some(d) if d.size > 0 => d,
        _ => return Ok(out),
    };

    let img_base = oh.windows_fields.image_base as u32;
    let read_va = |va: u32| -> Option<&[u8]> {
        // Find the section that contains `va` and slice from it.
        for s in &pe.sections {
            let start = s.virtual_address;
            let end = start.saturating_add(s.virtual_size.max(s.size_of_raw_data));
            if va >= start && va < end {
                let off = (va - start) as usize + s.pointer_to_raw_data as usize;
                if off < bytes.len() {
                    return Some(&bytes[off..]);
                }
            }
        }
        None
    };
    let read_cstring = |va: u32| -> Option<String> {
        let slice = read_va(va)?;
        let len = slice.iter().position(|&b| b == 0)?;
        std::str::from_utf8(&slice[..len])
            .ok()
            .map(|s| s.to_string())
    };

    // The import directory RVA is relative to the image base.
    let dir_rva = dir.virtual_address;
    let mut idx = dir_rva;
    loop {
        let descriptor =
            read_va(idx).ok_or_else(|| LoadError::BadImports("descriptor oob".into()))?;
        if descriptor.len() < 20 {
            break;
        }
        let original_first_thunk = LittleEndian::read_u32(&descriptor[0..4]);
        let _timestamp = LittleEndian::read_u32(&descriptor[4..8]);
        let _forwarder = LittleEndian::read_u32(&descriptor[8..12]);
        let name_rva = LittleEndian::read_u32(&descriptor[12..16]);
        let first_thunk = LittleEndian::read_u32(&descriptor[16..20]);
        if original_first_thunk == 0 && name_rva == 0 && first_thunk == 0 {
            break;
        }
        let dll_name = normalize_import_dll(&read_cstring(name_rva).unwrap_or_default());
        // Walk the lookup table — prefer OriginalFirstThunk, fall back to
        // FirstThunk if the linker omitted the lookup table.
        let lookup_rva = if original_first_thunk != 0 {
            original_first_thunk
        } else {
            first_thunk
        };
        let mut t = lookup_rva;
        let mut iat = first_thunk;
        loop {
            let entry_slice = match read_va(t) {
                Some(s) if s.len() >= 4 => s,
                _ => break,
            };
            let entry = LittleEndian::read_u32(&entry_slice[..4]);
            if entry == 0 {
                break;
            }
            let binding = if entry & 0x8000_0000 != 0 {
                ImportBinding::Ordinal((entry & 0xFFFF) as u16)
            } else {
                let hint_name_va = entry & 0x7FFF_FFFF;
                // Skip the 2-byte hint, then a NUL-terminated ASCII name.
                let s = read_va(hint_name_va)
                    .ok_or_else(|| LoadError::BadImports("hint oob".into()))?;
                if s.len() < 3 {
                    return Err(LoadError::BadImports("short hint".into()));
                }
                let name_bytes = &s[2..];
                let len = name_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(name_bytes.len());
                let name = std::str::from_utf8(&name_bytes[..len])
                    .map_err(|_| LoadError::BadImports("non-utf8 import name".into()))?
                    .to_string();
                ImportBinding::Name(name)
            };
            out.push(ImportSymbol {
                dll: dll_name.clone(),
                binding,
                iat_va: img_base.wrapping_add(iat),
            });
            t += 4;
            iat += 4;
        }
        idx += 20;
    }
    Ok(out)
}

/// Group imports by DLL name (lower-cased) for nicer reporting.
pub fn imports_by_dll(image: &LoadedImage) -> BTreeMap<String, Vec<&ImportSymbol>> {
    let mut by_dll: BTreeMap<String, Vec<&ImportSymbol>> = BTreeMap::new();
    for imp in &image.imports {
        by_dll
            .entry(imp.dll.to_ascii_lowercase())
            .or_default()
            .push(imp);
    }
    by_dll
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CeGCC / mingw32ce writes the bare module name into the import
    /// directory. Every consumer downstream keys on the name *with* the
    /// `.dll` suffix, so the loader has to supply it.
    #[test]
    fn suffixless_import_names_gain_the_dll_extension() {
        assert_eq!(normalize_import_dll("COREDLL"), "COREDLL.dll");
        assert_eq!(normalize_import_dll("aygshell"), "aygshell.dll");
    }

    /// MSVC already writes the extension, and the comparisons downstream
    /// are case-blind, so the name has to survive untouched.
    #[test]
    fn names_that_already_have_an_extension_are_left_alone() {
        for name in [
            "COREDLL.dll",
            "coredll.dll",
            "libGLES_CM.dll",
            // Not every WinCE module is a `.dll`.
            "keybd.drv",
            "ctlpnl.cpl",
            // A versioned name keeps its dots too.
            "fmodce370.dll",
        ] {
            assert_eq!(normalize_import_dll(name), name);
        }
    }

    /// A malformed import descriptor leaves `read_cstring` returning
    /// `None`, which becomes an empty name. Appending `.dll` there would
    /// invent a module called `.dll`.
    #[test]
    fn an_empty_name_stays_empty() {
        assert_eq!(normalize_import_dll(""), "");
    }

    #[test]
    fn machine_names() {
        let img = LoadedImage {
            source_path: "x".into(),
            machine: machine::ARM,
            subsystem: subsystem::WINDOWS_CE_GUI,
            image_base: 0x10000,
            size_of_image: 0,
            entry_point: 0,
            sections: vec![],
            imports: vec![],
            exports: IndexMap::new(),
            resources: vec![],
            managed_runtime: None,
        };
        assert_eq!(img.machine_name(), "ARM (legacy)");
        assert_eq!(img.entry_va(), 0x10000);
    }
}
