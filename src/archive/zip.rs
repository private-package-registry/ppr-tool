//! ZIP reader mirroring zipMetadata() in registry/server/archives.ts.

use super::{MAX_ARCHIVE, MAX_ENTRIES, MAX_METADATA, invalid, safe_path};
use crate::error::{Error, Result};
use crate::names::Format;
use std::io::Read;

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

struct Entry {
    name: String,
    method: u16,
    compressed: u64,
    original: u64,
    crc: u32,
    local: u64,
}

/// Returns the metadata entry's path and contents.
pub fn metadata(format: Format, bytes: &[u8]) -> Result<(String, Vec<u8>)> {
    let size = bytes.len();
    let tail_offset = size.saturating_sub(65_557);
    let tail = &bytes[tail_offset..];
    let mut end = None;
    if tail.len() >= 22 {
        let mut i = tail.len() - 22;
        loop {
            if u32_at(tail, i) == 0x0605_4b50 && i + 22 + u16_at(tail, i + 20) as usize == tail.len() {
                end = Some(i);
                break;
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }
    }
    let Some(end) = end else { return Err(invalid("invalid ZIP directory")) };
    if u16_at(tail, end + 4) != 0 || u16_at(tail, end + 6) != 0 {
        return Err(invalid("multi-disk ZIP archives are not supported"));
    }
    let count = u16_at(tail, end + 10) as usize;
    let length = u32_at(tail, end + 12) as u64;
    let offset = u32_at(tail, end + 16) as u64;
    if count > MAX_ENTRIES || count != u16_at(tail, end + 8) as usize || length > 4 * 1024 * 1024 || offset + length != (tail_offset + end) as u64 {
        return Err(invalid("unsupported ZIP directory (zip64, trailing data or oversized central directory)"));
    }
    let central = &bytes[offset as usize..(offset + length) as usize];
    let mut entries = Vec::with_capacity(count);
    let mut seen = std::collections::HashSet::new();
    let mut expanded = 0u64;
    let mut position = 0usize;
    for _ in 0..count {
        if position + 46 > central.len() || u32_at(central, position) != 0x0201_4b50 {
            return Err(invalid("invalid ZIP entry"));
        }
        let flags = u16_at(central, position + 8);
        let method = u16_at(central, position + 10);
        let crc = u32_at(central, position + 16);
        let compressed = u32_at(central, position + 20) as u64;
        let original = u32_at(central, position + 24) as u64;
        let nl = u16_at(central, position + 28) as usize;
        let el = u16_at(central, position + 30) as usize;
        let cl = u16_at(central, position + 32) as usize;
        let attrs = u32_at(central, position + 38);
        let local = u32_at(central, position + 42) as u64;
        if position + 46 + nl + el + cl > central.len() {
            return Err(invalid("invalid ZIP entry size"));
        }
        let name = String::from_utf8_lossy(&central[position + 46..position + 46 + nl]).into_owned();
        expanded += original;
        let kind = (attrs >> 16) & 0xF000;
        if !safe_path(&name)
            || seen.contains(&name)
            || expanded > MAX_ARCHIVE
            || (flags & 1) != 0
            || !(method == 0 || method == 8)
            || !(kind == 0 || kind == 0x8000 || kind == 0x4000)
            || local + 30 + compressed > offset
        {
            return Err(Error::validation(format!("unsafe ZIP entry \"{name}\""))
                .hint("entries must use stored or deflate compression without encryption, safe relative paths and no zip64 extensions"));
        }
        seen.insert(name.clone());
        entries.push(Entry { name, method, compressed, original, crc, local });
        position += 46 + nl + el + cl;
    }
    if position != central.len() {
        return Err(invalid("invalid ZIP directory size"));
    }
    // The server streams local headers front to back (fflate Unzip), so walk them the same way:
    // every local entry must be listed in the central directory, once, with matching fields.
    let by_offset: std::collections::HashMap<u64, &Entry> = entries.iter().map(|e| (e.local, e)).collect();
    let mut selected: Option<(String, Vec<u8>)> = None;
    let mut total = 0u64;
    let mut visited = 0usize;
    let mut position = 0usize;
    let end_of_entries = offset as usize;
    while position < end_of_entries {
        if position + 30 > end_of_entries || u32_at(bytes, position) != 0x0403_4b50 {
            return Err(invalid("unexpected data between ZIP entries"));
        }
        let flags = u16_at(bytes, position + 6);
        let method = u16_at(bytes, position + 8);
        let crc = u32_at(bytes, position + 14);
        let compressed_size = u32_at(bytes, position + 18) as u64;
        let original_size = u32_at(bytes, position + 22) as u64;
        let name_length = u16_at(bytes, position + 26) as usize;
        let extra_length = u16_at(bytes, position + 28) as usize;
        if position + 30 + name_length > end_of_entries {
            return Err(invalid("truncated ZIP local header"));
        }
        let name = String::from_utf8_lossy(&bytes[position + 30..position + 30 + name_length]).into_owned();
        let Some(entry) = by_offset.get(&(position as u64)).filter(|e| e.name == name) else {
            return Err(invalid(&format!("unexpected ZIP entry \"{name}\" (missing from the central directory)")));
        };
        let described = flags & 8 != 0;
        if method != entry.method || (!described && (compressed_size != entry.compressed || original_size != entry.original || crc != entry.crc)) {
            return Err(invalid(&format!("ZIP local header of \"{name}\" does not match the central directory")));
        }
        let start = position + 30 + name_length + extra_length;
        let stop = start + entry.compressed as usize;
        if stop > end_of_entries {
            return Err(invalid("truncated ZIP entry"));
        }
        let data = inflate(entry, &bytes[start..stop])?;
        total += data.len() as u64;
        if total > MAX_ARCHIVE {
            return Err(invalid("expanded archive exceeds 64 MiB"));
        }
        if format.is_metadata_path(&entry.name) {
            if selected.is_some() {
                return Err(invalid("multiple package metadata files"));
            }
            if data.len() as u64 > MAX_METADATA {
                return Err(Error::validation(format!("{} exceeds 256 KiB", entry.name)));
            }
            selected = Some((entry.name.clone(), data));
        }
        position = stop;
        if described {
            // Data descriptor: optional signature, then CRC and 32-bit sizes.
            if position + 4 <= end_of_entries && u32_at(bytes, position) == 0x0807_4b50 {
                position += 4;
            }
            position += 12;
        }
        visited += 1;
    }
    if position != end_of_entries || visited != entries.len() {
        return Err(invalid("ZIP entries do not match the central directory"));
    }
    selected.ok_or_else(|| Error::validation(format!("archive does not contain {}", format.metadata_description())))
}

fn inflate(entry: &Entry, compressed: &[u8]) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let limit = entry.original + 1;
    if entry.method == 0 {
        if compressed.len() as u64 > limit {
            return Err(invalid("ZIP integrity mismatch"));
        }
        data.extend_from_slice(compressed);
    } else {
        let mut decoder = flate2::read::DeflateDecoder::new(compressed).take(limit);
        decoder.read_to_end(&mut data).map_err(|_| invalid("invalid or oversized ZIP contents"))?;
    }
    if data.len() as u64 != entry.original {
        return Err(invalid("ZIP integrity mismatch"));
    }
    let mut crc = flate2::Crc::new();
    crc.update(&data);
    if crc.sum() != entry.crc {
        return Err(invalid("ZIP integrity mismatch"));
    }
    Ok(data)
}
