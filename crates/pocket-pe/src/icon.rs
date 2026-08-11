//! Application icon extraction from a PE image.
//!
//! Windows Mobile executables carry their launcher icon in the
//! resource directory as an `RT_GROUP_ICON` directory plus one
//! `RT_ICON` per size/depth, exactly like desktop Win32. Launchers
//! want a single bitmap to show in a game list, so this module picks
//! the best-looking `RT_ICON` and decodes it to straight RGBA.
//!
//! Only the classic DIB icon encoding is handled (a
//! `BITMAPINFOHEADER`, an optional palette, the colour bits and a
//! 1bpp AND mask). PNG-compressed icons are also accepted and handed
//! back untouched so callers can decode them with a real PNG reader.
//!
//! Reference: <https://learn.microsoft.com/en-us/windows/win32/menurc/icon-resource>

use byteorder::{ByteOrder, LittleEndian};
use goblin::pe::{options::ParseOptions, PE};

use crate::resources::{collect_resources, ResourceKey};
use crate::LoadError;

/// `RT_ICON` — one icon image.
const RT_ICON: u32 = 3;
/// `RT_GROUP_ICON` — the directory that lists the images of one icon.
const RT_GROUP_ICON: u32 = 14;

/// A decoded icon, top-down, 8 bits per channel, non-premultiplied.
#[derive(Debug, Clone)]
pub struct IconImage {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, RGBA order.
    pub rgba: Vec<u8>,
}

/// Extract the largest, deepest icon from `pe` and decode it to RGBA.
///
/// Returns `None` when the image has no icon resource or the icon uses
/// an encoding we do not understand — callers are expected to fall
/// back to a placeholder rather than treat this as an error.
pub fn extract_icon(bytes: &[u8], pe: &PE) -> Option<IconImage> {
    let entries = collect_resources(bytes, pe).ok()?;

    // Prefer the icon the RT_GROUP_ICON directory points at: that is
    // the one Explorer would show, and it also tells us which of
    // several same-sized RT_ICONs is the colour version.
    let mut wanted: Option<u32> = None;
    let mut best_group_score = 0u64;
    for entry in entries
        .iter()
        .filter(|e| e.ty == ResourceKey::Id(RT_GROUP_ICON))
    {
        let data = resource_bytes(bytes, pe, entry.data_rva, entry.size)?;
        if data.len() < 6 {
            continue;
        }
        let count = LittleEndian::read_u16(&data[4..6]) as usize;
        for i in 0..count {
            let off = 6 + i * 14;
            if off + 14 > data.len() {
                break;
            }
            let w = normalise_dim(data[off]);
            let h = normalise_dim(data[off + 1]);
            let bit_count = LittleEndian::read_u16(&data[off + 6..off + 8]) as u64;
            let id = LittleEndian::read_u16(&data[off + 12..off + 14]) as u32;
            let score = icon_score(w, h, bit_count);
            if score > best_group_score {
                best_group_score = score;
                wanted = Some(id);
            }
        }
    }

    let icons: Vec<_> = entries
        .iter()
        .filter(|e| e.ty == ResourceKey::Id(RT_ICON))
        .collect();

    if let Some(id) = wanted {
        if let Some(entry) = icons.iter().find(|e| e.name == ResourceKey::Id(id)) {
            if let Some(image) = decode_entry(bytes, pe, entry.data_rva, entry.size) {
                return Some(image);
            }
        }
    }

    // No usable group directory (or the image it named failed to
    // decode): fall back to whichever RT_ICON decodes to the biggest
    // bitmap.
    let mut best: Option<IconImage> = None;
    for entry in icons {
        let Some(image) = decode_entry(bytes, pe, entry.data_rva, entry.size) else {
            continue;
        };
        let better = match &best {
            Some(current) => image.width * image.height > current.width * current.height,
            None => true,
        };
        if better {
            best = Some(image);
        }
    }
    best
}

/// `0` in a group-icon entry means 256 pixels.
fn normalise_dim(raw: u8) -> u64 {
    if raw == 0 {
        256
    } else {
        raw as u64
    }
}

/// Rank icon variants: area first, colour depth as the tie-break.
fn icon_score(width: u64, height: u64, bit_count: u64) -> u64 {
    width * height * 256 + bit_count.min(255)
}

/// Copy `size` bytes of resource data out of the on-disk image.
fn resource_bytes<'a>(bytes: &'a [u8], pe: &PE, rva: u32, size: u32) -> Option<&'a [u8]> {
    let offset = rva_to_file_offset(pe, rva)?;
    let end = offset.checked_add(size as usize)?;
    bytes.get(offset..end)
}

/// Translate an image-relative virtual address to a file offset.
fn rva_to_file_offset(pe: &PE, rva: u32) -> Option<usize> {
    for section in &pe.sections {
        let start = section.virtual_address;
        let virtual_size = section.virtual_size.max(section.size_of_raw_data);
        let end = start.saturating_add(virtual_size);
        if rva >= start && rva < end {
            let delta = rva - start;
            if delta >= section.size_of_raw_data {
                return None;
            }
            return Some(section.pointer_to_raw_data as usize + delta as usize);
        }
    }
    None
}

fn decode_entry(bytes: &[u8], pe: &PE, rva: u32, size: u32) -> Option<IconImage> {
    let data = resource_bytes(bytes, pe, rva, size)?;
    decode_icon_image(data)
}

/// Decode one `RT_ICON` payload.
pub fn decode_icon_image(data: &[u8]) -> Option<IconImage> {
    if data.len() >= 8 && data[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        // Vista-style PNG icon. We have no PNG decoder here, so leave
        // it to the caller; signalling "unsupported" is more honest
        // than returning a wrong bitmap.
        return None;
    }
    decode_dib_icon(data)
}

/// Decode a `BITMAPINFOHEADER`-based icon image (colour bits followed
/// by a 1bpp AND mask) into RGBA.
fn decode_dib_icon(data: &[u8]) -> Option<IconImage> {
    if data.len() < 40 {
        return None;
    }
    let header_size = LittleEndian::read_u32(&data[0..4]) as usize;
    if header_size < 40 || header_size > data.len() {
        return None;
    }
    let width = LittleEndian::read_i32(&data[4..8]);
    // An icon DIB stores the colour bits and the mask stacked, so the
    // declared height is twice the real one.
    let stored_height = LittleEndian::read_i32(&data[8..12]);
    let bit_count = LittleEndian::read_u16(&data[14..16]) as u32;
    let compression = LittleEndian::read_u32(&data[16..20]);
    let clr_used = LittleEndian::read_u32(&data[32..36]);

    // BI_RGB only: icons are never RLE-compressed in practice and we
    // would rather show a placeholder than a garbled bitmap.
    if compression != 0 {
        return None;
    }
    if width <= 0 || stored_height == 0 {
        return None;
    }
    let width = width as u32;
    let bottom_up = stored_height > 0;
    let height = if bottom_up {
        (stored_height / 2) as u32
    } else {
        (-stored_height / 2) as u32
    };
    if height == 0 || width > 512 || height > 512 {
        return None;
    }

    let palette_len = match bit_count {
        1 | 4 | 8 => {
            let entries = if clr_used == 0 {
                1usize << bit_count
            } else {
                clr_used as usize
            };
            entries * 4
        }
        _ => 0,
    };
    let palette_start = header_size;
    let palette_end = palette_start.checked_add(palette_len)?;
    let palette = data.get(palette_start..palette_end)?;

    let colour_stride = row_stride(width, bit_count)?;
    let colour_len = colour_stride.checked_mul(height as usize)?;
    let colour_start = palette_end;
    let colour_end = colour_start.checked_add(colour_len)?;
    let colour = data.get(colour_start..colour_end)?;

    // The AND mask is optional in malformed images; treat a missing or
    // short mask as "fully opaque" rather than bailing out.
    let mask_stride = row_stride(width, 1)?;
    let mask = data
        .get(colour_end..colour_end.checked_add(mask_stride * height as usize)?)
        .unwrap_or(&[]);

    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
    let mut any_alpha = false;
    for y in 0..height as usize {
        // Bottom-up rows are stored last-first.
        let src_y = if bottom_up {
            height as usize - 1 - y
        } else {
            y
        };
        let row = &colour[src_y * colour_stride..(src_y + 1) * colour_stride];
        let mask_row = if mask.is_empty() {
            None
        } else {
            Some(&mask[src_y * mask_stride..(src_y + 1) * mask_stride])
        };
        for x in 0..width as usize {
            let (r, g, b, a) = match bit_count {
                1 | 4 | 8 => {
                    let index = palette_index(row, x, bit_count)? as usize;
                    let off = index * 4;
                    let entry = palette.get(off..off + 4)?;
                    // RGBQUAD is stored BGRA.
                    (entry[2], entry[1], entry[0], 255)
                }
                16 => {
                    let v = LittleEndian::read_u16(row.get(x * 2..x * 2 + 2)?);
                    let r = ((v >> 10) & 0x1f) as u8;
                    let g = ((v >> 5) & 0x1f) as u8;
                    let b = (v & 0x1f) as u8;
                    (r << 3 | r >> 2, g << 3 | g >> 2, b << 3 | b >> 2, 255)
                }
                24 => {
                    let px = row.get(x * 3..x * 3 + 3)?;
                    (px[2], px[1], px[0], 255)
                }
                32 => {
                    let px = row.get(x * 4..x * 4 + 4)?;
                    (px[2], px[1], px[0], px[3])
                }
                _ => return None,
            };
            if bit_count == 32 && a != 0 {
                any_alpha = true;
            }
            let dst = (y * width as usize + x) * 4;
            rgba[dst] = r;
            rgba[dst + 1] = g;
            rgba[dst + 2] = b;
            rgba[dst + 3] = a;
            // A set mask bit means "transparent here".
            if let Some(mask_row) = mask_row {
                let byte = mask_row.get(x / 8).copied().unwrap_or(0);
                if byte & (0x80 >> (x % 8)) != 0 {
                    rgba[dst + 3] = 0;
                }
            }
        }
    }

    // A 32bpp icon whose alpha channel is entirely zero predates
    // alpha-aware icons; the AND mask is the real transparency, so
    // make the colour bits opaque again where the mask kept them.
    if bit_count == 32 && !any_alpha {
        for y in 0..height as usize {
            let src_y = if bottom_up {
                height as usize - 1 - y
            } else {
                y
            };
            for x in 0..width as usize {
                let dst = (y * width as usize + x) * 4;
                let transparent = if mask.is_empty() {
                    false
                } else {
                    let mask_row = &mask[src_y * mask_stride..(src_y + 1) * mask_stride];
                    let byte = mask_row.get(x / 8).copied().unwrap_or(0);
                    byte & (0x80 >> (x % 8)) != 0
                };
                rgba[dst + 3] = if transparent { 0 } else { 255 };
            }
        }
    }

    Some(IconImage {
        width,
        height,
        rgba,
    })
}

/// DIB rows are padded to a 4-byte boundary.
fn row_stride(width: u32, bit_count: u32) -> Option<usize> {
    let bits = (width as usize).checked_mul(bit_count as usize)?;
    Some(bits.div_ceil(32) * 4)
}

fn palette_index(row: &[u8], x: usize, bit_count: u32) -> Option<u8> {
    match bit_count {
        1 => {
            let byte = row.get(x / 8).copied()?;
            Some((byte >> (7 - (x % 8))) & 1)
        }
        4 => {
            let byte = row.get(x / 2).copied()?;
            Some(if x.is_multiple_of(2) {
                byte >> 4
            } else {
                byte & 0x0f
            })
        }
        8 => row.get(x).copied(),
        _ => None,
    }
}

/// Convenience wrapper used by launchers: parse `bytes` as a PE and
/// return its icon.
pub fn icon_from_pe_bytes(bytes: &[u8]) -> Result<Option<IconImage>, LoadError> {
    let mut options = ParseOptions::default();
    options.parse_attribute_certificates = false;
    let pe = PE::parse_with_opts(bytes, &options).map_err(|e| LoadError::NotPe(e.to_string()))?;
    Ok(extract_icon(bytes, &pe))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny 2x2 8bpp icon: two palette entries, the top-left
    /// pixel masked out.
    fn sample_icon() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&40u32.to_le_bytes()); // biSize
        out.extend_from_slice(&2i32.to_le_bytes()); // biWidth
        out.extend_from_slice(&4i32.to_le_bytes()); // biHeight (2x)
        out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        out.extend_from_slice(&8u16.to_le_bytes()); // biBitCount
        out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        out.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
        out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
        out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
        out.extend_from_slice(&2u32.to_le_bytes()); // biClrUsed
        out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
        out.extend_from_slice(&[0x00, 0x00, 0xff, 0x00]); // palette[0] = red
        out.extend_from_slice(&[0xff, 0x00, 0x00, 0x00]); // palette[1] = blue
                                                          // Colour rows, bottom-up, padded to 4 bytes.
        out.extend_from_slice(&[1, 1, 0, 0]); // bottom row: blue, blue
        out.extend_from_slice(&[0, 1, 0, 0]); // top row: red, blue
                                              // AND mask rows, bottom-up.
        out.extend_from_slice(&[0b0000_0000, 0, 0, 0]);
        out.extend_from_slice(&[0b1000_0000, 0, 0, 0]);
        out
    }

    #[test]
    fn decodes_8bpp_icon_with_mask() {
        let icon = decode_icon_image(&sample_icon()).expect("decodes");
        assert_eq!((icon.width, icon.height), (2, 2));
        // Top-left is red but masked out.
        assert_eq!(&icon.rgba[0..4], &[255, 0, 0, 0]);
        // Top-right is blue and opaque.
        assert_eq!(&icon.rgba[4..8], &[0, 0, 255, 255]);
        // Bottom row is blue and opaque.
        assert_eq!(&icon.rgba[8..12], &[0, 0, 255, 255]);
    }

    #[test]
    fn rejects_png_icons() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        assert!(decode_icon_image(&png).is_none());
    }

    #[test]
    fn scores_bigger_icons_first() {
        assert!(icon_score(32, 32, 8) > icon_score(16, 16, 32));
        assert!(icon_score(32, 32, 32) > icon_score(32, 32, 8));
    }
}
