//! ustar reader over gzip, mirroring the registry's TarInspector.

use super::{MAX_ARCHIVE, MAX_ENTRIES, MAX_METADATA, decode_name, invalid, safe_path};
use crate::error::{Error, Result};
use crate::names::Format;
use std::io::Read;

/// Single-member gzip only: the registry rejects trailing or concatenated gzip members.
fn gunzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = flate2::bufread::GzDecoder::new(bytes);
    let mut out = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut buffer).map_err(|_| invalid("invalid GZIP data"))?;
        if n == 0 {
            break;
        }
        if out.len() as u64 + n as u64 > MAX_ARCHIVE {
            return Err(invalid("expanded archive exceeds 64 MiB"));
        }
        out.extend_from_slice(&buffer[..n]);
    }
    if !decoder.into_inner().is_empty() {
        return Err(invalid("data after the end of the GZIP stream"));
    }
    Ok(out)
}

/// Cuts at the first NUL and decodes like TextDecoder (lossy, one BOM dropped), as the server does.
fn field(header: &[u8], start: usize, length: usize) -> Result<String> {
    let raw = &header[start..start + length];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Ok(decode_name(&raw[..end]))
}

/// ECMAScript String.prototype.trim() whitespace (the server trims numbers like JavaScript did):
/// unlike Rust's trim(), it includes U+FEFF and excludes U+0085.
fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| {
        matches!(
            c,
            '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
        )
    })
}

fn octal(header: &[u8], start: usize, length: usize) -> Result<u64> {
    let text = field(header, start, length)?;
    let value = js_trim(&text);
    if value.is_empty() || !value.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return Err(invalid("invalid TAR number"));
    }
    u64::from_str_radix(value, 8).map_err(|_| invalid("invalid TAR number"))
}

/// Returns the metadata entry's path and contents.
pub fn metadata(format: Format, bytes: &[u8]) -> Result<(String, Vec<u8>)> {
    let tar = gunzip(bytes)?;
    let mut offset = 0usize;
    let mut count = 0usize;
    let mut names = std::collections::HashSet::new();
    let mut selected: Option<(String, Vec<u8>)> = None;
    let mut ended = false;
    while offset < tar.len() {
        if ended {
            if tar[offset..].iter().any(|&b| b != 0) {
                return Err(invalid("data after TAR end"));
            }
            break;
        }
        if tar.len() - offset < 512 {
            return Err(invalid("incomplete TAR package"));
        }
        let header = &tar[offset..offset + 512];
        offset += 512;
        if header.iter().all(|&b| b == 0) {
            ended = true;
            continue;
        }
        count += 1;
        if count > MAX_ENTRIES {
            return Err(invalid("too many TAR entries"));
        }
        let checksum = octal(header, 148, 8)?;
        let sum: u64 = header.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { b as u64 }).sum();
        if sum != checksum {
            return Err(invalid("TAR checksum mismatch"));
        }
        let prefix = field(header, 345, 155)?;
        let base = field(header, 0, 100)?;
        let name = if prefix.is_empty() { base } else { format!("{prefix}/{base}") };
        let kind = field(header, 156, 1)?;
        let size = octal(header, 124, 12)?;
        let is_dir = kind == "5";
        if !safe_path(&name)
            || names.contains(&name)
            || !(kind.is_empty() || kind == "0" || kind == "5")
            || size > MAX_ARCHIVE
            || (is_dir && size != 0)
            || (!is_dir && name.ends_with('/'))
        {
            return Err(Error::validation(format!("unsafe or unsupported TAR entry \"{name}\""))
                .hint("only regular files and directories from a plain ustar archive are accepted; links, PAX and GNU extensions are rejected"));
        }
        names.insert(name.clone());
        let padded = size.div_ceil(512) * 512;
        if ((tar.len() - offset) as u64) < padded {
            return Err(invalid("incomplete TAR package"));
        }
        if format.is_metadata_path(&name) {
            if selected.is_some() {
                return Err(invalid("multiple package metadata files"));
            }
            if size > MAX_METADATA {
                return Err(Error::validation(format!("{} exceeds 256 KiB", format.metadata_description())));
            }
            selected = Some((name.clone(), tar[offset..offset + size as usize].to_vec()));
        }
        offset += padded as usize;
    }
    if !ended {
        return Err(invalid("incomplete TAR package"));
    }
    selected.ok_or_else(|| Error::validation(format!("archive does not contain {}", format.metadata_description())))
}
