use std::path::Path;

use anyhow::{bail, Context, Result};
use pocket_kernel::DeviceProfile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomProfile {
    pub model: String,
    pub manufacturer: String,
    pub processor: String,
    pub sku: String,
    pub screen_width: u32,
    pub screen_height: u32,
    pub bits_per_pixel: u32,
    pub device_profile: DeviceProfile,
}

impl RomProfile {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes =
            std::fs::read(path).with_context(|| format!("reading ROM image {}", path.display()))?;
        Self::from_bytes(&bytes).with_context(|| format!("parsing ROM image {}", path.display()))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 0x1000 {
            bail!("ROM image is too small ({} bytes)", bytes.len());
        }
        if !bytes.windows(4).any(|window| window == b"ECEC") {
            bail!("ROM image has no Windows CE XIP marker");
        }

        let model = find_ascii(bytes, b"HP iPAQ ", 96)
            .unwrap_or_else(|| "Windows Mobile device".to_string());
        let manufacturer = find_ascii(bytes, b"Hewlett-Packard", 64)
            .unwrap_or_else(|| "Unknown manufacturer".to_string());
        let processor = find_ascii(bytes, b"Marvell ", 64)
            .unwrap_or_else(|| "ARM Windows CE device".to_string());
        let sku = find_ascii(bytes, b"FB040AA", 32).unwrap_or_default();
        let is_ipaq_210 = model.to_ascii_lowercase().contains("ipaq 210")
            || bytes.windows(9).any(|window| window == b"FB040AA#");
        let (screen_width, screen_height) = if is_ipaq_210 { (640, 480) } else { (240, 320) };
        let os = (5, 2, 1616);
        let device_profile = DeviceProfile {
            model: model.clone(),
            manufacturer: manufacturer.clone(),
            processor: processor.clone(),
            screen_width,
            screen_height,
            bits_per_pixel: 16,
            ram_bytes: 128 * 1024 * 1024,
            storage_bytes: 256 * 1024 * 1024,
            wince_major: os.0,
            wince_minor: os.1,
            wince_build: os.2,
            wince_platform: 3,
        };
        Ok(Self {
            model,
            manufacturer,
            processor,
            sku,
            screen_width,
            screen_height,
            bits_per_pixel: 16,
            device_profile,
        })
    }
}

fn find_ascii(bytes: &[u8], prefix: &[u8], max_len: usize) -> Option<String> {
    let start = bytes
        .windows(prefix.len())
        .position(|window| window == prefix)?;
    let tail = &bytes[start..bytes.len().min(start + max_len)];
    let end = tail
        .iter()
        .position(|&byte| byte == 0 || !(byte.is_ascii_graphic() || byte == b' '))
        .unwrap_or(tail.len());
    let value = String::from_utf8_lossy(&tail[..end]).trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_hp_ipaq_210_dump_signature() {
        let mut image = vec![0u8; 0x2000];
        image[0x100..0x104].copy_from_slice(b"ECEC");
        let manufacturer = b"Hewlett-Packard POCKET PC HP iPAQ 210";
        let processor = b"Marvell MHLV";
        let sku = b"FB040AA#";
        let os = b"Windows CE 502";
        image[0x300..0x300 + manufacturer.len()].copy_from_slice(manufacturer);
        image[0x400..0x400 + processor.len()].copy_from_slice(processor);
        image[0x500..0x500 + sku.len()].copy_from_slice(sku);
        image[0x600..0x600 + os.len()].copy_from_slice(os);
        let profile = RomProfile::from_bytes(&image).unwrap();
        assert_eq!(profile.model, "HP iPAQ 210");
        assert_eq!(profile.screen_width, 640);
        assert_eq!(profile.screen_height, 480);
        assert_eq!(profile.device_profile.wince_major, 5);
    }

    #[test]
    fn rejects_non_ce_images() {
        let error = RomProfile::from_bytes(&vec![0u8; 0x2000]).unwrap_err();
        assert!(error.to_string().contains("XIP marker"));
    }
}
